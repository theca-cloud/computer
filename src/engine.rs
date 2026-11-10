//! The engine: per account, one task following the event feed and running
//! rounds (scan, plan, execute) for each sync pair when something changed:
//! the feed, the watcher, a retry, the hourly full scan.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use theca_client::{Cancel, Client, Error, FeedMessage};
use theca_common::api;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::Instrument;
use uuid::Uuid;

use crate::exec::{Executor, remote_state};
use crate::keychain::{self, KeychainError};
use crate::local;
use crate::ops;
use crate::plan::{Action, Input, plan};
use crate::state::{Account, Offer, Pair, RemoteState, State, StateError, Stop, Update};

/// A full scan even without any sign of change: watchers drop events.
const FULL_SCAN: Duration = Duration::from_secs(3600);
/// A round asking for another (a file still being written, the network).
const RETRY: Duration = Duration::from_secs(5);
/// How long a node changed by this client is checked against the server
/// before a remote state about it is believed (it may predate the change).
const RECENT: Duration = Duration::from_secs(60);
/// Local deletions wait until the folder has been this quiet: `rm -rf`
/// deletes the files before their folder, and a round in between would trash
/// them one by one (or at all, when the folder is the pair's root).
const QUIET: Duration = Duration::from_secs(2);
/// ... but no longer than this: a folder never quiet (a log written every
/// second) would hold them forever.
const QUIET_AT_MOST: Duration = Duration::from_secs(30);
/// How long a stop waits for the accounts' rounds to end: their transfers
/// are cancelled at once; a service manager kills after about 10 s.
const STOP: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Client(#[from] Error),
    #[error(transparent)]
    Keychain(#[from] KeychainError),
    #[error("{0}")]
    Server(String),
}

/// What the GUI and the CLI ask the running engine.
#[derive(Clone)]
pub struct Handle {
    /// Something changed in the state (an account or a pair added, resumed,
    /// settings changed): every account looks again. A watch, not a
    /// `Notify`: each task has its own receiver, so one wake-up reaches all.
    woken: watch::Sender<()>,
    /// What the engine changes, as it writes it (the GUI follows them).
    pub updates: broadcast::Sender<Update>,
    /// Per account, the libraries last offered (a window opening shows them
    /// without waiting for the next `Update::Offered`).
    pub offered: Arc<std::sync::Mutex<HashMap<i64, Vec<Offer>>>>,
}

impl Handle {
    /// Every account task looks at the state again.
    pub fn wake(&self) {
        self.woken.send_replace(());
    }
}

impl Default for Handle {
    fn default() -> Self {
        Self {
            woken: watch::channel(()).0,
            // A window that falls this far behind reloads the whole status.
            updates: broadcast::channel(256).0,
            offered: Arc::default(),
        }
    }
}

/// Runs every account until `shutdown`, picking up accounts added meanwhile:
/// one task per account.
pub async fn run(
    state_path: PathBuf,
    handle: Handle,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), EngineError> {
    let mut running: HashMap<i64, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut woken = handle.woken.subscribe();
    loop {
        let accounts = State::open(&state_path)?.accounts()?;
        running.retain(|id, task| {
            let keep = accounts.iter().any(|a| a.id == *id && !a.signed_out) && !task.is_finished();
            if !keep {
                task.abort();
            }
            keep
        });
        for account in accounts.into_iter().filter(|a| !a.signed_out) {
            if running.contains_key(&account.id) {
                continue;
            }
            let path = state_path.clone();
            let handle = handle.clone();
            let shutdown = shutdown.clone();
            running.insert(
                account.id,
                tokio::spawn(
                    async move {
                        if let Err(e) = follow(&path, account.id, handle, shutdown).await {
                            tracing::error!(
                                account = account.id,
                                "the account's engine stopped: {e}"
                            );
                            let _ = stopped(&path, account.id, &e);
                        }
                    }
                    // The caller's span on its lines too (the tests name each device).
                    .in_current_span(),
                ),
            );
        }
        tokio::select! {
            _ = woken.changed() => {}
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            _ = shutdown.changed() => break,
        }
    }
    // Each account's round ends at once (its transfers cancelled, what
    // finished already recorded); one that doesn't in time is aborted.
    let deadline = tokio::time::Instant::now() + STOP;
    for (_, mut task) in running {
        if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
            task.abort();
            let _ = task.await;
        }
    }
    Ok(())
}

/// The account's engine stopped on an error (started again within 30 s):
/// said, so that pairs don't look up to date while nothing syncs (an error
/// coming back every time would otherwise never show).
fn stopped(state_path: &Path, account: i64, e: &EngineError) -> Result<(), StateError> {
    let state = State::open(state_path)?;
    let message = format!("syncing stopped, tried again every 30 s: {e}");
    state.notify(Some(account), None, "error", None, &message, None)?;
    for p in state.pairs()?.iter().filter(|p| p.account == account) {
        state.set_error(p.id, &e.to_string())?;
    }
    Ok(())
}

/// The account's app password was refused, or is gone from the keychain:
/// nothing runs until a new login.
fn sign_out(state: &State, account: &Account, why: &str) -> Result<(), StateError> {
    state.set_signed_out(account.id)?;
    state.notify(
        Some(account.id),
        None,
        "error",
        None,
        &format!(
            "This device was signed out of {} ({why}): sign in again, or remove the account",
            account.instance
        ),
        None,
    )?;
    for p in state.pairs()?.iter().filter(|p| p.account == account.id) {
        state.set_activity(p.id, "signed out", false)?;
    }
    Ok(())
}

/// One account's engine.
struct Follower {
    state: State,
    account: Account,
    client: Client,
    /// The last position the feed announced, per library.
    positions: HashMap<Uuid, api::Position>,
    /// The last `libraries` message.
    libraries: Vec<api::FeedLibrary>,
    /// The positions the last `libraries` message announced: a pair runs no
    /// round before its position reaches it (the feed's catch-up), or it would
    /// act on a past it hasn't seen (deleting what changed meanwhile).
    announced: HashMap<Uuid, i64>,
    /// Pairs to run a round for.
    dirty: HashSet<i64>,
    retry_at: Option<Instant>,
    full_at: HashMap<i64, Instant>,
    /// The last watcher event, per pair: stamped by the watcher itself, so a
    /// round deciding whether to trash sees the events that came during it
    /// (an `rm -rf` started mid-scan), not only those before it.
    heard_at: Heard,
    /// The ignored folders each pair's last scan skipped: what the watcher
    /// hears below them starts no round.
    skipped: Skipped,
    /// Pairs whose deletions wait for their folder to be quiet, since when.
    deletions_held: HashMap<i64, Instant>,
    /// Nodes this client changed lately, per pair.
    recent: HashMap<i64, HashMap<Uuid, Instant>>,
    /// Each running pair's folder held open (its rename is followed) and its
    /// watcher.
    roots: HashMap<i64, std::fs::File>,
    /// `None`: the folder can't be watched (the hourly scan only).
    /// Keyed by the folder too: a pair pointed elsewhere (Locate, from this
    /// process or another) is watched, held and synced there anew.
    watchers: HashMap<i64, (PathBuf, Option<RecommendedWatcher>)>,
    events: mpsc::UnboundedSender<i64>,
    offered: Arc<std::sync::Mutex<HashMap<i64, Vec<Offer>>>>,
    /// Pairs stopped for their folder, and what was at its path when last
    /// checked: checked again once that changes (a remount's check scans).
    stopped_at: HashMap<i64, Option<String>>,
    /// Set when the process stops: no round starts, a running one ends.
    stopping: watch::Receiver<bool>,
    /// Cancels the round's transfers when it stops.
    cancel: Cancel,
}

async fn follow(
    state_path: &Path,
    account: i64,
    handle: Handle,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), EngineError> {
    let state = State::open(state_path)?.with_updates(handle.updates.clone());
    let Some(account) = state.account(account)? else {
        return Ok(());
    };
    // Gone from the keychain: signed out. The keychain locked, or not up yet:
    // an error, tried again within 30 s.
    let password = match keychain::password(&account) {
        Err(KeychainError::Missing) => {
            sign_out(
                &state,
                &account,
                "its app password is missing from the keychain",
            )?;
            return Ok(());
        }
        password => password?,
    };
    let client = Client::new(&account.instance)?.with_token(password);
    // A server older than this client works with (or rolled back since the
    // login): stopped, saying to update it. Unreachable: the feed retries.
    if let Ok(version) = client.check_version().await
        && let Some(why) = ops::too_old(&version)
    {
        return Err(EngineError::Server(why));
    }
    let (events, mut changed) = mpsc::unbounded_channel();
    let mut f = Follower {
        state,
        account,
        client,
        positions: HashMap::new(),
        libraries: Vec::new(),
        announced: HashMap::new(),
        dirty: HashSet::new(),
        retry_at: None,
        full_at: HashMap::new(),
        heard_at: Heard::default(),
        skipped: Skipped::default(),
        deletions_held: HashMap::new(),
        recent: HashMap::new(),
        roots: HashMap::new(),
        watchers: HashMap::new(),
        events,
        offered: handle.offered.clone(),
        stopped_at: HashMap::new(),
        stopping: shutdown.clone(),
        cancel: Cancel::default(),
    };
    // The stop cancels the transfers in flight (aborted with this task).
    let mut cancelling = tokio::task::JoinSet::new();
    {
        let (mut stopping, cancel) = (shutdown.clone(), f.cancel.clone());
        cancelling.spawn(async move {
            if stopping.wait_for(|stop| *stop).await.is_ok() {
                cancel.cancel();
            }
        });
    }
    let after: BTreeMap<Uuid, api::Position> = f
        .pairs()?
        .iter()
        .filter_map(|p| {
            p.position.map(|seq| {
                let event = p.position_event;
                (p.library, api::Position::Event { seq, event })
            })
        })
        .collect();
    let mut feed = Box::pin(f.client.events(after));
    let mut woken = handle.woken.subscribe();
    loop {
        f.watch()?;
        tokio::select! {
            message = feed.next() => match message {
                Some(Ok(FeedMessage::Libraries(libraries))) => f.libraries(libraries).await?,
                Some(Ok(FeedMessage::Changes(changes))) => f.changes(changes).await?,
                Some(Err(e)) => {
                    if matches!(e.code(), Some("unauthenticated")) {
                        sign_out(&f.state, &f.account, "the server refused its app password")?;
                    } else {
                        f.state.notify(Some(f.account.id), None, "error", None, &e.to_string(), None)?;
                    }
                    return Ok(());
                }
                None => return Ok(()),
            },
            Some(pair) = changed.recv() => f.heard(pair),
            _ = woken.changed() => {
                // Settings may have changed (the limit, the default folder):
                // libraries offered are looked at again; every pair runs.
                f.account = f.state.account(f.account.id)?.unwrap_or(f.account);
                f.auto_sync().await?;
                for p in f.pairs()? {
                    f.dirty.insert(p.id);
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = shutdown.changed() => return Ok(()),
        }
        while let Ok(pair) = changed.try_recv() {
            f.heard(pair);
        }
        f.rounds().await?;
    }
}

impl Follower {
    fn heard(&mut self, pair: i64) {
        self.dirty.insert(pair);
    }

    /// Whether the pair's local deletions may go to the trash now.
    fn may_trash(&self, pair: i64) -> bool {
        self.heard_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&pair)
            .is_none_or(|at| at.elapsed() >= QUIET)
            || self
                .deletions_held
                .get(&pair)
                .is_some_and(|since| since.elapsed() >= QUIET_AT_MOST)
    }

    /// A pair's library is there (again): a pair its absence stopped runs,
    /// and its name and access are kept.
    fn library_there(&mut self, p: &Pair, name: &str, writable: bool) -> Result<(), StateError> {
        let for_its_library = matches!(p.stop, Some(Stop::AccessRemoved | Stop::LibraryDeleted));
        if for_its_library {
            self.state.set_stopped(p.id, None)?;
        }
        // Access given back: the stream doesn't replay what happened
        // meanwhile (a library gained follows from its last event), so the
        // pair would wait forever to catch up: it lists the library again. A
        // trashed library stayed in the feed, but a folder that arrived while
        // it was there couldn't be listed (its tree answers 404 in the trash):
        // listed again too.
        if for_its_library {
            self.state.reset_position(p.id)?;
        }
        self.state.set_library(p.id, name, writable)?;
        self.dirty.insert(p.id);
        Ok(())
    }

    fn pairs(&self) -> Result<Vec<Pair>, StateError> {
        Ok(self
            .state
            .pairs()?
            .into_iter()
            .filter(|p| p.account == self.account.id)
            .collect())
    }

    /// A watcher and an open handle for each running pair; none for the others.
    fn watch(&mut self) -> Result<(), EngineError> {
        let pairs = self.pairs()?;
        let running: HashSet<i64> = pairs
            .iter()
            .filter(|p| !p.paused && p.stop.is_none())
            .map(|p| p.id)
            .collect();
        let paths: HashMap<i64, &PathBuf> = pairs.iter().map(|p| (p.id, &p.path)).collect();
        let moved: Vec<i64> = self
            .watchers
            .iter()
            .filter(|(id, (path, _))| paths.get(id).is_some_and(|now| *now != path))
            .map(|(id, _)| *id)
            .collect();
        for id in moved {
            self.watchers.remove(&id);
            self.roots.remove(&id);
        }
        self.watchers.retain(|p, _| running.contains(p));
        self.roots.retain(|p, _| running.contains(p));
        for p in pairs.iter().filter(|p| running.contains(&p.id)) {
            if !self.roots.contains_key(&p.id)
                && let Ok(file) = open_dir(&p.path)
            {
                self.roots.insert(p.id, file);
            }
            if !self.watchers.contains_key(&p.id) {
                let events = self.events.clone();
                let heard_at = self.heard_at.clone();
                let skipped = self.skipped.clone();
                let id = p.id;
                // Events only mark the pair dirty: the next round (within a
                // second) scans, and files still being written wait for it.
                // No debouncer: notify-debouncer-full drops a file created
                // then deleted, which FSEvents reports in one event. Reads
                // aren't changes (a file closed after writing is): on Linux,
                // a round's own scan opens every folder (inotify's OPEN), and
                // would wake the pair again.
                let watcher =
                    notify::recommended_watcher(move |e: notify::Result<notify::Event>| {
                        let relevant = match &e {
                            Err(_) => true,
                            Ok(e) => changes(&e.kind) && !below_skipped(&skipped, id, &e.paths),
                        };
                        if relevant {
                            heard(&heard_at, id);
                            let _ = events.send(id);
                        }
                    });
                let watched = watcher.and_then(|mut w| {
                    // The real path: FSEvents reports events under it, and
                    // a folder reached through a symlink (macOS's /var is
                    // /private/var) would otherwise hear nothing.
                    let real = p.path.canonicalize().unwrap_or_else(|_| p.path.clone());
                    w.watch(&real, RecursiveMode::Recursive)?;
                    Ok(w)
                });
                if watched.is_err() {
                    self.state.notify(Some(p.account), Some(p.id), "info", None,
                        "changes are noticed by a full scan every hour (the folder can't be watched)", None)?;
                }
                self.watchers.insert(p.id, (p.path.clone(), watched.ok()));
                self.dirty.insert(p.id);
            }
        }
        Ok(())
    }

    async fn libraries(&mut self, libraries: Vec<api::FeedLibrary>) -> Result<(), EngineError> {
        for l in &libraries {
            let (seq, event) = (l.position, l.event);
            self.positions
                .insert(l.id, api::Position::Event { seq, event });
            self.announced.insert(l.id, l.position);
        }
        for p in self.pairs()? {
            match libraries.iter().find(|l| l.id == p.library) {
                None => self.stop(&p, Stop::AccessRemoved)?,
                Some(l) if l.trashed => self.stop(&p, Stop::LibraryDeleted)?,
                Some(l) => {
                    self.library_there(&p, &l.name, l.writable)?;
                    // A server restored from a backup: its tree is older than
                    // the base, so the resync has no base (decisions.md, Sync).
                    if l.reset {
                        self.state.forget_base(p.id)?;
                    }
                }
            }
        }
        self.libraries = libraries;
        self.auto_sync().await?;
        Ok(())
    }

    fn stop(&mut self, p: &Pair, stop: Stop) -> Result<(), StateError> {
        // Checked again for a resume once what is at its path changes.
        self.stopped_at.insert(p.id, local::root_id(&p.path));
        if p.stop.as_ref() != Some(&stop) {
            let why = stop.reason();
            // Headless, the log is where it shows.
            tracing::error!(pair = p.id, path = %p.path.display(), "stopped syncing: {why}");
            self.state.set_stopped(p.id, Some(&stop))?;
            self.state
                .set_activity(p.id, &format!("stopped: {why}"), false)?;
            self.state.notify(
                Some(p.account),
                Some(p.id),
                "error",
                Some(&p.path.to_string_lossy()),
                &format!("stopped: {why} (the local files stay)"),
                None,
            )?;
        }
        Ok(())
    }

    /// The libraries the account sees and doesn't sync: those small enough
    /// (and never declined) are synced into the default folder; the others
    /// are offered (`Update::Offered`, the GUI; `libraries`, the CLI).
    async fn auto_sync(&mut self) -> Result<(), EngineError> {
        // The account as it is now: its limit and folder are settings.
        if let Some(account) = self.state.account(self.account.id)? {
            self.account = account;
        }
        let limit = self.account.auto_sync_limit;
        let pairs = self.pairs()?;
        let declined = self.state.declined(self.account.id)?;
        let candidates: Vec<api::FeedLibrary> = self
            .libraries
            .iter()
            .filter(|l| !l.trashed && !pairs.iter().any(|p| p.library == l.id))
            .cloned()
            .collect();
        let others: Vec<(Uuid, String)> = self
            .libraries
            .iter()
            .map(|o| (o.id, o.name.clone()))
            .collect();
        // Every account's folders: two accounts may share a default folder.
        let mut taken: Vec<PathBuf> = self.state.pairs()?.iter().map(|p| p.path.clone()).collect();
        let mut offered = Vec::new();
        for l in candidates {
            let Ok(info) = self.client.library(l.id).await else {
                continue;
            };
            let path = ops::default_path(
                &self.client,
                &self.account.default_folder,
                (l.id, &l.name, l.organization),
                &others,
                &taken,
            )
            .await;
            let small = limit.is_some_and(|limit| info.size <= limit);
            // A folder already there with content: offered, never merged unasked.
            let occupied = std::fs::read_dir(&path).is_ok_and(|mut d| d.next().is_some());
            let synced = small
                && !declined.contains(&l.id)
                && !occupied
                && ops::check_path(&self.state, &path, None).is_ok()
                && ops::has_room(&path, info.size)
                // Created here, last: the pair's folder must exist before its
                // first round (it would stop as missing otherwise).
                && std::fs::create_dir_all(&path).is_ok();
            if !synced {
                offered.push(Offer {
                    library: l.id,
                    name: l.name.clone(),
                    size: info.size,
                });
                continue;
            }
            let id = self.state.add_pair(&Pair {
                id: 0,
                account: self.account.id,
                library: l.id,
                root: l.root,
                library_name: l.name.clone(),
                path: path.clone(),
                root_file_id: None,
                position: None,
                position_event: None,
                paused: false,
                stop: None,
                writable: l.writable,
                activity: String::new(),
                error: None,
                synced_at: None,
            })?;
            self.state.notify(
                Some(self.account.id),
                Some(id),
                "info",
                Some(&path.to_string_lossy()),
                &format!("{} is now synced here", l.name),
                None,
            )?;
            self.state
                .set_pair_path(id, &path, local::root_id(&path).as_deref())?;
            taken.push(path);
            self.dirty.insert(id);
        }
        self.offered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(self.account.id, offered.clone());
        self.state.publish(Update::Offered {
            account: self.account.id,
            libraries: offered,
        });
        Ok(())
    }

    async fn changes(&mut self, changes: api::Changes) -> Result<(), EngineError> {
        let pairs = self.pairs()?;
        // States per pair.
        let mut states: HashMap<i64, Vec<(Uuid, Option<RemoteState>)>> = HashMap::new();
        for change in changes.entries {
            match change.kind {
                api::ResourceKind::Library => {
                    let Some(p) = pairs.iter().find(|p| p.library == change.id) else {
                        continue;
                    };
                    match &change.state {
                        None => self.stop(p, Stop::AccessRemoved)?,
                        Some(s) if s.trashed => self.stop(p, Stop::LibraryDeleted)?,
                        Some(s) => self.library_there(p, &s.name, p.writable)?,
                    }
                }
                api::ResourceKind::Node => {
                    let mut targets: Vec<i64> = match &change.state {
                        Some(s) => pairs
                            .iter()
                            .filter(|p| p.library == s.library)
                            .map(|p| p.id)
                            .collect(),
                        None => Vec::new(),
                    };
                    // Gone (or moved to another library): the pairs that knew it.
                    for p in self.state.pairs_of_node(change.id)? {
                        if pairs.iter().any(|x| x.id == p) && !targets.contains(&p) {
                            targets.push(p);
                        }
                    }
                    for pair in targets {
                        let p = pairs.iter().find(|p| p.id == pair).expect("filtered");
                        let state = change
                            .state
                            .as_ref()
                            .filter(|s| s.library == p.library)
                            .map(|s| {
                                remote_state(
                                    s.parent.unwrap_or_default(),
                                    s.name.clone(),
                                    s.is_file,
                                    s.current_version,
                                    s.size,
                                    s.modified,
                                )
                            });
                        states.entry(pair).or_default().push((change.id, state));
                    }
                }
                api::ResourceKind::Unknown => {}
            }
        }
        let position = |library: &Uuid| {
            changes.positions.get(library).map(|&seq| {
                let event = changes.events.get(library).copied();
                api::Position::Event { seq, event }
            })
        };
        for library in changes.positions.keys() {
            self.positions
                .insert(*library, position(library).expect("listed"));
        }
        for p in pairs.iter().filter(|p| p.position.is_some()) {
            let mut list = states.remove(&p.id).unwrap_or_default();
            let position = position(&p.library);
            if list.is_empty() && position.is_none() {
                continue;
            }
            self.check_recent(p.id, &mut list).await;
            // A folder this pair doesn't know: its content isn't in the feed
            // (a copy, a restore, an arrival): its tree is listed.
            let mut listed = Vec::new();
            for (node, state) in &list {
                if let Some(s) = state
                    && !s.is_file
                    && !self.state.knows(p.id, *node)?
                {
                    match self.tree(*node).await {
                        Ok(below) => listed.extend(below),
                        // Gone since: a later message says so.
                        Err(Error::Api { status: 404, .. }) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            list.extend(listed);
            self.state.record_feed(p.id, &list, position)?;
            self.dirty.insert(p.id);
        }
        Ok(())
    }

    /// Remote states about nodes this client just changed may predate the
    /// change: the server is asked for the node as it is now. Not found while
    /// the feed said it exists: no state at all, neither the old one nor
    /// "gone" (its library may only be in the trash, a deleted file would be
    /// deleted locally): a later message says what became of it.
    async fn check_recent(&mut self, pair: i64, list: &mut Vec<(Uuid, Option<RemoteState>)>) {
        let recent = self.recent.entry(pair).or_default();
        recent.retain(|_, at| at.elapsed() < RECENT);
        let mut unknown = HashSet::new();
        for (node, state) in list.iter_mut() {
            if !recent.contains_key(node) {
                continue;
            }
            match self.client.node(*node).await {
                Ok(info) => {
                    *state = Some(remote_state(
                        info.parent.unwrap_or_default(),
                        info.name,
                        info.is_file,
                        info.current_version,
                        info.size,
                        info.modified,
                    ));
                }
                Err(Error::Api { status: 404, .. }) if state.is_some() => {
                    unknown.insert(*node);
                }
                Err(_) => {}
            }
        }
        list.retain(|(node, _)| !unknown.contains(node));
    }

    /// A folder's tree, as remote states.
    async fn tree(&self, folder: Uuid) -> Result<Vec<(Uuid, Option<RemoteState>)>, Error> {
        let mut lines = Box::pin(self.client.tree(folder).await?);
        let mut ids: HashMap<String, Uuid> = HashMap::new();
        let mut states = Vec::new();
        while let Some(node) = lines.next().await {
            let node = node?;
            let (parent, name) = match node.path.rsplit_once('/') {
                Some((up, name)) => (ids.get(up).copied().unwrap_or(folder), name.to_owned()),
                None => (folder, node.path.clone()),
            };
            ids.insert(node.path.clone(), node.id);
            states.push((
                node.id,
                Some(remote_state(
                    parent,
                    name,
                    node.is_file,
                    node.current_version,
                    node.size,
                    node.modified,
                )),
            ));
        }
        Ok(states)
    }

    /// Rounds for the pairs that need one.
    async fn rounds(&mut self) -> Result<(), EngineError> {
        let now = Instant::now();
        if self.retry_at.is_some_and(|at| at <= now) {
            self.retry_at = None;
            for p in self.pairs()? {
                self.dirty.insert(p.id);
            }
        }
        for p in self.pairs()? {
            // The hourly full scan; and a folder gone (renamed: followed)
            // without the watcher, which watches its old path, telling.
            if self.full_at.get(&p.id).is_none_or(|at| *at <= now) || !p.path.is_dir() {
                self.dirty.insert(p.id);
            }
        }
        // A pair stopped for its folder resumes once the folder passes the
        // root check again: what a Resume by hand did.
        for p in self.pairs()? {
            if p.paused || !p.stopped_for_its_folder() {
                self.stopped_at.remove(&p.id);
                continue;
            }
            let here = local::root_id(&p.path);
            if self.stopped_at.get(&p.id) == Some(&here) {
                continue;
            }
            self.stopped_at.insert(p.id, here);
            if let Ok(back) = self.root(&p).await? {
                self.state.set_stopped(back.id, None)?;
                self.state.notify(
                    Some(back.account),
                    Some(back.id),
                    "info",
                    Some(&back.path.to_string_lossy()),
                    "the folder is back: syncing again",
                    None,
                )?;
                self.dirty.insert(back.id);
            }
        }
        let due: Vec<i64> = self
            .deletions_held
            .keys()
            .copied()
            .filter(|p| self.may_trash(*p))
            .collect();
        self.dirty.extend(due);
        let dirty: Vec<i64> = self.dirty.drain().collect();
        for id in dirty {
            if *self.stopping.borrow() {
                break;
            }
            let Some(p) = self.state.pair(id)? else {
                continue;
            };
            if p.paused || p.stop.is_some() {
                continue;
            }
            let Some(&announced) = self.announced.get(&p.library) else {
                // Not connected yet: nothing known of the remote side.
                continue;
            };
            if p.position.is_some_and(|pos| pos < announced) {
                // Catching up: the feed's next batches bring it.
                continue;
            }
            match self.round(&p).await {
                Ok(()) => {}
                Err(EngineError::Client(e)) if matches!(e.code(), Some("unauthenticated")) => {
                    return Err(EngineError::Client(e));
                }
                Err(EngineError::Client(e)) => {
                    self.state.set_activity(p.id, "offline", false)?;
                    tracing::warn!(pair = p.id, "round failed: {e}");
                    self.retry_at = Some(Instant::now() + RETRY);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn round(&mut self, p: &Pair) -> Result<(), EngineError> {
        tracing::debug!(pair = p.id, "round");
        let p = match self.root(p).await? {
            Ok(p) => p,
            Err(why) => {
                self.stop(p, Stop::Folder(why))?;
                return Ok(());
            }
        };
        if p.position.is_none() && !self.resync(&p).await? {
            return Ok(());
        }
        self.full_at.insert(p.id, Instant::now() + FULL_SCAN);
        self.state.set_activity(p.id, "syncing", false)?;
        let base = self.state.entries(p.id)?;
        let remote = self.state.remote(p.id)?;
        let path = p.path.clone();
        // An ignored folder is scanned only where the server has something.
        let known = crate::plan::known_paths(p.root, &base, &remote);
        let mut scan =
            match tokio::task::spawn_blocking(move || local::scan_entering(&path, &known)).await {
                Ok(Ok(scan)) => scan,
                _ => {
                    // Renamed or removed during this round: the root check says
                    // which (followed, or stopped as missing); the next round
                    // runs from there. Unreadable where it is: stopped as such.
                    match self.root(&p).await? {
                        Ok(now) if now.path != p.path => {
                            self.dirty.insert(p.id);
                        }
                        Ok(_) => self.stop(&p, Stop::Folder("the folder can't be read".into()))?,
                        Err(why) => self.stop(&p, Stop::Folder(why))?,
                    }
                    return Ok(());
                }
            };
        let real = p.path.canonicalize().unwrap_or_else(|_| p.path.clone());
        self.skipped
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                p.id,
                scan.not_entered
                    .iter()
                    .map(|path| real.join(path.replace('/', std::path::MAIN_SEPARATOR_STR)))
                    .collect(),
            );
        let planned = |scan: &local::Scan| {
            plan(&Input {
                root: p.root,
                base: &base,
                scan,
                remote: &remote,
                writable: p.writable,
            })
        };
        let mut plan = planned(&scan);
        // Something to trash, and ignored folders the scan didn't enter: what
        // was synced may have moved in there (no deletion). Their file IDs,
        // read now only.
        let trashes = |plan: &crate::plan::Plan| {
            plan.actions
                .iter()
                .any(|a| matches!(a, Action::RemoteTrash { .. }))
        };
        if trashes(&plan) && !scan.not_entered.is_empty() {
            let (root, folders) = (p.path.clone(), scan.not_entered.clone());
            if let Ok(ids) =
                tokio::task::spawn_blocking(move || local::ids_below(&root, &folders)).await
            {
                scan.hidden_ids = ids;
                plan = planned(&scan);
            }
        }
        let held = !self.may_trash(p.id)
            && plan
                .actions
                .iter()
                .any(|a| matches!(a, Action::RemoteTrash { .. }));
        if held {
            plan.actions
                .retain(|a| !matches!(a, Action::RemoteTrash { .. }));
            self.deletions_held.entry(p.id).or_insert_with(Instant::now);
        } else {
            self.deletions_held.remove(&p.id);
        }
        let outcome = Executor::new(&self.client, &self.state, &p, &self.cancel, &self.stopping)?
            .run(plan, &scan)
            .await?;
        let recent = self.recent.entry(p.id).or_default();
        for node in &outcome.touched {
            recent.insert(*node, Instant::now());
        }
        if !outcome.trashed.is_empty() {
            let undo: Vec<String> = outcome.trashed.iter().map(Uuid::to_string).collect();
            self.state.notify(
                Some(p.account),
                Some(p.id),
                "info",
                None,
                &format!(
                    "{} moved to the trash of {}",
                    plural(outcome.trashed.len(), "item"),
                    p.library_name
                ),
                Some(&undo.join(" ")),
            )?;
        }
        if outcome.again {
            self.retry_at = Some(Instant::now() + RETRY);
            self.state.set_activity(p.id, "syncing", false)?;
        } else if held {
            self.state.set_activity(p.id, "syncing", false)?;
        } else {
            self.state.set_activity(p.id, "up to date", true)?;
        }
        Ok(())
    }

    /// The pair's folder, checked: the same folder as before (its file ID),
    /// followed if renamed while watched, or its disk remounted on another
    /// device number if its files vouch for it; otherwise why not (the caller
    /// stops the pair, or leaves a stopped one as it is).
    async fn root(&mut self, p: &Pair) -> Result<Result<Pair, String>, StateError> {
        let here = local::root_id(&p.path);
        let same = match (&here, &p.root_file_id) {
            (Some(here), Some(known)) => here == known,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if same && p.path.is_dir() {
            return Ok(Ok(p.clone()));
        }
        tracing::debug!(pair = p.id, path = %p.path.display(), ?here, known = ?p.root_file_id,
            held = ?self.roots.get(&p.id).map(filepath::FilePath::path), "root changed");
        if let (Some(here), Some(known)) = (&here, &p.root_file_id)
            && local::remounted(known, here)
            && p.path.is_dir()
        {
            let path = p.path.clone();
            let scan = tokio::task::spawn_blocking(move || local::scan(&path)).await;
            let base = self.state.entries(p.id)?;
            if let Ok(Ok(scan)) = scan
                && crate::plan::same_disk(&base, &scan)
            {
                self.state.set_pair_path(p.id, &p.path, Some(here))?;
                self.state.notify(
                    Some(p.account),
                    Some(p.id),
                    "info",
                    Some(&p.path.to_string_lossy()),
                    "the disk was mounted again: followed",
                    None,
                )?;
                // Watched and held anew: the old ones were on the gone mount.
                self.watchers.remove(&p.id);
                self.roots.remove(&p.id);
                return Ok(self.state.pair(p.id)?.ok_or_else(String::new));
            }
            return Ok(Err(format!(
                "the disk at {} doesn't match the one synced: if it is that one, locate it",
                p.path.display()
            )));
        }
        // Renamed or moved while held open: where is it now?
        if let Some(moved) = self
            .roots
            .get(&p.id)
            .and_then(|f| filepath::FilePath::path(f).ok())
            && moved != p.path
            && moved.is_dir()
            && local::root_id(&moved) == p.root_file_id
        {
            if in_trash(&moved) {
                return Ok(Err("the folder was moved to the trash".into()));
            }
            // Never into or around another pair's folder, as when added.
            if let Err(e) = ops::check_path(&self.state, &moved, Some(p.id)) {
                return Ok(Err(format!("moved to {}: {e}", moved.display())));
            }
            self.state
                .set_pair_path(p.id, &moved, p.root_file_id.as_deref())?;
            self.state.notify(
                Some(p.account),
                Some(p.id),
                "info",
                Some(&moved.to_string_lossy()),
                &format!("the folder moved from {}: followed", p.path.display()),
                None,
            )?;
            self.watchers.remove(&p.id);
            return Ok(self.state.pair(p.id)?.ok_or_else(String::new));
        }
        Ok(Err(format!("{} is missing", p.path.display())))
    }

    /// Lists the library's tree as its remote state (a reset, a lost state, a
    /// first sync), from the position the feed announced before the listing.
    /// False: no position known yet.
    async fn resync(&mut self, p: &Pair) -> Result<bool, EngineError> {
        let Some(&position) = self.positions.get(&p.library) else {
            return Ok(false);
        };
        self.state
            .set_activity(p.id, "listing the library", false)?;
        let listed = self.tree(p.root).await?;
        let present: HashSet<Uuid> = listed.iter().map(|(n, _)| *n).collect();
        let mut states = listed;
        for e in self.state.entries(p.id)? {
            if !present.contains(&e.node) {
                states.push((e.node, None));
            }
        }
        self.state.record_remote(p.id, &states, Some(position))?;
        Ok(true)
    }
}

/// When each pair's watcher last reported a change.
type Heard = Arc<std::sync::Mutex<HashMap<i64, Instant>>>;

/// Each pair's ignored folders its last scan skipped, as the watcher reports
/// paths (below the folder's real path).
type Skipped = Arc<std::sync::Mutex<HashMap<i64, Vec<PathBuf>>>>;

/// Whether every path of an event is at or below an ignored folder the
/// pair's last scan skipped: nothing there is synced.
fn below_skipped(skipped: &Skipped, pair: i64, paths: &[PathBuf]) -> bool {
    let skipped = skipped.lock().unwrap_or_else(|e| e.into_inner());
    let Some(folders) = skipped.get(&pair) else {
        return false;
    };
    !paths.is_empty()
        && paths
            .iter()
            .all(|p| folders.iter().any(|f| p.starts_with(f)))
}

fn heard(heard_at: &Heard, pair: i64) {
    heard_at
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(pair, Instant::now());
}

/// `1 item`, `3 items`.
pub fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

/// The OS's trash folders (`.Trash`, `.local/share/Trash`, `$Recycle.Bin`).
fn in_trash(path: &Path) -> bool {
    path.components().any(|c| {
        let c = c.as_os_str().to_string_lossy().to_lowercase();
        c == ".trash" || c == "trash" || c == "$recycle.bin" || c.starts_with(".trash-")
    })
}

/// Whether a watcher event may mean a change: reads don't, a file closed
/// after writing does.
fn changes(kind: &notify::EventKind) -> bool {
    use notify::event::{AccessKind, AccessMode, EventKind};
    match kind {
        EventKind::Access(access) => *access == AccessKind::Close(AccessMode::Write),
        _ => true,
    }
}

/// A folder held open without stopping anyone renaming or deleting it.
fn open_dir(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_BACKUP_SEMANTICS opens a folder; every share mode, so it
        // can still be renamed (FILE_SHARE_DELETE) while held.
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(0x0200_0000)
            .share_mode(0x1 | 0x2 | 0x4)
            .open(path)
    }
    #[cfg(not(windows))]
    std::fs::File::open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Watcher events below an ignored folder the last scan skipped start no
    /// round; one with any path elsewhere does, and so does any before a scan.
    #[test]
    fn events_below_skipped_folders_start_no_round() {
        let skipped = Skipped::default();
        let root = PathBuf::from("/pair");
        let paths = |ps: &[&str]| ps.iter().map(|p| root.join(p)).collect::<Vec<_>>();
        assert!(!below_skipped(&skipped, 1, &paths(&["node_modules/x.js"])));
        skipped
            .lock()
            .unwrap()
            .insert(1, vec![root.join("node_modules")]);
        assert!(below_skipped(&skipped, 1, &paths(&["node_modules/x.js"])));
        assert!(below_skipped(&skipped, 1, &paths(&["node_modules"])));
        assert!(!below_skipped(
            &skipped,
            1,
            &paths(&["node_modules/x.js", "a.txt"])
        ));
        assert!(!below_skipped(&skipped, 1, &paths(&["node_modules2/x.js"])));
        assert!(!below_skipped(&skipped, 1, &[]));
        assert!(!below_skipped(&skipped, 2, &paths(&["node_modules/x.js"])));
    }

    /// C16: an engine stopped by an error says so, once however often.
    #[test]
    fn a_stopped_engine_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        let state = State::open(&path).unwrap();
        let account = state
            .save_account(&Account {
                id: 0,
                instance: "https://x".into(),
                uuid: Uuid::now_v7(),
                email: "a@x".into(),
                device: "d".into(),
                password: crate::state::Password::InState("p".into()),
                default_folder: dir.path().to_owned(),
                auto_sync_limit: None,
                signed_out: false,
            })
            .unwrap();
        let e = EngineError::Client(Error::Decode("a cut listing".into()));
        stopped(&path, account, &e).unwrap();
        stopped(&path, account, &e).unwrap();
        let notices = state.notices().unwrap();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].message.contains("a cut listing"));
    }
}
