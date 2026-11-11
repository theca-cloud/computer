//! Running a plan: the server through `theca-client`, the disk, and the
//! state, which follows each action as it succeeds. A failed action leaves its
//! remote state pending and its local change unsynced: the next round plans
//! it again.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use theca_client::{
    Cancel, Client, DownloadOptions, Downloader, Error, FileSource, Sink, Target, UploadOptions,
    Uploader,
};
use theca_common::api;
use theca_common::name::Name;
use uuid::Uuid;

use crate::drive::busy::{Busy, Taken};
use crate::local::{self, PART_SUFFIX, Scan};
use crate::plan::{Action, Level, Parent, Plan, UploadTo};
use crate::state::{Entry, Pair, RemoteState, State};

/// A file whose modification time is this recent may still be being written:
/// it waits for a later round.
const SETTLING: Duration = Duration::from_secs(2);

/// Files uploaded at once (each open until committed).
const OPEN_UPLOADS: usize = 64;

/// What a round did.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Something waits for a later round (a file still being written, an
    /// action that failed on the network).
    pub again: bool,
    /// Nodes changed on the server by this round: a remote state received
    /// for them meanwhile may predate the change.
    pub touched: Vec<Uuid>,
    /// Nodes trashed on the server, for Undo.
    pub trashed: Vec<Uuid>,
    /// Files transferred, for status.
    pub uploaded: usize,
    pub downloaded: usize,
}

pub struct Executor<'a> {
    client: &'a Client,
    state: &'a State,
    pair: &'a Pair,
    /// The transfers', cancelled when the process stops.
    cancel: &'a Cancel,
    /// The stop: no action starts once set.
    stopping: &'a tokio::sync::watch::Receiver<bool>,
    base: HashMap<Uuid, Entry>,
    /// Folders this round created on the server, by local path.
    created: HashMap<Vec<String>, Uuid>,
    /// Nodes whose local move the server refused this round: nothing else is
    /// done to them (a download would bring the old name back).
    unmoved: HashSet<Uuid>,
    /// Nodes this round moves locally, and the moves waiting for another one
    /// to free their place (a chain, a swap: `a` → `b` while `b` → `a`).
    moving: HashSet<Uuid>,
    waiting: Vec<(Uuid, Uuid, String)>,
    /// A node moved out of the way of a swap, where it is meanwhile.
    stepped_aside: HashMap<Uuid, PathBuf>,
    /// A mounted drive's: what its file system is serving.
    busy: Option<&'a Busy>,
    outcome: Outcome,
}

impl<'a> Executor<'a> {
    pub fn new(
        client: &'a Client,
        state: &'a State,
        pair: &'a Pair,
        cancel: &'a Cancel,
        stopping: &'a tokio::sync::watch::Receiver<bool>,
    ) -> crate::state::Result<Self> {
        let base = state
            .entries(pair.id)?
            .into_iter()
            .map(|e| (e.node, e))
            .collect();
        Ok(Self {
            client,
            state,
            pair,
            cancel,
            stopping,
            base,
            created: HashMap::new(),
            unmoved: HashSet::new(),
            moving: HashSet::new(),
            waiting: Vec::new(),
            stepped_aside: HashMap::new(),
            busy: None,
            outcome: Outcome::default(),
        })
    }

    /// A mounted drive's: its rounds leave alone what its file system serves.
    pub fn with_busy(mut self, busy: Option<&'a Busy>) -> Self {
        self.busy = busy;
        self
    }

    /// A drive's file taken for an action on it (`Ok(None)`: not a mounted
    /// drive's). `Err`: open or fetched through the mount, or below a folder
    /// that is: skipped, the next round does it.
    fn take(&mut self, path: &Path) -> Result<Option<Taken>, ()> {
        let Some(busy) = self.busy else {
            return Ok(None);
        };
        let rel = path.strip_prefix(&self.pair.path).unwrap_or(path);
        match busy.try_take(rel) {
            Some(taken) => Ok(Some(taken)),
            None => {
                self.outcome.again = true;
                Err(())
            }
        }
    }

    /// A node's base entries from the root down to it (none for the root).
    fn chain(&self, mut node: Uuid) -> Option<Vec<&Entry>> {
        let mut chain = Vec::new();
        while node != self.pair.root {
            let e = self.base.get(&node)?;
            chain.push(e);
            node = e.parent;
            if chain.len() > 10_000 {
                return None;
            }
        }
        chain.reverse();
        Some(chain)
    }

    /// A node's names from the root, as the base knows them.
    fn names(&self, node: Uuid) -> Option<Vec<String>> {
        Some(self.chain(node)?.iter().map(|e| e.name.clone()).collect())
    }

    /// A node's local path (the spelling on disk where it differs), found by
    /// the base's names and file IDs.
    fn path(&self, node: Uuid) -> Option<PathBuf> {
        let steps: Vec<_> = self
            .chain(node)?
            .iter()
            .map(|e| (e.name.as_str(), e.file_id.as_deref()))
            .collect();
        Some(local::resolve(&self.pair.path, &steps))
    }

    /// An action as its log line says it as it starts: what, and the path
    /// below the pair (a node the base doesn't know: by its parent's path and
    /// its name).
    fn said(&self, action: &Action) -> (&'static str, String) {
        let at = |node: &Uuid| {
            self.names(*node)
                .map_or_else(|| node.to_string(), |n| n.join("/"))
        };
        let under = |parent: &Uuid, name: &str| match self.names(*parent) {
            Some(n) if !n.is_empty() => format!("{}/{name}", n.join("/")),
            _ => name.to_owned(),
        };
        match action {
            Action::RemoteMkdir { path, .. } => ("make the folder on the server", path.join("/")),
            Action::RemoteMove { node, name, .. } => {
                ("move on the server", format!("{} to {name}", at(node)))
            }
            Action::Upload { path, .. } => ("upload", path.join("/")),
            Action::LocalMove { node, parent, name } => (
                "move here",
                format!("{} to {}", at(node), under(parent, name)),
            ),
            Action::LocalMkdir { parent, name, .. } => {
                ("make the folder here", under(parent, name))
            }
            Action::Download { parent, name, .. } => ("download", under(parent, name)),
            Action::Placeholder { parent, name, .. } => ("make a placeholder", under(parent, name)),
            Action::Merge { path, .. } => ("merge", path.join("/")),
            Action::Adopt { path, .. } => ("adopt", path.join("/")),
            Action::RemoteTrash { node } => ("trash on the server", at(node)),
            Action::LocalDelete { node } => ("delete here", at(node)),
            Action::KeepAside { node } => ("keep aside", at(node)),
            Action::Forget { node } => ("forget", at(node)),
        }
    }

    fn local(&self, names: &[String]) -> PathBuf {
        let steps: Vec<_> = names.iter().map(|n| (n.as_str(), None)).collect();
        local::resolve(&self.pair.path, &steps)
    }

    /// An item a first sync matched with a server node spelled otherwise
    /// (only by case: the key matched, and NFC isn't a difference) is renamed
    /// to the server's spelling: the folder joining a library takes its
    /// names. Otherwise the next scan would see a rename of its own, and send
    /// it to the server for every device. Never over another file (a variant
    /// beside it on ext4).
    fn spell_as_server(&self, names: &[String], name: &str) {
        let Some(own) = names.last() else { return };
        if Name::new(own).is_ok_and(|n| n.as_str() == name) {
            return;
        }
        let from = self.local(names);
        let to = from.with_file_name(name);
        // The same file under another case (a disk ignoring case): renamed
        // onto itself. Anything else there is never replaced.
        let same_file = local::file_id(&from).is_some_and(|f| local::file_id(&to) == Some(f));
        let renamed = if same_file {
            std::fs::rename(&from, &to)
        } else {
            local::rename_new(&from, &to)
        };
        if let Err(e) = renamed {
            self.report(
                &names.join("/"),
                &format!("not renamed to the library's {name:?}: {e}"),
                Level::Error,
            );
        }
    }

    fn parent(&self, parent: &Parent) -> Option<Uuid> {
        match parent {
            Parent::Node(p) => Some(*p),
            Parent::New(path) => self.created.get(path).copied(),
        }
    }

    fn report(&self, path: &str, message: &str, level: Level) {
        let level = match level {
            Level::Error => "error",
            Level::Info => "info",
        };
        let _ = self.state.notify(
            Some(self.pair.account),
            Some(self.pair.id),
            level,
            Some(path),
            message,
            None,
        );
    }

    fn fail(&mut self, path: &str, e: &Error) {
        if matches!(e, Error::Cancelled) {
            // The process stopping: nothing failed, the next start goes on.
            self.outcome.again = true;
        } else if e.transient() {
            // Retried, and said: a failure that never ends (a storage's
            // certificate gone bad, a server down) must show. Cleared with the
            // path's other notices once it goes through.
            self.outcome.again = true;
            self.report(path, &format!("trying again: {e}"), Level::Info);
        } else {
            self.report(path, &e.to_string(), Level::Error);
        }
    }

    /// A node synced: its entry, its path's notices gone, and the remote
    /// states it reconciles (`settled`), in one transaction.
    fn put(&mut self, e: Entry, settled: &[Uuid]) -> crate::state::Result<()> {
        let path = self.names(e.parent).map(|mut n| {
            n.push(e.name.clone());
            n.join("/")
        });
        self.state
            .synced(self.pair.id, &e, path.as_deref(), settled)?;
        self.base.insert(e.node, e);
        Ok(())
    }

    fn forget(&mut self, node: Uuid) -> crate::state::Result<()> {
        self.state.delete_entry(self.pair.id, node)?;
        let below: Vec<Uuid> = self
            .base
            .keys()
            .filter(|n| self.is_below(**n, node))
            .copied()
            .collect();
        for n in below {
            self.base.remove(&n);
        }
        self.base.remove(&node);
        Ok(())
    }

    fn is_below(&self, mut node: Uuid, folder: Uuid) -> bool {
        for _ in 0..10_000 {
            match self.base.get(&node) {
                Some(e) if e.parent == folder => return true,
                Some(e) => node = e.parent,
                None => return false,
            }
        }
        false
    }

    fn settle(&self, nodes: &[Uuid]) -> crate::state::Result<()> {
        self.state.clear_remote(self.pair.id, nodes)
    }

    pub async fn run(mut self, plan: Plan, scan: &Scan) -> crate::state::Result<Outcome> {
        self.settle(&plan.settled)?;
        for r in &plan.reports {
            self.report(&r.path, &r.message, r.level);
        }
        for link in &scan.links {
            self.report(link, "not synced: a symbolic link", Level::Info);
        }
        for path in &scan.elsewhere {
            self.report(path, "not synced: another disk", Level::Info);
        }
        for (path, why) in &scan.unreadable {
            self.report(path, &format!("not synced: {why}"), Level::Error);
        }
        for (path, problem) in &scan.ignore_problems {
            self.report(path, &format!("ignore file: {problem}"), Level::Error);
        }
        self.outcome.again |= plan.wait;
        // The plan is ordered: uploads, downloads and trashing come in runs,
        // each run one batch.
        let kind = |a: &Action| match a {
            Action::Upload { .. } | Action::Merge { .. } => 1,
            Action::Download { .. } => 2,
            Action::RemoteTrash { .. } => 3,
            _ => 0,
        };
        for action in &plan.actions {
            tracing::debug!(?action, "planned");
            if let Action::LocalMove { node, .. } = action {
                self.moving.insert(*node);
            }
        }
        let mut actions = plan.actions.into_iter().peekable();
        while let Some(first) = actions.next() {
            // Stopping: what is done is recorded; the rest waits for the next
            // start.
            if *self.stopping.borrow() {
                break;
            }
            let k = kind(&first);
            if k == 0 {
                // As it starts (a failure says so in its notice, logged too);
                // uploads, downloads and the trash say so once done.
                let (what, path) = self.said(&first);
                tracing::info!(path, "{what}");
                self.one(first).await?;
                continue;
            }
            let mut batch = vec![first];
            while let Some(next) = actions.next_if(|a| kind(a) == k) {
                batch.push(next);
            }
            match k {
                // A file stays open from its submission to its commit: so
                // many at a time, under macOS's default of 256 open files.
                1 => {
                    for part in batch.chunks(OPEN_UPLOADS) {
                        self.uploads(part.to_vec()).await?;
                    }
                }
                2 => self.downloads(batch).await?,
                _ => {
                    let nodes = batch
                        .into_iter()
                        .filter_map(|a| match a {
                            Action::RemoteTrash { node } => Some(node),
                            _ => None,
                        })
                        .collect();
                    self.trash(nodes).await?;
                }
            }
        }
        self.waiting_moves()?;
        // A new pair's root, known from its first round on. Never replaced: a
        // disk unmounted meanwhile would make the empty mount point the root,
        // and the next round would read it as everything deleted.
        if self.pair.root_file_id.is_none()
            && let Some(root) = local::root_id(&self.pair.path)
        {
            self.state
                .set_pair_path(self.pair.id, &self.pair.path, Some(&root))?;
        }
        Ok(self.outcome)
    }

    /// A remote move done locally; false: its place is taken by a file this
    /// round moves away, so it waits.
    fn local_move(&mut self, node: Uuid, parent: Uuid, name: &str) -> crate::state::Result<bool> {
        let from = self
            .stepped_aside
            .get(&node)
            .cloned()
            .or_else(|| self.path(node));
        let (Some(from), Some(mut to)) = (from, self.names(parent)) else {
            return Ok(true);
        };
        let name = name.to_owned();
        // The folder as spelled on disk, the new name exactly as given:
        // resolving it too would find the old spelling of a case-only
        // rename (`Report.txt` for `REPORT.txt`) on a case-sensitive
        // disk, and rename the file onto itself.
        let to_path = self.local(&to).join(&name);
        to.push(name.clone());
        // The same file (a case-only rename on a disk ignoring case) by
        // its ID, never by its name: on ext4, `report.txt` beside
        // `Report.txt` is another file.
        let same_file = local::file_id(&from).is_some_and(|f| local::file_id(&to_path) == Some(f));
        if to_path.symlink_metadata().is_ok() && !same_file {
            // Another file of this round's moves, still there: it waits.
            let occupant = local::file_id(&to_path);
            let moves = self.base.values().any(|e| {
                e.node != node
                    && e.file_id.is_some()
                    && e.file_id == occupant
                    && self.moving.contains(&e.node)
            });
            if moves {
                return Ok(false);
            }
            self.report(
                &to.join("/"),
                "not moved here: something local has this name",
                Level::Error,
            );
            return Ok(true);
        }
        if let Some(dir) = to_path.parent() {
            let _ = create_in(self.pair, dir);
        }
        // Never over a file put there since the check: the same file only (a
        // case-only rename on a disk ignoring case).
        let moved = if same_file {
            std::fs::rename(&from, &to_path)
        } else {
            local::rename_new(&from, &to_path)
        };
        match moved {
            Ok(()) => {
                crate::crash_point("moved-unrecorded");
                match self.base.get(&node).cloned() {
                    Some(mut e) => {
                        e.parent = parent;
                        e.name = name;
                        self.put(e, &[node])?;
                    }
                    None => self.settle(&[node])?,
                }
            }
            Err(e) => self.report(&to.join("/"), &format!("not moved: {e}"), Level::Error),
        }
        self.stepped_aside.remove(&node);
        Ok(true)
    }

    /// The moves that waited: again until none frees a place; a cycle left
    /// (a swap) is broken by one stepping aside under a temporary name.
    fn waiting_moves(&mut self) -> crate::state::Result<()> {
        while !self.waiting.is_empty() {
            let before = self.waiting.len();
            for (node, parent, name) in std::mem::take(&mut self.waiting) {
                if !self.local_move(node, parent, &name)? {
                    self.waiting.push((node, parent, name));
                }
            }
            if self.waiting.len() < before {
                continue;
            }
            let (node, _, _) = self.waiting[0].clone();
            let Some(from) = self.path(node) else { break };
            if self.stepped_aside.contains_key(&node) {
                break;
            }
            let mut temp = from.file_name().unwrap_or_default().to_owned();
            temp.push(".theca-swap");
            let temp = from.with_file_name(format!(".{}", temp.to_string_lossy()));
            // Never over a file of that name. ponytail: a crash before the
            // swap ends leaves the file under this name, which then goes up as
            // a rename (nothing lost).
            if temp.symlink_metadata().is_ok() || std::fs::rename(&from, &temp).is_err() {
                break;
            }
            self.stepped_aside.insert(node, temp);
        }
        for (node, parent, name) in std::mem::take(&mut self.waiting) {
            let _ = node;
            let to = self.names(parent).map_or(name.clone(), |mut n| {
                n.push(name);
                n.join("/")
            });
            self.report(
                &to,
                "not moved here: something local has this name",
                Level::Error,
            );
        }
        Ok(())
    }

    async fn one(&mut self, action: Action) -> crate::state::Result<()> {
        // What changes a drive's file, or a folder holding it, waits while the
        // mount serves it.
        let acted_on = match &action {
            Action::LocalMove { node, .. }
            | Action::KeepAside { node }
            | Action::LocalDelete { node }
            | Action::Placeholder { node, .. } => self.path(*node),
            _ => None,
        };
        let Ok(_taken) = acted_on.map_or(Ok(None), |path| self.take(&path)) else {
            return Ok(());
        };
        match action {
            Action::Adopt { node, path, state } => {
                // A folder, or a read-only library's file as downloaded.
                self.spell_as_server(&path, &state.name);
                let full = self.local(&path);
                let meta = std::fs::metadata(&full).ok().filter(|_| state.is_file);
                self.put(
                    Entry {
                        node,
                        parent: state.parent,
                        name: state.name,
                        is_file: state.is_file,
                        version: state.version.filter(|_| state.is_file),
                        size: meta.as_ref().map_or(0, |m| m.len() as i64),
                        mtime: meta.as_ref().map_or(0, local::mtime),
                        file_id: local::file_id(&full),
                    },
                    &[node],
                )?;
            }
            Action::RemoteMkdir { path, parent, name } => {
                let Some(parent) = self.parent(&parent) else {
                    return Ok(());
                };
                match self
                    .client
                    .create_folders(parent, std::slice::from_ref(&name))
                    .await
                {
                    Ok(created) if !created.is_empty() => {
                        let node = created[0].id;
                        self.created.insert(path.clone(), node);
                        let file_id = local::file_id(&self.local(&path));
                        self.outcome.touched.push(node);
                        self.put(
                            Entry {
                                node,
                                parent,
                                name,
                                is_file: false,
                                version: None,
                                size: 0,
                                mtime: 0,
                                file_id,
                            },
                            &[],
                        )?;
                    }
                    Ok(_) => {}
                    Err(e) => self.fail(&path.join("/"), &e),
                }
            }
            Action::RemoteMove { node, parent, name } => {
                let Some(parent) = self.parent(&parent) else {
                    return Ok(());
                };
                let Ok(valid) = Name::new(&name) else {
                    return Ok(());
                };
                let update = api::UpdateNode {
                    id: node,
                    name: Some(valid),
                    parent: Some(parent),
                    replace: false,
                };
                let display = self.names(parent).map_or(name.clone(), |mut n| {
                    n.push(name.clone());
                    n.join("/")
                });
                match self.client.update_nodes(&[update]).await {
                    Ok(mut results) => match results.pop() {
                        Some(Ok(_)) => {
                            if let Some(mut e) = self.base.get(&node).cloned() {
                                e.parent = parent;
                                e.name = name;
                                self.outcome.touched.push(node);
                                if let Some(path) = self.names(parent) {
                                    let mut path = path;
                                    path.push(e.name.clone());
                                    e.file_id = local::file_id(&self.local(&path)).or(e.file_id);
                                }
                                self.put(e, &[])?;
                            }
                        }
                        Some(Err(e)) => {
                            self.unmoved.insert(node);
                            self.fail(&display, &e);
                        }
                        None => {}
                    },
                    Err(e) => {
                        self.unmoved.insert(node);
                        self.fail(&display, &e);
                    }
                }
            }
            Action::LocalMove { node, .. } if self.unmoved.contains(&node) => {}
            Action::LocalMove { node, parent, name } => {
                if !self.local_move(node, parent, &name)? {
                    self.waiting.push((node, parent, name));
                }
            }
            Action::LocalMkdir { node, parent, name } => {
                let Some(mut names) = self.names(parent) else {
                    return Ok(());
                };
                names.push(name.clone());
                let path = self.local(&names);
                match create_in(self.pair, &path) {
                    Ok(()) => {
                        self.put(
                            Entry {
                                node,
                                parent,
                                name,
                                is_file: false,
                                version: None,
                                size: 0,
                                mtime: 0,
                                file_id: local::file_id(&path),
                            },
                            &[node],
                        )?;
                    }
                    Err(e) => {
                        self.report(&names.join("/"), &format!("not created: {e}"), Level::Error)
                    }
                }
            }
            Action::KeepAside { node } => {
                if let Some(path) = self.path(node) {
                    let aside = aside(&path);
                    if let Err(e) = local::rename_new(&path, &aside) {
                        self.report(
                            &path.to_string_lossy(),
                            &format!("not kept aside: {e}"),
                            Level::Error,
                        );
                    } else {
                        self.report(
                            &aside.to_string_lossy(),
                            "kept aside: this library is read-only and the file changed on the server",
                            Level::Info,
                        );
                    }
                }
            }
            Action::LocalDelete { node } => {
                let (Some(path), Some(e)) = (self.path(node), self.base.get(&node).cloned()) else {
                    return Ok(());
                };
                let removed = if e.is_file {
                    match std::fs::symlink_metadata(&path) {
                        // Still the synced file: deleted.
                        Ok(_) if local::unchanged(&path, e.size, e.mtime, e.file_id.as_deref()) => {
                            std::fs::remove_file(&path).is_ok()
                        }
                        Ok(_) => false,
                        // Already gone, unless the whole folder is (a disk
                        // unmounted mid-round: the file is still on it).
                        Err(_) => pair_there(self.pair),
                    }
                } else {
                    // What the scan skips (`.DS_Store`, a stale part) was
                    // never anybody's: it goes first. Then only once empty:
                    // anything else left in it wasn't synced.
                    for entry in std::fs::read_dir(&path).into_iter().flatten().flatten() {
                        let skipped = entry.file_name().to_str().is_some_and(local::skipped);
                        if skipped && entry.file_type().is_ok_and(|t| t.is_file()) {
                            let _ = std::fs::remove_file(entry.path());
                        }
                    }
                    (!path.exists() && pair_there(self.pair)) || std::fs::remove_dir(&path).is_ok()
                };
                if removed {
                    crate::crash_point("deleted-unrecorded");
                    self.forget(node)?;
                    self.settle(&[node])?;
                } else if !e.is_file && path.exists() {
                    // What the scan doesn't sync (a link, another disk) keeps
                    // it: said, not retried in silence.
                    let display = self.names(node).map(|n| n.join("/")).unwrap_or_default();
                    self.report(
                        &display,
                        "not deleted: it holds what isn't synced",
                        Level::Info,
                    );
                }
            }
            Action::Forget { node } => {
                self.forget(node)?;
                self.settle(&[node])?;
            }
            Action::Upload { .. }
            | Action::Merge { .. }
            | Action::Download { .. }
            | Action::RemoteTrash { .. } => unreachable!("batched"),
            #[cfg(any(target_os = "linux", windows))]
            Action::Placeholder {
                node,
                parent,
                name,
                state,
            } => {
                let Some(sink) = self.landing(node, parent, name, &state) else {
                    return Ok(());
                };
                let size = u64::try_from(state.size).unwrap_or_default();
                match sink.placeholder(size) {
                    Ok(()) => {
                        if let Some(e) = sink.done.lock().ok().and_then(|d| d.clone()) {
                            self.base.insert(e.node, e);
                        }
                    }
                    // Changed locally meanwhile: the local edit wins, next round.
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        self.outcome.again = true;
                    }
                    Err(e) => self.fail(&sink.target.to_string_lossy(), &Error::Io(e)),
                }
            }
            // macOS has no plan: no other pair is a drive.
            #[cfg(not(any(target_os = "linux", windows)))]
            Action::Placeholder { .. } => unreachable!("no drive here"),
        }
        Ok(())
    }

    /// Uploads and merges, in one engine run.
    async fn uploads(&mut self, actions: Vec<Action>) -> crate::state::Result<()> {
        if actions.is_empty() {
            return Ok(());
        }
        // (source, target, local names, fingerprint, the entry's place, its
        // node, a merge)
        let mut jobs = Vec::new();
        for action in actions {
            let (names, target, place, node, merge) = match action {
                Action::Upload { path, to } => match to {
                    UploadTo::New { parent, name } => {
                        let Some(p) = self.parent(&parent) else {
                            continue;
                        };
                        let Ok(valid) = Name::new(&name) else {
                            continue;
                        };
                        let target = Target::New {
                            library: self.pair.library,
                            parent: p,
                            name: valid,
                        };
                        (path, target, (p, name), None, false)
                    }
                    UploadTo::Version { node, base } => {
                        // A drive's file fetched since this round read its
                        // base (fetches land beside rounds): no edit.
                        if self.pair.mode == crate::state::Mode::Drive
                            && let Some(now) = self.state.entry(self.pair.id, node)?
                            && local::unchanged(
                                &self.local(&path),
                                now.size,
                                now.mtime,
                                now.file_id.as_deref(),
                            )
                        {
                            self.base.insert(node, now);
                            continue;
                        }
                        let Some(e) = self.base.get(&node) else {
                            continue;
                        };
                        let place = (e.parent, e.name.clone());
                        let target = Target::Version {
                            library: self.pair.library,
                            file: node,
                            base,
                        };
                        (path, target, place, Some(node), false)
                    }
                },
                Action::Merge { node, path, state } => {
                    let target = Target::Version {
                        library: self.pair.library,
                        file: node,
                        base: state.version,
                    };
                    (path, target, (state.parent, state.name), Some(node), true)
                }
                _ => continue,
            };
            let full = self.local(&names);
            let meta = match std::fs::metadata(&full) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            if meta
                .modified()
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .is_some_and(|age| age < SETTLING)
            {
                // Still being written: next round.
                self.outcome.again = true;
                continue;
            }
            let source = match FileSource::open(&full) {
                Ok(source) => source,
                Err(e) => {
                    self.report(
                        &names.join("/"),
                        &format!("not uploaded: {e}"),
                        Level::Error,
                    );
                    self.outcome.again = true;
                    continue;
                }
            };
            let fingerprint = (
                meta.len() as i64,
                local::mtime(&meta),
                local::file_id(&full),
            );
            jobs.push((source, target, names, fingerprint, place, node, merge));
        }
        let options = UploadOptions::default();
        let cancel = self.cancel;
        let client = self.client;
        let (uploader, run) = Uploader::new(client, &options, cancel, |_| {});
        let mut submitted: futures::stream::FuturesUnordered<_> = jobs
            .iter()
            .enumerate()
            .map(|(i, (source, target, ..))| {
                let committed = uploader.submit(source, target.clone());
                async move { (i, committed.await) }
            })
            .collect();
        drop(uploader);
        // Each recorded as it's committed, never at the end of the batch, which
        // a crash or a quit may not reach.
        let record = async {
            use futures::StreamExt;
            while let Some((i, result)) = submitted.next().await {
                let (_, _, names, fingerprint, place, node, merge) = &jobs[i];
                if *merge && result.is_ok() {
                    self.spell_as_server(names, &place.1);
                }
                self.uploaded(result, names, fingerprint.clone(), place.clone(), *node)?;
            }
            Ok::<_, crate::state::StateError>(())
        };
        let ((), recorded) = futures::join!(run, record);
        recorded
    }

    fn uploaded(
        &mut self,
        result: Result<api::Committed, Error>,
        names: &[String],
        (size, mtime, file_id): (i64, i64, Option<String>),
        (parent, name): (Uuid, String),
        node: Option<Uuid>,
    ) -> crate::state::Result<()> {
        match result {
            Ok(committed) => {
                tracing::info!(
                    path = names.join("/"),
                    unchanged = committed.unchanged,
                    "uploaded"
                );
                crate::crash_point("uploaded-unrecorded");
                self.outcome.uploaded += usize::from(!committed.unchanged);
                self.outcome.touched.push(committed.file);
                // Unchanged: the content was already a version's. When that's
                // a base no longer current (a stale copy only touched), the
                // pending remote state stays, and the next round downloads it.
                // Nothing on the server woke for it either: a round soon.
                // ponytail: also after a plain touch (a round with nothing to
                // do); the answer doesn't say which.
                let settled = if committed.unchanged {
                    self.outcome.again = true;
                    &[][..]
                } else {
                    node.as_slice()
                };
                self.put(
                    Entry {
                        node: committed.file,
                        parent,
                        name,
                        is_file: true,
                        version: Some(committed.version),
                        size,
                        mtime,
                        file_id,
                    },
                    settled,
                )?;
                // After `put`, which clears the path's notices.
                if committed.conflict {
                    self.report(
                        &names.join("/"),
                        "also changed elsewhere: yours is current, the other is in its versions",
                        Level::Info,
                    );
                }
            }
            // Changed while read: next round.
            Err(Error::SourceChanged) => self.outcome.again = true,
            Err(e) => self.fail(&names.join("/"), &e),
        }
        Ok(())
    }

    /// Where a remote file's version lands, and what it records: its sink.
    /// None: it doesn't land this round (its local move refused, something
    /// else local has its name).
    fn landing(
        &self,
        node: Uuid,
        parent: Uuid,
        name: String,
        state: &RemoteState,
    ) -> Option<SyncSink<'a>> {
        if self.unmoved.contains(&node) {
            return None;
        }
        let mut names = self.names(parent)?;
        // Where it is locally now (a known node), else where it goes: in
        // its folder found by file ID too.
        let target = match self.base.get(&node) {
            Some(_) => self.path(node),
            None => self.path(parent).map(|folder| folder.join(&name)),
        }
        .unwrap_or_else(|| {
            names.push(name.clone());
            self.local(&names)
        });
        let expected = self
            .base
            .get(&node)
            .filter(|e| e.is_file)
            .map(|e| (e.size, e.mtime, e.file_id.clone()));
        // A new file where something local already has the name: never
        // replaced, so not fetched either (it would be, every few seconds).
        // A drive's placeholder holds nothing: replaced.
        if expected.is_none()
            && target.symlink_metadata().is_ok()
            && !is_placeholder(self.pair, &target)
        {
            let below = target.strip_prefix(&self.pair.path).unwrap_or(&target);
            let display: Vec<_> = below.iter().map(|n| n.to_string_lossy()).collect();
            self.report(
                &display.join("/"),
                "not downloaded: something local has this name",
                Level::Error,
            );
            return None;
        }
        let place = self.base.get(&node).map(|e| (e.parent, e.name.clone()));
        // Where the server has it now, unless the base has it elsewhere (a
        // remote move not done locally yet): then its state stays pending.
        let settle = place
            .as_ref()
            .is_none_or(|(p, n)| *p == state.parent && *n == state.name);
        let (parent, name) = place.unwrap_or((parent, name));
        let path = self.names(parent).map(|mut n| {
            n.push(name.clone());
            n.join("/")
        });
        let entry = Entry {
            node,
            parent,
            name,
            is_file: true,
            version: state.version,
            size: 0,
            mtime: 0,
            file_id: None,
        };
        let record = Record {
            state: self.state,
            pair: self.pair.id,
            entry,
            path,
            settle,
        };
        Some(SyncSink::new(
            self.pair,
            record,
            &target,
            expected,
            state.modified,
        ))
    }

    /// A Windows drive's placeholder downloaded in place: brought to the
    /// version first if it changed (recorded), then hydrated (Windows asks
    /// this process for the data, as for an open; never taken here, or that
    /// fetch would wait for it).
    #[cfg(windows)]
    async fn hydrate(&mut self, sink: SyncSink<'a>, state: &RemoteState) {
        let target = sink.target.clone();
        let current = self.base.get(&sink.record.entry.node).map(|e| e.version);
        if current != Some(state.version) {
            let size = u64::try_from(state.size).unwrap_or_default();
            if let Err(e) = sink.placeholder(size) {
                self.fail(&target.to_string_lossy(), &Error::Io(e));
                return;
            }
            if let Some(e) = sink.done.lock().ok().and_then(|d| d.clone()) {
                self.base.insert(e.node, e);
            }
        }
        let path = target.clone();
        match tokio::task::spawn_blocking(move || crate::drive::cloud::hydrate(&path)).await {
            Ok(Ok(())) => self.outcome.downloaded += 1,
            Ok(Err(e)) => self.fail(&target.to_string_lossy(), &Error::Io(e)),
            Err(_) => self.outcome.again = true,
        }
    }

    async fn downloads(&mut self, actions: Vec<Action>) -> crate::state::Result<()> {
        if actions.is_empty() {
            return Ok(());
        }
        let mut jobs = Vec::new();
        for action in actions {
            let Action::Download {
                node,
                parent,
                name,
                state,
            } = action
            else {
                continue;
            };
            let Some(sink) = self.landing(node, parent, name, &state) else {
                continue;
            };
            // A Windows drive's placeholder (kept): hydrated in place, so it
            // stays one Windows can free later.
            #[cfg(windows)]
            if self.pair.mode == crate::state::Mode::Drive
                && crate::drive::cloud::is_placeholder(&sink.target)
            {
                self.hydrate(sink, &state).await;
                continue;
            }
            if let Ok(taken) = self.take(&sink.target.clone()) {
                jobs.push((sink, node, state, taken));
            }
        }
        let options = DownloadOptions::default();
        let cancel = self.cancel;
        let (downloader, run) = Downloader::new(self.client, &options, cancel, |_| {});
        let fetched: Vec<_> = jobs
            .iter()
            .map(|(sink, node, state, _)| downloader.submit(*node, state.version, sink))
            .collect();
        drop(downloader);
        let (_, results) = futures::join!(run, futures::future::join_all(fetched));
        for (result, (sink, _, _, _)) in results.into_iter().zip(jobs) {
            let display = sink.target.to_string_lossy().into_owned();
            match result {
                // Recorded by the sink as it landed.
                Ok(_) => {
                    let Some(e) = sink.done.lock().ok().and_then(|d| d.clone()) else {
                        continue;
                    };
                    self.outcome.downloaded += 1;
                    self.base.insert(e.node, e);
                }
                Err(Error::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists => {
                    // Changed locally meanwhile: the local edit wins, next round.
                    self.outcome.again = true;
                }
                // Gone since: the feed says so next.
                Err(Error::Api { status: 404, .. }) => {}
                Err(e) => self.fail(&display, &e),
            }
        }
        Ok(())
    }

    /// Local deletes to the server's trash: only the topmost (the server
    /// takes a folder's content with it).
    async fn trash(&mut self, nodes: Vec<Uuid>) -> crate::state::Result<()> {
        let set: HashSet<Uuid> = nodes.iter().copied().collect();
        let top: Vec<Uuid> = nodes
            .iter()
            .filter(|n| !set.iter().any(|f| *f != **n && self.is_below(**n, *f)))
            .copied()
            .collect();
        if top.is_empty() {
            return Ok(());
        }
        for batch in top.chunks(1000) {
            match self.client.trash_nodes(batch, false).await {
                Ok(results) => {
                    for (node, result) in batch.iter().zip(results) {
                        match result {
                            // Already gone: as good.
                            Ok(()) | Err(Error::Api { status: 404, .. }) => {
                                let path = self.names(*node).unwrap_or_default().join("/");
                                tracing::info!(path, "trashed on the server");
                                crate::crash_point("trashed-unforgotten");
                                self.outcome.trashed.push(*node);
                                self.outcome.touched.push(*node);
                                self.forget(*node)?;
                                self.settle(&[*node])?;
                            }
                            Err(e) => {
                                let path = self.names(*node).unwrap_or_default().join("/");
                                self.fail(&path, &e);
                            }
                        }
                    }
                }
                Err(e) => self.fail(&self.pair.library_name.clone(), &e),
            }
        }
        Ok(())
    }
}

/// A drive's file fetched now, beside rounds (opened through the mount as a
/// placeholder): its entry's version lands as a download does, only over
/// the placeholder, its mtime kept, and is recorded. The caller holds the
/// file (`Busy`), so no round acts on it meanwhile.
pub async fn fetch(
    client: &Client,
    state: &State,
    pair: &Pair,
    entry: Entry,
    target: &Path,
    cancel: &Cancel,
) -> Result<Entry, Error> {
    let expected = Some((entry.size, entry.mtime, entry.file_id.clone()));
    let modified = Some(DateTime::<Utc>::from_timestamp_nanos(entry.mtime));
    let (node, version) = (entry.node, entry.version);
    let record = Record {
        state,
        pair: pair.id,
        entry,
        path: None,
        settle: false,
    };
    let sink = SyncSink::new(pair, record, target, expected, modified);
    let options = DownloadOptions::default();
    let (downloader, run) = Downloader::new(client, &options, cancel, |_| {});
    let fetched = downloader.submit(node, version, &sink);
    drop(downloader);
    let (_, result) = futures::join!(run, fetched);
    result?;
    sink.done
        .lock()
        .ok()
        .and_then(|d| d.clone())
        .ok_or_else(|| Error::Io(io::Error::other("fetched, not recorded")))
}

/// A drive's placeholder, which holds nothing to lose (Linux; Windows' come
/// with its drive).
fn is_placeholder(pair: &Pair, path: &Path) -> bool {
    #[cfg(any(target_os = "linux", windows))]
    return pair.mode == crate::state::Mode::Drive && crate::drive::is_placeholder(path);
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (pair, path);
        false
    }
}

/// The pair's folder is still the one it synced (a disk unmounted mid-round
/// leaves an empty mount point, or nothing).
fn pair_there(pair: &Pair) -> bool {
    let now = local::root_id(&pair.path);
    now.is_some() && (pair.root_file_id.is_none() || now == pair.root_file_id)
}

/// Creates a folder (and its parents) inside the pair's, only while that one
/// is still there: `create_dir_all` would make it again on whatever disk is
/// below, and what lands there would be missing once the disk is back.
fn create_in(pair: &Pair, dir: &Path) -> io::Result<()> {
    if !pair_there(pair) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the synced folder is gone",
        ));
    }
    std::fs::create_dir_all(dir)
}

/// `report.docx` → `report (local).docx`, or the first free of
/// `report (local 2).docx`…: an earlier one may be the only copy of an edit (a
/// read-only library never uploads it).
fn aside(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    (1..)
        .map(|n| {
            let local = if n == 1 {
                "local".into()
            } else {
                format!("local {n}")
            };
            let name = match path.extension() {
                Some(ext) => format!("{stem} ({local}).{}", ext.to_string_lossy()),
                None => format!("{stem} ({local})"),
            };
            path.with_file_name(name)
        })
        .find(|p| p.symlink_metadata().is_err())
        .expect("a free name")
}

/// A download written as `.<name>.<random>.theca-part`, then moved into place
/// only over the base's file (or nothing, for a new file): a local edit made
/// meanwhile is never overwritten (`place`).
struct SyncSink<'a> {
    /// The pair's folder and file ID: never recreated if it's gone.
    pair: Pair,
    /// Its entry, written to the state as soon as the file is in place: never
    /// at the end of the batch, which a crash or a quit may not reach.
    record: Record<'a>,
    target: PathBuf,
    part: PathBuf,
    /// The fingerprint (size, mtime, file ID) of the base's file the target
    /// may replace; none for a new file.
    expected: Option<(i64, i64, Option<String>)>,
    modified: Option<DateTime<Utc>>,
    file: Mutex<Option<std::fs::File>>,
    /// What was renamed into place (size, mtime, file ID), taken from the part
    /// before the rename: the target itself may be edited a moment later, and
    /// that edit must look like one.
    done: Mutex<Option<Entry>>,
    /// Tests: runs where a save could land unseen, before the download is in
    /// place.
    #[cfg(test)]
    meanwhile: Option<Box<dyn Fn() + Send + Sync>>,
}

/// What a download records once in place: its entry (the fingerprint filled
/// then), clearing its path's notices and its pending remote state.
struct Record<'a> {
    state: &'a State,
    pair: i64,
    entry: Entry,
    path: Option<String>,
    /// Its pending remote state settled with it.
    settle: bool,
}

impl<'a> SyncSink<'a> {
    fn new(
        pair: &Pair,
        record: Record<'a>,
        target: &Path,
        expected: Option<(i64, i64, Option<String>)>,
        modified: Option<DateTime<Utc>>,
    ) -> Self {
        // A name nobody picks (hidden, random), created new: a user's own
        // `x.txt.theca-part` is never truncated, a stale part never reused.
        // ponytail: a part a crash left stays (skipped by the scan) until its
        // folder is deleted.
        use std::hash::BuildHasher;
        let random = std::collections::hash_map::RandomState::new().hash_one(target);
        let name = target.file_name().unwrap_or_default().to_string_lossy();
        let part = format!(".{name}.{:08x}{PART_SUFFIX}", random as u32);
        Self {
            pair: pair.clone(),
            record,
            target: target.to_owned(),
            part: target.with_file_name(part),
            expected,
            modified,
            file: Mutex::new(None),
            done: Mutex::new(None),
            #[cfg(test)]
            meanwhile: None,
        }
    }

    /// The part moved into place, never over anything but the base's file.
    /// The target is moved aside first, atomically: whatever was saved until
    /// then moves with it. Still the base's file, the part is renamed in
    /// without replacing, and the aside file deleted (the server has it).
    /// Edited, it goes back: the local edit wins, next round. A save
    /// creating the target meanwhile makes the rename fail: it wins too.
    /// Nothing is ever overwritten; a crash in between leaves the old file
    /// as a visible `(local)` copy, never a hidden one.
    fn place(&self) -> io::Result<()> {
        let changed = || {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "changed locally during the download",
            )
        };
        // A drive (FUSE): the part and the target exchanged in one step,
        // then the target checked. Moved aside, it showed as `<name> (local)`
        // for a moment, and a round scanning beside the fetch took it for a
        // rename and renamed the file on the server. Where the file system
        // can't exchange, as below.
        #[cfg(target_os = "linux")]
        if self.pair.mode == crate::state::Mode::Drive {
            #[cfg(test)]
            if let Some(meanwhile) = &self.meanwhile {
                meanwhile();
            }
            if let Some(swapped) = self.swap_in() {
                return swapped;
            }
        }
        // A new file: nothing may be replaced, but a drive's placeholder.
        if self.expected.is_none() && !is_placeholder(&self.pair, &self.target) {
            return local::rename_new(&self.part, &self.target).map_err(|_| changed());
        }
        let aside = aside(&self.target);
        match local::rename_new(&self.target, &aside) {
            Ok(()) => {}
            // Deleted meanwhile: in place as a new file.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return local::rename_new(&self.part, &self.target).map_err(|_| changed());
            }
            Err(e) => return Err(e),
        }
        #[cfg(test)]
        if let Some(meanwhile) = &self.meanwhile {
            meanwhile();
        }
        let unchanged = is_placeholder(&self.pair, &aside)
            || self.expected.as_ref().is_some_and(|(size, mtime, id)| {
                local::unchanged(&aside, *size, *mtime, id.as_deref())
            });
        if !unchanged {
            // Back where it was (unless a save took the name meanwhile: then
            // both stay, the old one as a `(local)` copy).
            let _ = local::rename_new(&aside, &self.target);
            return Err(changed());
        }
        match local::rename_new(&self.part, &self.target) {
            Ok(()) => {
                let _ = std::fs::remove_file(&aside);
                Ok(())
            }
            // Saved meanwhile under its name: the save wins; the base's
            // content, which the server has, goes.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(&aside);
                Err(changed())
            }
            // Anything else: the old file back in its place.
            Err(e) => {
                let _ = local::rename_new(&aside, &self.target);
                Err(e)
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl SyncSink<'_> {
    /// The part and the target exchanged atomically (the file is always
    /// there, under its name); what was there, now at the part's hidden
    /// name, removed if it may be replaced, else swapped back. `None`: not
    /// exchanged (no target, or a file system without it).
    fn swap_in(&self) -> Option<io::Result<()>> {
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        let exchange = || renameat_with(CWD, &self.part, CWD, &self.target, RenameFlags::EXCHANGE);
        exchange().ok()?;
        if self.replaceable(&self.part) {
            let _ = std::fs::remove_file(&self.part);
            return Some(Ok(()));
        }
        // Back: what is there stays, the download is dropped. Not back: kept
        // beside, visible, never left under the part's hidden name.
        if exchange().is_err() {
            let _ = local::rename_new(&self.part, &aside(&self.target));
        }
        let _ = std::fs::remove_file(&self.part);
        Some(Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "changed locally during the download",
        )))
    }

    /// What a download may replace: the base's file as synced, or a
    /// placeholder, the one it was for (a placeholder renamed over it
    /// meanwhile is another file: it stays).
    fn replaceable(&self, old: &Path) -> bool {
        match &self.expected {
            Some((size, mtime, id)) => {
                local::unchanged(old, *size, *mtime, id.as_deref())
                    || (is_placeholder(&self.pair, old)
                        && id
                            .as_deref()
                            .is_none_or(|id| local::file_id(old).as_deref() == Some(id)))
            }
            None => is_placeholder(&self.pair, old),
        }
    }
}

impl SyncSink<'_> {
    /// What landed recorded: its entry, its path's notices cleared, its
    /// remote state settled.
    fn recorded(&self, entry: Entry) -> io::Result<()> {
        let r = &self.record;
        let settled = if r.settle { &[entry.node][..] } else { &[] };
        r.state
            .synced(r.pair, &entry, r.path.as_deref(), settled)
            .map_err(io::Error::other)?;
        crate::crash_point("downloaded");
        let path = self
            .target
            .strip_prefix(&self.pair.path)
            .unwrap_or(&self.target);
        tracing::info!(path = %path.display(), "downloaded");
        *self.done.lock().map_err(|_| io::Error::other("poisoned"))? = Some(entry);
        Ok(())
    }

    /// A placeholder of the version, its content left on the server (a
    /// Windows drive): over a downloaded file only if it holds the base's
    /// content (an eviction), over a placeholder whatever (it holds nothing);
    /// recorded as a download is.
    #[cfg(windows)]
    fn placeholder(&self, size: u64) -> io::Result<()> {
        use crate::drive::cloud;
        if self.target.symlink_metadata().is_ok() && !cloud::is_placeholder(&self.target) {
            let unchanged = self
                .expected
                .as_ref()
                .is_some_and(|(s, m, id)| local::unchanged(&self.target, *s, *m, id.as_deref()));
            if !unchanged {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "changed locally since it was synced",
                ));
            }
        }
        if let Some(dir) = self.target.parent() {
            create_in(&self.pair, dir)?;
        }
        let modified = self.modified.map(SystemTime::from);
        cloud::placeholder(&self.target, self.record.entry.node, size, modified)?;
        let meta = std::fs::metadata(&self.target)?;
        let mut entry = self.record.entry.clone();
        entry.size = meta.len() as i64;
        entry.mtime = local::mtime(&meta);
        entry.file_id = local::file_id(&self.target);
        self.recorded(entry)
    }

    /// A placeholder of the version, its content left on the server (a
    /// drive): landed and recorded as a download is.
    #[cfg(target_os = "linux")]
    fn placeholder(&self, size: u64) -> io::Result<()> {
        if let Some(dir) = self.part.parent() {
            create_in(&self.pair, dir)?;
        }
        let file = std::fs::File::create_new(&self.part)?;
        crate::drive::placeholder::write(&file, size)?;
        *self.file.lock().map_err(|_| io::Error::other("poisoned"))? = Some(file);
        self.finish()
    }
}

impl Sink for SyncSink<'_> {
    fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = self.file.lock().map_err(|_| io::Error::other("poisoned"))?;
        if file.is_none() {
            if let Some(dir) = self.part.parent() {
                create_in(&self.pair, dir)?;
            }
            *file = Some(std::fs::File::create_new(&self.part)?);
        }
        let file = file.as_mut().expect("opened above");
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)
    }

    fn finish(&self) -> io::Result<()> {
        use std::io::Write;
        let mut file = self.file.lock().map_err(|_| io::Error::other("poisoned"))?;
        let file = match file.take() {
            Some(mut file) => {
                file.flush()?;
                file
            }
            None => {
                if let Some(dir) = self.part.parent() {
                    create_in(&self.pair, dir)?;
                }
                std::fs::File::create_new(&self.part)?
            }
        };
        if let Some(modified) = self.modified {
            let _ = file.set_modified(SystemTime::from(modified));
        }
        // On the disk before the state says synced: after a power loss, an
        // empty file in its place would go up as the current version.
        file.sync_all()?;
        drop(file);
        // From the closed part (a rename keeps all three), not from the target
        // once the batch ends: by then the user may have edited it.
        let meta = std::fs::metadata(&self.part)?;
        let mut entry = self.record.entry.clone();
        entry.size = meta.len() as i64;
        entry.mtime = local::mtime(&meta);
        entry.file_id = local::file_id(&self.part);
        if let Err(e) = self.place() {
            let _ = std::fs::remove_file(&self.part);
            return Err(e);
        }
        // The rename too (Windows has no handle on a folder to flush).
        #[cfg(unix)]
        if let Some(dir) = self.target.parent() {
            std::fs::File::open(dir)?.sync_all()?;
        }
        crate::crash_point("downloaded-unrecorded");
        self.recorded(entry)
    }

    fn abort(&self) {
        if let Ok(mut file) = self.file.lock()
            && file.take().is_some()
        {
            let _ = std::fs::remove_file(&self.part);
        }
    }
}

/// A remote state from a feed entry or a tree line.
pub fn remote_state(
    parent: Uuid,
    name: String,
    is_file: bool,
    version: Option<Uuid>,
    size: Option<i64>,
    modified: Option<DateTime<Utc>>,
) -> RemoteState {
    RemoteState {
        parent,
        name,
        is_file,
        version,
        size: size.unwrap_or_default(),
        modified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(state: &State) -> Record<'_> {
        Record {
            state,
            pair: 1,
            entry: Entry {
                node: Uuid::nil(),
                parent: Uuid::nil(),
                name: "x.txt".into(),
                is_file: true,
                version: None,
                size: 0,
                mtime: 0,
                file_id: None,
            },
            path: None,
            settle: true,
        }
    }

    fn pair(path: &Path) -> Pair {
        Pair {
            id: 1,
            account: 1,
            library: Uuid::nil(),
            root: Uuid::nil(),
            library_name: "Docs".into(),
            path: path.to_owned(),
            root_file_id: local::root_id(path),
            position: Some(1),
            position_event: None,
            paused: false,
            stop: None,
            writable: true,
            activity: String::new(),
            error: None,
            synced_at: None,
            mode: crate::state::Mode::Sync,
        }
    }

    /// B4: the pair's folder gone mid-round (a disk unmounted): a download
    /// never recreates it, on whatever disk is below.
    #[test]
    fn a_download_never_recreates_a_vanished_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("pair");
        std::fs::create_dir(&root).unwrap();
        let state = State::memory().unwrap();
        let sink = SyncSink::new(
            &pair(&root),
            record(&state),
            &root.join("sub/x.txt"),
            None,
            None,
        );
        std::fs::remove_dir(&root).unwrap();
        let written = sink.write_at(0, b"x").and_then(|()| sink.finish());
        assert!(written.is_err());
        assert!(!root.exists());
    }

    /// A user's own `x.txt.theca-part` is never a download's part file.
    #[test]
    fn a_part_file_is_never_a_users_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x.txt");
        let mine = dir.path().join("x.txt.theca-part");
        std::fs::write(&mine, "mine").unwrap();
        let state = State::memory().unwrap();
        let sink = SyncSink::new(&pair(dir.path()), record(&state), &target, None, None);
        sink.write_at(0, b"downloaded").unwrap();
        sink.finish().unwrap();
        assert_eq!(std::fs::read_to_string(&mine).unwrap(), "mine");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "downloaded");
    }

    /// A4: an edit made just after a download lands is not what was synced.
    #[test]
    fn a_download_records_what_it_renamed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x.txt");
        let state = State::memory().unwrap();
        let sink = SyncSink::new(&pair(dir.path()), record(&state), &target, None, None);
        sink.write_at(0, b"downloaded").unwrap();
        sink.finish().unwrap();
        std::fs::write(&target, "edited right after, longer").unwrap();
        let e = sink.done.lock().unwrap().clone().unwrap();
        assert_eq!(e.size, 10);
        assert_eq!(e.file_id, local::file_id(&target), "the same file, renamed");
    }

    /// A save landing while a download moves into place, in place or by an
    /// atomic save, is never overwritten.
    #[test]
    fn a_save_while_a_download_lands_is_never_lost() {
        for atomic in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("x.txt");
            std::fs::write(&target, "synced").unwrap();
            let meta = std::fs::metadata(&target).unwrap();
            let expected = (
                meta.len() as i64,
                local::mtime(&meta),
                local::file_id(&target),
            );
            let state = State::memory().unwrap();
            let mut sink = SyncSink::new(
                &pair(dir.path()),
                record(&state),
                &target,
                Some(expected),
                None,
            );
            let (at, folder) = (target.clone(), dir.path().to_owned());
            sink.meanwhile = Some(Box::new(move || {
                if atomic {
                    let saved = folder.join(".x.txt.tmp");
                    std::fs::write(&saved, "saved meanwhile").unwrap();
                    std::fs::rename(&saved, &at).unwrap();
                } else {
                    std::fs::write(&at, "saved meanwhile").unwrap();
                }
            }));
            sink.write_at(0, b"downloaded").unwrap();
            let _ = sink.finish();
            let contents: Vec<String> = std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                .collect();
            assert!(
                contents.iter().any(|c| c == "saved meanwhile"),
                "atomic: {atomic}, {contents:?}"
            );
        }
    }

    /// A drive's file fetched over its placeholder is there under its name
    /// at every moment: a scan never sees it gone, nor under another name
    /// (the `(local)` name it once stepped aside to read as a rename, and
    /// the round renamed it on the server).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_fetched_placeholder_is_never_seen_elsewhere() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x.txt");
        let state = State::memory().unwrap();
        let done = AtomicBool::new(false);
        std::thread::scope(|s| {
            let seen = s.spawn(|| {
                let mut wrong = Vec::new();
                while !done.load(Ordering::Acquire) {
                    let names: Vec<String> = std::fs::read_dir(dir.path())
                        .unwrap()
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .filter(|n| !local::skipped(n))
                        .collect();
                    if names != ["x.txt"] {
                        wrong.push(names);
                    }
                }
                wrong
            });
            for _ in 0..300 {
                let file = std::fs::File::create(&target).unwrap();
                crate::drive::placeholder::write(&file, 10).unwrap();
                drop(file);
                let sink = SyncSink::new(&drive(dir.path()), record(&state), &target, None, None);
                sink.write_at(0, b"downloaded").unwrap();
                sink.finish().unwrap();
                assert_eq!(std::fs::read_to_string(&target).unwrap(), "downloaded");
            }
            done.store(true, Ordering::Release);
            let wrong = seen.join().unwrap();
            assert!(wrong.is_empty(), "seen: {:?}", &wrong[..wrong.len().min(3)]);
        });
    }

    /// Another placeholder renamed over the one being fetched (FUSE's rename
    /// holds only its source): it isn't replaced; the fetch fails, the
    /// round follows the rename.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_fetch_never_lands_over_another_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.txt");
        let file = std::fs::File::create(&target).unwrap();
        crate::drive::placeholder::write(&file, 10).unwrap();
        drop(file);
        let expected = fingerprint(&target);
        let state = State::memory().unwrap();
        let mut sink = SyncSink::new(
            &drive(dir.path()),
            record(&state),
            &target,
            Some(expected),
            None,
        );
        let (at, other) = (target.clone(), dir.path().join("c.txt"));
        sink.meanwhile = Some(Box::new(move || {
            let file = std::fs::File::create(&other).unwrap();
            crate::drive::placeholder::write(&file, 3).unwrap();
            drop(file);
            std::fs::rename(&other, &at).unwrap();
        }));
        sink.write_at(0, b"downloaded").unwrap();
        assert!(sink.finish().is_err());
        assert!(
            crate::drive::is_placeholder(&target),
            "c's placeholder stays"
        );
        assert_eq!(std::fs::metadata(&target).unwrap().len(), 3);
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(names.len(), 1, "no part left");
    }

    /// Edited through the mount just before the fetch lands: the edit stays,
    /// the download is dropped.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_fetch_never_lands_over_an_edit() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x.txt");
        let file = std::fs::File::create(&target).unwrap();
        crate::drive::placeholder::write(&file, 10).unwrap();
        drop(file);
        let state = State::memory().unwrap();
        let mut sink = SyncSink::new(&drive(dir.path()), record(&state), &target, None, None);
        let at = target.clone();
        sink.meanwhile = Some(Box::new(move || {
            std::fs::remove_file(&at).unwrap();
            std::fs::write(&at, "edited").unwrap();
        }));
        sink.write_at(0, b"downloaded").unwrap();
        assert!(sink.finish().is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "edited");
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(names.len(), 1, "no part left");
    }

    /// A drive's pair (Linux: placeholders in its folder).
    #[cfg(target_os = "linux")]
    fn drive(path: &Path) -> Pair {
        Pair {
            mode: crate::state::Mode::Drive,
            ..pair(path)
        }
    }

    /// The fingerprint a sink expects of the file at `path`.
    #[cfg(target_os = "linux")]
    fn fingerprint(path: &Path) -> (i64, i64, Option<String>) {
        let meta = std::fs::metadata(path).unwrap();
        (meta.len() as i64, local::mtime(&meta), local::file_id(path))
    }

    /// A drive's placeholder lands as a download does, and records its
    /// fingerprint: its size, no data, marked. Over the base's content (an
    /// eviction for a remote change) it replaces it; over an edit, the edit
    /// stays.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_placeholder_lands_only_over_what_was_synced() {
        use crate::drive::placeholder;
        let dir = tempfile::tempdir().unwrap();
        let state = State::memory().unwrap();
        let p = drive(dir.path());
        let new = dir.path().join("new.bin");
        let sink = SyncSink::new(&p, record(&state), &new, None, None);
        sink.placeholder(7_000).unwrap();
        assert!(placeholder::is(&new));
        assert_eq!(std::fs::metadata(&new).unwrap().len(), 7_000);
        let e = sink.done.lock().unwrap().clone().unwrap();
        assert_eq!((e.size, e.file_id), (7_000, local::file_id(&new)));

        for edited in [false, true] {
            let target = dir.path().join("x.txt");
            std::fs::write(&target, "synced").unwrap();
            let expected = fingerprint(&target);
            if edited {
                std::fs::write(&target, "edited, longer").unwrap();
            }
            let sink = SyncSink::new(&p, record(&state), &target, Some(expected), None);
            let landed = sink.placeholder(20);
            assert_eq!(landed.is_ok(), !edited, "edited: {edited}");
            assert_eq!(placeholder::is(&target), !edited);
            if edited {
                assert_eq!(std::fs::read_to_string(&target).unwrap(), "edited, longer");
            }
            std::fs::remove_file(&target).unwrap();
        }
    }

    /// Over a drive's placeholder nothing is checked, as it holds nothing: a
    /// download replaces one whose fingerprint isn't the base's (an eviction
    /// interrupted), or one no entry knows (a crash before its record); a
    /// plain file no entry knows stays.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_placeholder_is_replaced_unchecked() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::memory().unwrap();
        let p = drive(dir.path());
        let target = dir.path().join("x.bin");
        let sink = SyncSink::new(&p, record(&state), &target, None, None);
        sink.placeholder(10).unwrap();
        let mut stale = fingerprint(&target);
        stale.1 += 1;
        for expected in [Some(stale), None] {
            let sink = SyncSink::new(&p, record(&state), &target, expected, None);
            sink.write_at(0, b"downloaded").unwrap();
            sink.finish().unwrap();
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "downloaded");
            assert!(!crate::drive::placeholder::is(&target));
            std::fs::remove_file(&target).unwrap();
            let sink = SyncSink::new(&p, record(&state), &target, None, None);
            sink.placeholder(10).unwrap();
        }
        let plain = dir.path().join("plain.txt");
        std::fs::write(&plain, "mine").unwrap();
        let sink = SyncSink::new(&p, record(&state), &plain, None, None);
        assert!(sink.placeholder(10).is_err());
        assert_eq!(std::fs::read_to_string(&plain).unwrap(), "mine");
    }

    /// A download replaces the base's file, and only it: a file edited
    /// before the download lands keeps its edit (uploaded on its base next
    /// round), and nothing is left beside either.
    #[test]
    fn a_download_replaces_only_the_synced_file() {
        for edited in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("x.txt");
            std::fs::write(&target, "synced").unwrap();
            let meta = std::fs::metadata(&target).unwrap();
            let expected = (
                meta.len() as i64,
                local::mtime(&meta),
                local::file_id(&target),
            );
            if edited {
                std::fs::write(&target, "edited, longer").unwrap();
            }
            let state = State::memory().unwrap();
            let sink = SyncSink::new(
                &pair(dir.path()),
                record(&state),
                &target,
                Some(expected),
                None,
            );
            sink.write_at(0, b"downloaded").unwrap();
            assert_eq!(sink.finish().is_ok(), !edited);
            let want = if edited {
                "edited, longer"
            } else {
                "downloaded"
            };
            assert_eq!(std::fs::read_to_string(&target).unwrap(), want);
            let names: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect();
            assert_eq!(names, ["x.txt"], "edited: {edited}");
        }
    }

    #[test]
    fn aside_never_replaces_an_earlier_copy() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("r.docx");
        assert_eq!(aside(&file), dir.path().join("r (local).docx"));
        std::fs::write(dir.path().join("r (local).docx"), "first edit").unwrap();
        assert_eq!(aside(&file), dir.path().join("r (local 2).docx"));
        std::fs::write(dir.path().join("notes (local)"), "").unwrap();
        assert_eq!(
            aside(&dir.path().join("notes")),
            dir.path().join("notes (local 2)")
        );
    }
}
