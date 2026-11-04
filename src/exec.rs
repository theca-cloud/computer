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

use crate::local::{self, PART_SUFFIX, Scan, key};
use crate::plan::{Action, Level, Parent, Plan, UploadTo};
use crate::state::{Entry, Pair, RemoteState, State};

/// A file whose modification time is this recent may still be being written:
/// it waits for a later round.
const SETTLING: Duration = Duration::from_secs(2);

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
    state: &'a mut State,
    pair: &'a Pair,
    base: HashMap<Uuid, Entry>,
    /// Folders this round created on the server, by local path.
    created: HashMap<Vec<String>, Uuid>,
    outcome: Outcome,
}

/// Whether an error is worth retrying later as is (the network, the server
/// busy) rather than reporting.
fn transient(e: &Error) -> bool {
    match e {
        Error::Http(_) => true,
        Error::Api { status, .. } => *status >= 500 || *status == 429,
        _ => false,
    }
}

impl<'a> Executor<'a> {
    pub fn new(
        client: &'a Client,
        state: &'a mut State,
        pair: &'a Pair,
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
            base,
            created: HashMap::new(),
            outcome: Outcome::default(),
        })
    }

    /// A node's names from the root, as the base knows them.
    fn names(&self, mut node: Uuid) -> Option<Vec<String>> {
        let mut names = Vec::new();
        while node != self.pair.root {
            let e = self.base.get(&node)?;
            names.push(e.name.clone());
            node = e.parent;
            if names.len() > 10_000 {
                return None;
            }
        }
        names.reverse();
        Some(names)
    }

    /// A node's local path (the spelling on disk where it differs).
    fn path(&self, node: Uuid) -> Option<PathBuf> {
        self.names(node)
            .map(|n| local::resolve(&self.pair.path, &n))
    }

    fn local(&self, names: &[String]) -> PathBuf {
        local::resolve(&self.pair.path, names)
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
        if transient(e) {
            self.outcome.again = true;
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
        for (path, why) in &scan.unreadable {
            self.report(path, &format!("not synced: {why}"), Level::Error);
        }
        // The plan is ordered: uploads, downloads and trashing come in runs,
        // each run one batch.
        let kind = |a: &Action| match a {
            Action::Upload { .. } | Action::Merge { .. } => 1,
            Action::Download { .. } => 2,
            Action::RemoteTrash { .. } => 3,
            _ => 0,
        };
        for action in &plan.actions {
            tracing::debug!(pair = self.pair.id, ?action, "planned");
        }
        let mut actions = plan.actions.into_iter().peekable();
        while let Some(first) = actions.next() {
            let k = kind(&first);
            if k == 0 {
                self.one(first).await?;
                continue;
            }
            let mut batch = vec![first];
            while let Some(next) = actions.next_if(|a| kind(a) == k) {
                batch.push(next);
            }
            match k {
                1 => self.uploads(batch).await?,
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
        // A new pair's root, known from its first round on. Never replaced: a
        // disk unmounted meanwhile would make the empty mount point the root,
        // and the next round would read it as everything deleted.
        if self.pair.root_file_id.is_none()
            && let Some(root) = local::file_id(&self.pair.path)
        {
            self.state
                .set_pair_path(self.pair.id, &self.pair.path, Some(&root))?;
        }
        Ok(self.outcome)
    }

    async fn one(&mut self, action: Action) -> crate::state::Result<()> {
        match action {
            Action::Adopt { node, path, state } => {
                let file_id = local::file_id(&self.local(&path));
                self.put(
                    Entry {
                        node,
                        parent: state.parent,
                        name: state.name,
                        is_file: false,
                        version: None,
                        size: 0,
                        mtime: 0,
                        file_id,
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
                        Some(Err(e)) => self.fail(&display, &e),
                        None => {}
                    },
                    Err(e) => self.fail(&display, &e),
                }
            }
            Action::LocalMove { node, parent, name } => {
                let (Some(from), Some(mut to)) = (self.path(node), self.names(parent)) else {
                    return Ok(());
                };
                // The folder as spelled on disk, the new name exactly as given:
                // resolving it too would find the old spelling of a case-only
                // rename (`Report.txt` for `REPORT.txt`) on a case-sensitive
                // disk, and rename the file onto itself.
                let to_path = self.local(&to).join(&name);
                to.push(name.clone());
                let same_file = key(&from.to_string_lossy()) == key(&to_path.to_string_lossy());
                if to_path.symlink_metadata().is_ok() && !same_file {
                    self.report(
                        &to.join("/"),
                        "not moved here: something local has this name",
                        Level::Error,
                    );
                    return Ok(());
                }
                if let Some(dir) = to_path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                match std::fs::rename(&from, &to_path) {
                    Ok(()) => match self.base.get(&node).cloned() {
                        Some(mut e) => {
                            e.parent = parent;
                            e.name = name;
                            self.put(e, &[node])?;
                        }
                        None => self.settle(&[node])?,
                    },
                    Err(e) => self.report(&to.join("/"), &format!("not moved: {e}"), Level::Error),
                }
            }
            Action::LocalMkdir { node, parent, name } => {
                let Some(mut names) = self.names(parent) else {
                    return Ok(());
                };
                names.push(name.clone());
                let path = self.local(&names);
                match std::fs::create_dir_all(&path) {
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
                    if let Err(e) = std::fs::rename(&path, &aside) {
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
                        // Still the synced content: deleted.
                        Ok(m) if m.len() as i64 == e.size && local::mtime(&m) == e.mtime => {
                            std::fs::remove_file(&path).is_ok()
                        }
                        Ok(_) => false,
                        Err(_) => true,
                    }
                } else {
                    // Only once empty: anything left in it wasn't synced.
                    !path.exists() || std::fs::remove_dir(&path).is_ok()
                };
                if removed {
                    self.forget(node)?;
                    self.settle(&[node])?;
                }
            }
            Action::Accept { node, state } => match self.base.get(&node).cloned() {
                Some(mut e) => {
                    e.version = state.version;
                    self.put(e, &[node])?;
                }
                None => self.settle(&[node])?,
            },
            Action::Forget { node } => {
                self.forget(node)?;
                self.settle(&[node])?;
            }
            Action::Upload { .. }
            | Action::Merge { .. }
            | Action::Download { .. }
            | Action::RemoteTrash { .. } => unreachable!("batched"),
        }
        Ok(())
    }

    /// Uploads and merges, in one engine run.
    async fn uploads(&mut self, actions: Vec<Action>) -> crate::state::Result<()> {
        if actions.is_empty() {
            return Ok(());
        }
        // (source, target, local names, fingerprint, the entry's place)
        let mut jobs = Vec::new();
        for action in actions {
            let (names, target, place, node) = match action {
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
                        (path, target, (p, name), None)
                    }
                    UploadTo::Version { node, base } => {
                        let Some(e) = self.base.get(&node) else {
                            continue;
                        };
                        let place = (e.parent, e.name.clone());
                        let target = Target::Version {
                            library: self.pair.library,
                            file: node,
                            base,
                        };
                        (path, target, place, Some(node))
                    }
                },
                Action::Merge { node, path, state } => {
                    let target = Target::Version {
                        library: self.pair.library,
                        file: node,
                        base: state.version,
                    };
                    (path, target, (state.parent, state.name), Some(node))
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
            jobs.push((source, target, names, fingerprint, place, node));
        }
        let options = UploadOptions::default();
        let cancel = Cancel::default();
        let (uploader, run) = Uploader::new(self.client, &options, &cancel, |_| {});
        let submitted: Vec<_> = jobs
            .iter()
            .map(|(source, target, ..)| uploader.submit(source, target.clone()))
            .collect();
        drop(uploader);
        let (_, results) = futures::join!(run, futures::future::join_all(submitted));
        for (result, (_, _, names, (size, mtime, file_id), (parent, name), node)) in
            results.into_iter().zip(jobs)
        {
            match result {
                Ok(committed) => {
                    self.outcome.uploaded += usize::from(!committed.unchanged);
                    self.outcome.touched.push(committed.file);
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
                        node.as_slice(),
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
        }
        Ok(())
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
            let Some(mut names) = self.names(parent) else {
                continue;
            };
            // Where it is locally now (a known node), else where it goes.
            let target = match self.base.get(&node) {
                Some(_) => self.path(node),
                None => None,
            }
            .unwrap_or_else(|| {
                names.push(name.clone());
                self.local(&names)
            });
            let expected = self
                .base
                .get(&node)
                .filter(|e| e.is_file)
                .map(|e| (e.size, e.mtime));
            let sink = SyncSink::new(&target, expected, state.modified);
            jobs.push((sink, node, parent, name, state));
        }
        let options = DownloadOptions::default();
        let cancel = Cancel::default();
        let (downloader, run) = Downloader::new(self.client, &options, &cancel, |_| {});
        let fetched: Vec<_> = jobs
            .iter()
            .map(|(sink, node, _, _, state)| downloader.submit(*node, state.version, sink))
            .collect();
        drop(downloader);
        let (_, results) = futures::join!(run, futures::future::join_all(fetched));
        for (result, (sink, node, parent, name, state)) in results.into_iter().zip(jobs) {
            let display = sink.target.to_string_lossy().into_owned();
            match result {
                Ok(done) => {
                    let Ok(meta) = std::fs::metadata(&sink.target) else {
                        continue;
                    };
                    self.outcome.downloaded += 1;
                    let place = self.base.get(&node).map(|e| (e.parent, e.name.clone()));
                    let (parent, name) = place.unwrap_or((parent, name));
                    self.put(
                        Entry {
                            node,
                            parent,
                            name,
                            is_file: true,
                            version: Some(done.version),
                            size: meta.len() as i64,
                            mtime: local::mtime(&meta),
                            file_id: local::file_id(&sink.target),
                        },
                        &[node],
                    )?;
                    let _ = state;
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

/// `report.docx` → `report (local).docx`.
fn aside(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let name = match path.extension() {
        Some(ext) => format!("{stem} (local).{}", ext.to_string_lossy()),
        None => format!("{stem} (local)"),
    };
    path.with_file_name(name)
}

/// A download written as `<name>.theca-part`, then renamed over the real name
/// only if what's there is still what the base knew (or nothing, for a new
/// file): a local edit made meanwhile is never overwritten.
struct SyncSink {
    target: PathBuf,
    part: PathBuf,
    /// The size and modification time the target may have to be replaced.
    expected: Option<(i64, i64)>,
    modified: Option<DateTime<Utc>>,
    file: Mutex<Option<std::fs::File>>,
}

impl SyncSink {
    fn new(target: &Path, expected: Option<(i64, i64)>, modified: Option<DateTime<Utc>>) -> Self {
        let mut part = target
            .file_name()
            .map_or_else(Default::default, |n| n.to_owned());
        part.push(PART_SUFFIX);
        Self {
            target: target.to_owned(),
            part: target.with_file_name(part),
            expected,
            modified,
            file: Mutex::new(None),
        }
    }

    fn unchanged_target(&self) -> bool {
        match (std::fs::symlink_metadata(&self.target), self.expected) {
            (Err(_), _) => true,
            (Ok(m), Some((size, mtime))) => m.len() as i64 == size && local::mtime(&m) == mtime,
            (Ok(_), None) => false,
        }
    }
}

impl Sink for SyncSink {
    fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = self.file.lock().map_err(|_| io::Error::other("poisoned"))?;
        if file.is_none() {
            if let Some(dir) = self.part.parent() {
                std::fs::create_dir_all(dir)?;
            }
            *file = Some(std::fs::File::create(&self.part)?);
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
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::File::create(&self.part)?
            }
        };
        if let Some(modified) = self.modified {
            let _ = file.set_modified(SystemTime::from(modified));
        }
        drop(file);
        if !self.unchanged_target() {
            let _ = std::fs::remove_file(&self.part);
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "changed locally during the download",
            ));
        }
        std::fs::rename(&self.part, &self.target)
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
