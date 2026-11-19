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

use crate::drive;
use crate::exec::{Executor, remote_state};
use crate::keychain::{self, KeychainError};
use crate::local;
use crate::ops;
use crate::plan::{Action, Input, plan};
use crate::state::{
    Account, Mode, NewLibraries, Offer, Pair, RemoteState, State, StateError, Stop, Update,
};

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
    /// The macOS drives this process serves to the File Provider extension.
    pub provider: Arc<crate::drive::provider::Provider>,
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
            provider: Arc::default(),
        }
    }
}

/// What a library new to an account came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Acted {
    /// Synced, or a drive.
    Added,
    /// Listed (notified or not): decided.
    Listed,
    /// Not yet (no room, a failure): tried again, still new.
    Again,
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
    // The File Provider extension's socket, for every macOS drive.
    #[cfg(target_os = "macos")]
    crate::drive::provider::listen(handle.provider.clone(), tokio::runtime::Handle::current());
    // Drive removals a crash left half done.
    #[cfg(any(target_os = "linux", windows))]
    crate::drive::finish_removals(&State::open(&state_path)?);
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
            // Every line of the account's engine says which account (inside
            // the caller's span: the tests name each device).
            let span =
                tracing::info_span!("account", id = account.id, instance = %account.instance);
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
                    .instrument(span),
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
    /// Each running drive's mount (FUSE) or sync root (Windows), and what fetches for them.
    #[cfg(any(target_os = "linux", windows))]
    mounts: HashMap<i64, drive::Served>,
    #[cfg(any(target_os = "linux", windows))]
    fetcher: drive::fetch::Fetcher,
    /// The macOS drives served to the extension, and the state they read
    /// (a connection of their own).
    #[cfg(target_os = "macos")]
    provider: Arc<crate::drive::provider::Provider>,
    #[cfg(target_os = "macos")]
    provider_state: Arc<State>,
    #[cfg(target_os = "macos")]
    served: HashSet<i64>,
    /// Per macOS drive, the system's last answer when it couldn't be asked
    /// (said once, not every round).
    #[cfg(target_os = "macos")]
    unchecked: HashMap<i64, String>,
    /// The macOS drives whose kept files are being downloaded (a thread
    /// each, at most).
    #[cfg(target_os = "macos")]
    materializing: Arc<std::sync::Mutex<HashSet<i64>>>,
    /// Each pair's counter of changes to its entries, as last seen: a drive
    /// whose counter moved (a node kept, from another process) runs a round.
    seqs: HashMap<i64, i64>,
    /// Drives with nodes freed (`theca free`) to evict: as soon as possible,
    /// tried every pass while a file of theirs is open.
    #[cfg(any(target_os = "linux", windows))]
    freeing: HashSet<i64>,
    /// When the drive limit is next checked: every minute, and after a fetch.
    limit_at: Instant,
    #[cfg(any(target_os = "linux", windows))]
    fetched: Arc<std::sync::atomic::AtomicBool>,
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
    let cancel = Cancel::default();
    // Fetches for the drives' mounts, beside the rounds, on a connection of
    // their own; aborted with this task.
    let mut cancelling = tokio::task::JoinSet::new();
    #[cfg(any(target_os = "linux", windows))]
    let fetched = Arc::new(std::sync::atomic::AtomicBool::new(false));
    #[cfg(any(target_os = "linux", windows))]
    let fetcher = {
        let state = Arc::new(State::open(state_path)?);
        let (fetcher, serve) =
            drive::fetch::service(state, client.clone(), cancel.clone(), fetched.clone());
        // In the account's span: a fetch's lines say which account.
        cancelling.spawn(serve.in_current_span());
        fetcher
    };
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
        #[cfg(target_os = "macos")]
        provider: handle.provider.clone(),
        #[cfg(target_os = "macos")]
        provider_state: Arc::new(State::open(state_path)?),
        #[cfg(target_os = "macos")]
        served: HashSet::new(),
        #[cfg(target_os = "macos")]
        unchecked: HashMap::new(),
        #[cfg(target_os = "macos")]
        materializing: Default::default(),
        stopped_at: HashMap::new(),
        stopping: shutdown.clone(),
        cancel,
        #[cfg(any(target_os = "linux", windows))]
        mounts: HashMap::new(),
        #[cfg(any(target_os = "linux", windows))]
        fetcher,
        seqs: HashMap::new(),
        #[cfg(any(target_os = "linux", windows))]
        freeing: HashSet::new(),
        limit_at: Instant::now(),
        #[cfg(any(target_os = "linux", windows))]
        fetched,
    };
    // The stop cancels the transfers in flight (aborted with this task).
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
                Some(Ok(FeedMessage::Libraries(libraries))) => {
                    tracing::debug!(libraries = libraries.len(), "the feed: libraries");
                    f.libraries(libraries).await?
                }
                Some(Ok(FeedMessage::Changes(changes))) => {
                    tracing::debug!(?changes, "the feed: changes");
                    f.changes(changes).await?
                }
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
        #[cfg(any(target_os = "linux", windows))]
        f.free()?;
        f.keep_under_limit()?;
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
            .map(|p| self.local_side(p))
            .collect())
    }

    /// Each running drive mounted (FUSE) or connected (Windows) on its folder, once that folder is
    /// bare (the root check stops it otherwise); the others unmounted.
    #[cfg(any(target_os = "linux", windows))]
    fn mount(&mut self, pairs: &[Pair], running: &HashSet<i64>) -> Result<(), EngineError> {
        self.mounts.retain(|p, _| running.contains(p));
        for p in pairs
            .iter()
            .filter(|p| p.mode == Mode::Drive && running.contains(&p.id))
        {
            if self.mounts.contains_key(&p.id) {
                continue;
            }
            let Some(folder) = self.state.pair(p.id)?.map(|p| p.path) else {
                continue;
            };
            match drive::serve(p, &folder, self.fetcher.clone()) {
                Ok(m) => {
                    self.mounts.insert(p.id, m);
                }
                Err(why) => self.stop(p, Stop::Folder(why))?,
            }
        }
        Ok(())
    }

    /// What `theca free` asked, evicted now: a file open waits for the next
    /// pass (every second).
    #[cfg(any(target_os = "linux", windows))]
    fn free(&mut self) -> Result<(), EngineError> {
        let pairs: Vec<i64> = self.freeing.iter().copied().collect();
        for id in pairs {
            let (Some(p), Some(mount)) = (self.state.pair(id)?, self.mounts.get(&id)) else {
                self.freeing.remove(&id);
                continue;
            };
            if drive::free_now(&self.state, &p, Some(&mount.busy))? == 0 {
                self.freeing.remove(&id);
            }
        }
        Ok(())
    }

    /// Drives' downloads kept under the drive limit (one per computer): the
    /// same choice over every drive in each account's engine, each evicting
    /// its own drives' files, least recently used first, never one open.
    #[cfg(any(target_os = "linux", windows))]
    fn keep_under_limit(&mut self) -> Result<(), EngineError> {
        use std::sync::atomic::Ordering;
        let now = Instant::now();
        if now < self.limit_at && !self.fetched.swap(false, Ordering::Relaxed) {
            return Ok(());
        }
        self.limit_at = now + Duration::from_secs(60);
        let limit = drive::limit(&self.state)?;
        let mut files = Vec::new();
        for p in self.state.pairs()? {
            if p.mode != Mode::Drive {
                continue;
            }
            let busy = self.mounts.get(&p.id).map(|m| &m.busy);
            let own = p.account == self.account.id;
            for f in drive::downloaded(&self.state, &p, |rel| busy.is_some_and(|b| b.is_open(rel)))?
            {
                files.push(drive::Downloaded {
                    file: (own.then_some(p.id), f.file),
                    size: f.size,
                    used: f.used,
                    busy: f.busy,
                });
            }
        }
        for (pair, rel) in drive::evictions(files, limit) {
            let Some(pair) = pair else {
                continue;
            };
            let Some(mount) = self.mounts.get(&pair) else {
                continue;
            };
            let Some(_taken) = mount.busy.try_take(&rel) else {
                continue;
            };
            let Some(p) = self.state.pair(pair)? else {
                continue;
            };
            let names: Vec<String> = rel
                .iter()
                .map(|n| n.to_string_lossy().into_owned())
                .collect();
            let (Some(root), Some(entry)) = (
                drive::root_of(&self.state, &p),
                self.state.entry_at(pair, p.root, &names)?,
            ) else {
                continue;
            };
            match drive::evict_synced(&root.join(&rel), &entry) {
                Ok(true) => tracing::debug!(pair, path = %rel.display(), "evicted"),
                Ok(false) => tracing::debug!(pair, path = %rel.display(), "edited since: kept"),
                Err(e) => tracing::warn!(pair, path = %rel.display(), "not evicted: {e}"),
            }
        }
        Ok(())
    }

    /// A macOS drive's round: no scan, no plan (the system holds the local
    /// side). The feed's states applied to its entries, the system told; its
    /// domain removed (System Settings, by hand): stopped.
    #[cfg(target_os = "macos")]
    async fn provider_round(&mut self, p: &Pair) -> Result<(), EngineError> {
        let domain = drive::provider::domain_of(p);
        let enabled = theca_fileprovider::host::enabled(&domain);
        match &enabled {
            Err(e) if self.unchecked.get(&p.id) != Some(e) => {
                tracing::warn!(pair = p.id, "the drive not checked: {e}");
                self.unchecked.insert(p.id, e.clone());
            }
            Err(_) => {}
            Ok(_) => {
                if self.unchecked.remove(&p.id).is_some() {
                    tracing::info!(pair = p.id, "the drive checked again");
                }
            }
        }
        match enabled {
            Ok(None) => {
                let why = "the drive was removed from this Mac".to_owned();
                self.stop(p, Stop::Folder(why))?;
                return Ok(());
            }
            // Switched on: its folder is where the system put it (`add`
            // guessed it while it was off).
            Ok(Some(true)) => {
                if let Ok(folder) = theca_fileprovider::host::folder(&domain)
                    && folder != p.path
                {
                    self.state.set_pair_path(p.id, &folder, None)?;
                }
            }
            Ok(Some(false)) | Err(_) => {}
        }
        // A fresh listing wrote its own activity since `p` was read.
        let listed = p.position.is_none();
        if listed && !self.resync(p).await? {
            return Ok(());
        }
        self.materialize(p)?;
        let changed = drive::provider::apply_remote(&self.state, p)?;
        if changed && let Err(e) = theca_fileprovider::host::signal(&domain) {
            tracing::debug!(pair = p.id, "the system not told: {e}");
        }
        // Said when it changes (every Activity redraws the window and
        // tray), which also ends an error the pair stopped on.
        if p.activity != "up to date" || p.error.is_some() || listed || changed {
            self.state.set_activity(p.id, "up to date", true)?;
        }
        Ok(())
    }

    /// A macOS drive's files kept and not downloaded (a keep from Finder or
    /// `theca keep`, a file new in a kept folder): downloaded now by a read,
    /// on a thread of the drive's own (macOS's own eager download is a
    /// background task, which may wait long). Each file is tried once: one
    /// kept meanwhile joins, one freed meanwhile is left; a failed one waits
    /// for the drive's next round.
    #[cfg(target_os = "macos")]
    fn materialize(&self, p: &Pair) -> Result<(), EngineError> {
        use drive::provider::{kept_files, still_kept};
        fn locked(
            busy: &std::sync::Mutex<HashSet<i64>>,
        ) -> std::sync::MutexGuard<'_, HashSet<i64>> {
            busy.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
        let materialized = self.provider.materialized(p);
        let mut wanted: Vec<_> = kept_files(&self.state, p)?
            .into_iter()
            .filter(|(node, _)| !materialized.contains(node))
            .collect();
        if wanted.is_empty() || !locked(&self.materializing).insert(p.id) {
            return Ok(());
        }
        let (state, provider, pair) = (
            self.provider_state.clone(),
            self.provider.clone(),
            p.clone(),
        );
        let busy = self.materializing.clone();
        let span = tracing::Span::current();
        std::thread::spawn(move || {
            let _in = span.enter();
            let mut tried = HashSet::new();
            loop {
                for (node, path) in wanted {
                    tried.insert(node);
                    if !matches!(still_kept(&state, &pair, node), Ok(true)) {
                        continue;
                    }
                    match theca_fileprovider::host::materialize(&path) {
                        Ok(()) => tracing::debug!(%node, "a kept file downloaded"),
                        Err(e) => tracing::warn!(%node, "a kept file not downloaded: {e}"),
                    }
                }
                // Looked at again with the drive still marked busy: a keep
                // made now either is in this listing or starts a thread.
                let mut busy = locked(&busy);
                let materialized = provider.materialized(&pair);
                wanted = kept_files(&state, &pair)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|(node, _)| !materialized.contains(node) && !tried.contains(node))
                    .collect();
                if wanted.is_empty() {
                    busy.remove(&pair.id);
                    return;
                }
            }
        });
        Ok(())
    }

    /// Each running macOS drive served to the extension; the others not.
    #[cfg(target_os = "macos")]
    fn serve(&mut self, pairs: &[Pair], running: &HashSet<i64>) {
        for p in pairs.iter().filter(|p| p.mode == Mode::Drive) {
            let serve = running.contains(&p.id);
            if serve && self.served.insert(p.id) {
                self.provider.serve(
                    p,
                    self.client.clone(),
                    self.provider_state.clone(),
                    self.cancel.clone(),
                );
            } else if !serve && self.served.remove(&p.id) {
                self.provider.forget(p);
            }
        }
    }

    /// macOS's drives kept under the drive limit, every minute: what the
    /// system says is downloaded, not kept, least recently used first,
    /// evicted by the system (it refuses an edit not uploaded, a file in use).
    #[cfg(target_os = "macos")]
    fn keep_under_limit(&mut self) -> Result<(), EngineError> {
        let now = Instant::now();
        if now < self.limit_at {
            return Ok(());
        }
        self.limit_at = now + Duration::from_secs(60);
        let limit = drive::limit(&self.state)?;
        let mut files = Vec::new();
        for p in self.pairs()?.into_iter().filter(|p| p.mode == Mode::Drive) {
            let kept = self.state.kept(p.id)?;
            let used = self.state.used(p.id)?;
            for node in self.provider.materialized(&p) {
                let Some(e) = self.state.entry(p.id, node)? else {
                    continue;
                };
                if !e.is_file || drive::provider::kept_here(&self.state, &p, &e, &kept)? {
                    continue;
                }
                files.push(drive::Downloaded {
                    file: (drive::provider::domain_of(&p), node),
                    size: u64::try_from(e.size).unwrap_or_default(),
                    used: used.get(&node).copied().unwrap_or_default(),
                    busy: false,
                });
            }
        }
        for (domain, node) in drive::evictions(files, limit) {
            if let Err(e) = theca_fileprovider::host::evict(&domain, &node.to_string()) {
                tracing::debug!(%node, "not evicted: {}", e.why);
            }
        }
        Ok(())
    }

    /// No drive limit to keep elsewhere (no drives).
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    fn keep_under_limit(&mut self) -> Result<(), EngineError> {
        Ok(())
    }

    /// A pair as its rounds see it: a FUSE drive's path is its backing
    /// folder (its own folder is the mount, served from it).
    fn local_side(&self, p: Pair) -> Pair {
        #[cfg(target_os = "linux")]
        if p.mode == Mode::Drive
            && let Some(path) = drive::backing(&self.state, p.id)
        {
            return Pair { path, ..p };
        }
        p
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
        #[cfg(any(target_os = "linux", windows))]
        self.mount(&pairs, &running)?;
        #[cfg(target_os = "macos")]
        self.serve(&pairs, &running);
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
                    // the base, so the resync has no base (wiki: Sync client,
                    // Reconciliation).
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

    /// The libraries the account sees and doesn't sync or mount: listed
    /// (`Update::Offered`, the window; `libraries`, the CLI). One new to the
    /// account (not in `seen`) gets its `new_libraries` once, then is seen:
    /// synced into the default folder (up to the size limit: notified above
    /// it), a drive (where one can't be: an error notice), notified, or only
    /// listed.
    async fn auto_sync(&mut self) -> Result<(), EngineError> {
        // The account as it is now: its choice and folder are settings.
        if let Some(account) = self.state.account(self.account.id)? {
            self.account = account;
        }
        let pairs = self.pairs()?;
        let seen = self.state.seen(self.account.id)?;
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
            let new = !seen.contains(&l.id);
            let acted = match (new, self.account.new_libraries) {
                (false, _) => Acted::Listed,
                (true, NewLibraries::Silent) => Acted::Listed,
                (true, NewLibraries::Notify) => {
                    self.notify_new(&l.name);
                    Acted::Listed
                }
                (true, NewLibraries::Sync) => {
                    let acted = self.sync_new(&l, info.size, &others, &mut taken).await?;
                    if acted == Acted::Listed {
                        self.notify_new(&l.name);
                    }
                    acted
                }
                (true, NewLibraries::Drive) => self.drive_new(&l).await?,
            };
            // Seen once decided: a disk full, a network failure, is tried
            // again the next time (1.1.0 retried every pass too).
            if new && acted != Acted::Again {
                self.state.see(self.account.id, &[l.id])?;
            }
            if acted != Acted::Added {
                offered.push(Offer {
                    library: l.id,
                    name: l.name.clone(),
                    size: info.size,
                });
            }
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

    /// A new library said (the window shows an OS notification).
    fn notify_new(&self, name: &str) {
        tracing::info!(library = name, "a new library, listed");
        self.state.publish(Update::NewLibrary {
            account: self.account.id,
            name: name.to_owned(),
        });
    }

    /// A new library synced into the default folder: added; listed (to be
    /// notified) when it is over the size limit (0: any), its folder holds
    /// files or isn't one a pair may have; again later when the disk has no
    /// room or the folder can't be made.
    async fn sync_new(
        &mut self,
        l: &api::FeedLibrary,
        size: i64,
        others: &[(Uuid, String)],
        taken: &mut Vec<PathBuf>,
    ) -> Result<Acted, EngineError> {
        let limit = self.account.auto_sync_limit;
        let path = ops::default_path(
            &self.client,
            &self.account.default_folder,
            (l.id, &l.name, l.organization),
            others,
            taken,
        )
        .await;
        let fits = limit == 0 || size <= limit;
        // A folder already there with content: listed, never merged unasked.
        let occupied = std::fs::read_dir(&path).is_ok_and(|mut d| d.next().is_some());
        if !fits || occupied || ops::check_path(&self.state, &path, None).is_err() {
            return Ok(Acted::Listed);
        }
        // Created here, last: the pair's folder must exist before its first
        // round (it would stop as missing otherwise).
        if !ops::has_room(&path, size) || std::fs::create_dir_all(&path).is_err() {
            tracing::info!(library = l.name, path = %path.display(), "not synced yet: no room, or no folder");
            return Ok(Acted::Again);
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
            mode: Mode::Sync,
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
        Ok(Acted::Added)
    }

    /// A new library added as a drive; where one can't be, an error notice
    /// naming it and why, never synced instead; the add failing (the
    /// server, the keychain), the notice too, and again later.
    async fn drive_new(&mut self, l: &api::FeedLibrary) -> Result<Acted, EngineError> {
        let (refused, acted) = match crate::drive::unavailable() {
            Some(why) => (why, Acted::Listed),
            None => match ops::add(
                &self.state,
                &self.account,
                &l.id.to_string(),
                None,
                false,
                Mode::Drive,
            )
            .await
            {
                Ok(pair) => {
                    self.state.notify(
                        Some(self.account.id),
                        Some(pair.id),
                        "info",
                        Some(&pair.path.to_string_lossy()),
                        &format!("{} is now a drive here", l.name),
                        None,
                    )?;
                    self.dirty.insert(pair.id);
                    return Ok(Acted::Added);
                }
                Err(e) => (e.to_string(), Acted::Again),
            },
        };
        self.state.notify(
            Some(self.account.id),
            None,
            "error",
            None,
            &format!("{} not added as a drive: {refused}", l.name),
            None,
        )?;
        Ok(acted)
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
        tree(&self.client, folder).await
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
            // A macOS drive: no scan (the system holds its folder), its
            // rounds when something changes.
            #[cfg(target_os = "macos")]
            if p.mode == Mode::Drive {
                continue;
            }
            // The hourly full scan; and a folder gone (renamed: followed)
            // without the watcher, which watches its old path, telling.
            if self.full_at.get(&p.id).is_none_or(|at| *at <= now) || !p.path.is_dir() {
                self.dirty.insert(p.id);
            }
        }
        let seqs = self.state.seqs()?;
        for p in self.pairs()? {
            if self
                .seqs
                .get(&p.id)
                .is_some_and(|seen| Some(seen) != seqs.get(&p.id))
            {
                self.dirty.insert(p.id);
                #[cfg(any(target_os = "linux", windows))]
                self.freeing.insert(p.id);
            }
        }
        self.seqs = seqs;
        // A pair stopped for its folder resumes once the folder passes the
        // root check again: what a Resume by hand did.
        for p in self.pairs()? {
            if p.paused || !p.stopped_for_its_folder() {
                self.stopped_at.remove(&p.id);
                continue;
            }
            let here = local::root_id(&p.path);
            // A FUSE drive: also what its own folder holds (emptied, it mounts).
            #[cfg(target_os = "linux")]
            let here = match (p.mode, self.state.pair(p.id)?) {
                (Mode::Drive, Some(own)) => {
                    Some(format!("{here:?} {:?}", drive::fuse::listing(&own.path)))
                }
                _ => here,
            };
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
            let Some(p) = self.state.pair(id)?.map(|p| self.local_side(p)) else {
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
            let pair = tracing::info_span!("pair", id = p.id, library = %p.library_name);
            let round = tracing::info_span!(parent: &pair, "round");
            match self.round(&p).instrument(round).await {
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
        tracing::debug!("round");
        #[cfg(target_os = "macos")]
        if p.mode == Mode::Drive {
            return self.provider_round(p).await;
        }
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
        let drive = p.mode == Mode::Drive;
        let kept = if drive {
            self.state.kept(p.id)?
        } else {
            HashSet::new()
        };
        let path = p.path.clone();
        // An ignored folder is scanned only where the server has something.
        let known = crate::plan::known_paths(p.root, &base, &remote);
        let mut scan = match tokio::task::spawn_blocking(move || {
            if drive {
                crate::drive::scan(&path, &known)
            } else {
                local::scan_entering(&path, &known)
            }
        })
        .await
        {
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
        // A drive's fetch recorded between the base and the scan: the scan
        // shows the file downloaded, the base still a placeholder, and the
        // plan would read that as an edit (sent up, a pending remote delete
        // undone). The next round plans from both after it.
        if drive && self.state.entries(p.id)? != base {
            self.dirty.insert(p.id);
            return Ok(());
        }
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
                drive,
                kept: &kept,
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
        #[cfg(any(target_os = "linux", windows))]
        let busy = self.mounts.get(&p.id).map(|m| &m.busy);
        #[cfg(not(any(target_os = "linux", windows)))]
        let busy = None;
        let outcome = Executor::new(&self.client, &self.state, &p, &self.cancel, &self.stopping)?
            .with_busy(busy)
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
        // A FUSE drive not mounted: its own folder must be bare.
        #[cfg(target_os = "linux")]
        if p.mode == Mode::Drive
            && !self.mounts.contains_key(&p.id)
            && let Some(folder) = self.state.pair(p.id)?.map(|p| p.path)
            && let Err(why) = drive::fuse::bare(&folder)
        {
            return Ok(Err(why));
        }
        // A Windows drive whose sync root was unregistered (an uninstall, by
        // hand): every placeholder would read as deleted.
        #[cfg(windows)]
        if p.mode == Mode::Drive && !drive::cloud::registered(p) {
            return Ok(Err(format!(
                "{} isn't a drive for Windows anymore (its sync root was removed)",
                p.path.display()
            )));
        }
        let here = local::root_id(&p.path);
        let same = match (&here, &p.root_file_id) {
            (Some(here), Some(known)) => here == known,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if same && p.path.is_dir() {
            return Ok(Ok(p.clone()));
        }
        // A FUSE drive's backing folder is Theca's own, a Windows drive's
        // folder its sync root: never followed.
        #[cfg(target_os = "linux")]
        if p.mode == Mode::Drive {
            return Ok(Err(format!(
                "its files kept by Theca ({}) are missing or replaced",
                p.path.display()
            )));
        }
        #[cfg(windows)]
        if p.mode == Mode::Drive {
            return Ok(Err(format!("{} is missing or replaced", p.path.display())));
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

/// A folder's whole tree on the server, as remote states (a resync's listing).
pub async fn tree(
    client: &Client,
    folder: Uuid,
) -> Result<Vec<(Uuid, Option<RemoteState>)>, Error> {
    let mut lines = Box::pin(client.tree(folder).await?);
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
                new_libraries: Default::default(),
                auto_sync_limit: 0,
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
