//! The drives on Windows, through the Cloud Files API (`cloud-filter`, safe):
//! one registered sync root, the drives folder, so Explorer lists one entry; a
//! folder of it for each drive, its files placeholders (NTFS files without
//! their content until opened, `0x400000` in their attributes; kept: pinned,
//! `0x80000`). Rounds scan the drive's folder itself. Windows asks for a
//! placeholder's data when it is opened: the callback returns at once, and
//! the account's fetches serve it as it arrives, in the 4 KiB-aligned runs
//! Windows takes. One connection serves every drive, each callback routed by
//! the first component of its path to the drive.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::SystemTime;

use cloud_filter::error::{CResult, CloudErrorKind};
use cloud_filter::filter::{Request, SyncFilter, info, ticket};
use cloud_filter::metadata::Metadata;
use cloud_filter::placeholder::{ConvertOptions, PinOptions, PinState, Placeholder, UpdateOptions};
use cloud_filter::placeholder_create_info::PlaceholderCreateInfo;
use cloud_filter::root::{
    Connection, HydrationPolicy, HydrationType, PopulationType, SecurityId, Session, SyncRootId,
    SyncRootIdBuilder, SyncRootInfo,
};
use cloud_filter::utility::WriteAt;
use uuid::Uuid;

use super::busy::Busy;
use super::fetch::Fetcher;
use super::runs::Runs;
use super::{lock, lock_read, lock_write};
use crate::local;
use crate::state::{Mode, Pair, State};

const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
const PINNED: u32 = 0x8_0000;

/// Data handed to Windows at a time: a multiple of 4 KiB.
const RUN: usize = 1024 * 1024;

fn failed(e: impl std::fmt::Debug) -> io::Error {
    io::Error::other(format!("{e:?}"))
}

/// A folder's long, canonical path (`C:\Users\runneradmin`, not an 8.3
/// `RUNNER~1`), without `\\?\`: what Windows registers and names in its
/// callbacks.
fn long(path: &Path) -> PathBuf {
    let Ok(full) = fs::canonicalize(path) else {
        return path.to_owned();
    };
    let text = full.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(plain) if !plain.starts_with("UNC") => PathBuf::from(plain),
        _ => full,
    }
}

/// Registrations one at a time in this process: Windows' sync root manager
/// answered "Element not found" to one made while others were (a process
/// adding or removing several drives, the scenarios in parallel).
static REGISTRY: Mutex<()> = Mutex::new(());

fn registry() -> std::sync::MutexGuard<'static, ()> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

/// A sync root: Theca's, the user's, named by a UUID (unique on the
/// computer): this state's `drives_id` for the one sync root, a drive's
/// library for the one each drive had of its own before 1.4 (a pair's ID
/// isn't unique: every state numbers its pairs from 1).
fn id(unique: Uuid) -> io::Result<SyncRootId> {
    Ok(SyncRootIdBuilder::new("Theca")
        .user_security_id(SecurityId::current_user().map_err(failed)?)
        .account_name(unique.simple().to_string())
        .build())
}

/// The drives folder registered as the sync root (NTFS only: Windows refuses
/// another file system), "Theca" in Explorer's navigation pane; left if it
/// is.
pub fn register(state: &State, drives: &Path) -> io::Result<()> {
    let info = SyncRootInfo::default()
        .with_display_name("Theca")
        .with_icon("%SystemRoot%\\system32\\imageres.dll,-1043")
        .with_version(env!("CARGO_PKG_VERSION"))
        .with_hydration_type(HydrationType::Full)
        .with_hydration_policy(HydrationPolicy::AutoDehydrationAllowed)
        .with_population_type(PopulationType::AlwaysFull)
        .with_allow_pinning(true)
        .with_path(long(drives))
        .map_err(failed)?;
    let _one = registry();
    let id = id(state.drives_id().map_err(failed)?)?;
    if id.is_registered().map_err(failed)? {
        return Ok(());
    }
    id.register(info).map_err(failed)
}

/// The sync root still registered (an uninstall, or by hand, unregisters it:
/// every placeholder would read as deleted).
pub fn registered(state: &State) -> bool {
    let checked = state
        .drives_id()
        .map_err(failed)
        .and_then(id)
        .and_then(|id| id.is_registered().map_err(failed));
    match checked {
        Ok(registered) => {
            if !registered {
                tracing::warn!("no sync root registered for the drives folder");
            }
            registered
        }
        // Not a "not found": unknown, the drives go on (said).
        Err(e) => {
            tracing::warn!("the drives' sync root not checked: {e}");
            true
        }
    }
}

/// Unregistered: Windows deletes the placeholders not downloaded, and
/// leaves the others plain. `unique`: the state's `drives_id`, or a library
/// (a drive made by 1.3.x had a sync root of its own).
pub fn unregister(unique: Uuid) -> io::Result<()> {
    let _one = registry();
    let id = id(unique)?;
    if id.is_registered().map_err(failed)? {
        id.unregister().map_err(failed)?;
    }
    Ok(())
}

/// What a drive needs on its folder before its first round: the drives
/// folder registered (with the first drive), and the drive's folder a
/// directory placeholder, so that a delete or a rename of it reaches the
/// sync root's callbacks (`Filter`; that Windows then calls them for a
/// directory made here is assumed).
pub fn prepare(state: &State, pair: &Pair) -> io::Result<()> {
    let drives = pair
        .path
        .parent()
        .ok_or_else(|| io::Error::other("a drive has no drives folder"))?;
    register(state, drives)?;
    // Registered, yet not one Windows serves (a folder in the temp folder,
    // hidden or system: `UNABLE_TO_MASK_PATH`).
    if !registered(state) {
        if let Ok(id) = state.drives_id() {
            let _ = unregister(id);
        }
        return Err(io::Error::other(
            "Windows doesn't keep it as a sync root (a hidden, system or temporary folder?)",
        ));
    }
    let converted = Placeholder::open(&pair.path).and_then(|mut p| {
        p.convert_to_placeholder(ConvertOptions::default().mark_in_sync(), None)
            .map(|_| ())
    });
    if let Err(e) = converted {
        tracing::warn!(pair = pair.id, "its folder not made a placeholder: {e}");
    }
    Ok(())
}

/// The last drive gone (removed: its files out of the folder): the sync root
/// unregistered, the drives folder deleted. Nothing while another drive is.
pub fn release(state: &State) {
    if !matches!(state.pairs(), Ok(pairs) if pairs.iter().all(|p| p.mode != Mode::Drive)) {
        return;
    }
    let (Ok(Some(drives)), Ok(id)) = (state.drives_folder(), state.drives_id()) else {
        return;
    };
    if let Err(e) = unregister(id) {
        tracing::warn!("the drives' sync root not removed: {e}");
        return;
    }
    let _ = fs::remove_dir(&drives);
}

fn attributes(path: &Path) -> u32 {
    fs::symlink_metadata(path).map_or(0, |m| m.file_attributes())
}

/// A placeholder whose content isn't here.
pub fn is_placeholder(path: &Path) -> bool {
    attributes(path) & RECALL_ON_DATA_ACCESS != 0
}

/// Kept on this computer: pinned ("Always keep on this device").
pub fn is_pinned(path: &Path) -> bool {
    attributes(path) & PINNED != 0
}

/// The node a placeholder is, as its blob says.
fn blob(node: Uuid) -> Vec<u8> {
    node.as_bytes().to_vec()
}

fn metadata(size: u64, modified: Option<SystemTime>) -> Metadata {
    let mut m = Metadata::file().size(size);
    if let Some(t) = modified.and_then(|t| nt_time::FileTime::try_from(t).ok()) {
        m = m.written(t).changed(t).created(t).accessed(t);
    }
    m
}

/// A remote file's version as a placeholder at `path`: created, or the
/// placeholder there updated (its content dropped, when the caller checked
/// it is the base's). In sync, its blob its node.
pub fn placeholder(
    path: &Path,
    node: Uuid,
    size: u64,
    modified: Option<SystemTime>,
) -> io::Result<()> {
    let meta = metadata(size, modified);
    if path.symlink_metadata().is_err() {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(io::Error::other("no parent"));
        };
        return PlaceholderCreateInfo::new(name)
            .metadata(meta)
            .mark_in_sync()
            .blob(blob(node))
            .create(parent)
            .map(|_| ())
            .map_err(failed);
    }
    let mut p = exclusive(path)?;
    // A plain file (made here, or merged) becomes one first.
    let _ = p.convert_to_placeholder(
        ConvertOptions::default().mark_in_sync().blob(blob(node)),
        None,
    );
    p.update(
        UpdateOptions::default()
            .metadata(meta)
            .dehydrate()
            .mark_in_sync()
            .blob(&blob(node)),
        None,
    )
    .map(|_| ())
    .map_err(failed)
}

/// Opened alone: Windows refuses while another handle has it open, which
/// keeps an eviction from going under an open file. Read access, not none:
/// a handle without data access is never checked against the others.
fn exclusive(path: &Path) -> io::Result<Placeholder> {
    let file = fs::OpenOptions::new().read(true).share_mode(0).open(path)?;
    Ok(Placeholder::from(file))
}

/// A downloaded file evicted in place (refused while open elsewhere): a
/// plain file (made here) becomes a placeholder first, its node in its blob
/// (what a fetch asks for).
pub fn evict(path: &Path, node: Uuid) -> io::Result<()> {
    let mut p = exclusive(path)?;
    let _ = p.convert_to_placeholder(
        ConvertOptions::default().mark_in_sync().blob(blob(node)),
        None,
    );
    p.update(UpdateOptions::default().dehydrate(), None)
        .map(|_| ())
        .map_err(failed)
}

/// A placeholder's content brought here now (a file kept): Windows asks
/// this process for it, as for an open.
pub fn hydrate(path: &Path) -> io::Result<()> {
    Placeholder::open(path)
        .and_then(|mut p| p.hydrate(..))
        .map_err(failed)
}

/// Kept, or not anymore: the pin Windows stores (its children inherit it).
pub fn pin(path: &Path, kept: bool) -> io::Result<()> {
    let state = if kept {
        PinState::Pinned
    } else {
        PinState::Unspecified
    };
    let mut options = PinOptions::default();
    options.recurse();
    Placeholder::open(path)
        .and_then(|mut p| p.mark_pin(state, options).map(|_| ()))
        .map_err(failed)
}

/// The drives of one process listed under the one sync root, and served
/// while their engines run.
#[derive(Default)]
pub struct Drives {
    registry: Arc<Registry>,
    connection: Mutex<Option<Connection<Filter>>>,
}

/// What the callbacks answer from: the drives, by pair.
#[derive(Default)]
struct Registry {
    table: RwLock<HashMap<i64, Arc<Listed>>>,
    /// The state the drives are in (a drive being removed is no longer in it).
    state: OnceLock<PathBuf>,
}

struct Listed {
    /// The drive's folder in the drives folder.
    name: OsString,
    /// As its rounds see it.
    pair: Pair,
    /// What fetches for it while it is served; none while it is paused or
    /// stopped (its placeholders then fail to open).
    fetcher: Mutex<Option<Fetcher>>,
    busy: Busy,
}

/// A drive served by its engine: its fetches answered, what it serves busy
/// for the rounds. Dropped, it stays listed: what is downloaded opens.
pub struct Mount {
    pair: i64,
    drives: Arc<Drives>,
    pub busy: Busy,
}

impl Drop for Mount {
    fn drop(&mut self) {
        self.drives.idle(self.pair);
    }
}

/// A folder's name as Windows compares it.
fn key(name: &std::ffi::OsStr) -> String {
    local::key(&name.to_string_lossy())
}

impl Drives {
    /// `pair` served in its `folder` of the drives folder, which is connected
    /// with the first drive (registered by `add`; one lost stops the pairs,
    /// through the root check).
    pub fn serve(
        self: &Arc<Self>,
        pair: &Pair,
        folder: &Path,
        fetcher: Fetcher,
    ) -> io::Result<Mount> {
        let drive = self.attach(pair, folder, Some(fetcher))?;
        Ok(Mount {
            pair: pair.id,
            drives: self.clone(),
            busy: drive.busy.clone(),
        })
    }

    /// `pair` listed, not served (paused, or stopped).
    pub fn list(&self, pair: &Pair, folder: &Path) -> io::Result<()> {
        self.attach(pair, folder, None).map(|_| ())
    }

    /// `pair` is no longer a drive (removed): out of the list, and the
    /// connection ends with the last one.
    pub fn withdraw(&self, pair: i64) {
        let empty = {
            let mut table = lock_write(&self.registry.table);
            table.remove(&pair);
            table.is_empty()
        };
        if empty {
            lock(&self.connection).take();
            // The last drive removed while this connection was open: Windows
            // refuses to unregister a sync root with one open (`release`
            // tried first, from the removal), so it is done now it is closed.
            if let Some(path) = self.registry.state.get()
                && let Ok(state) = State::open(path)
            {
                release(&state);
            }
        }
    }

    /// The state the drives are in, for the callbacks to tell a drive being
    /// removed (out of the state, its files still being deleted) from one a
    /// user deletes. Said before the first drive.
    pub fn in_state(&self, state: &State) {
        if let Some(path) = state.path() {
            let _ = self.registry.state.set(path.to_owned());
        }
    }

    /// A drive that is listed has what it serves busy for the rounds.
    pub fn busy(&self, pair: i64) -> Option<Busy> {
        lock_read(&self.registry.table)
            .get(&pair)
            .map(|l| l.busy.clone())
    }

    /// The drives folder is connected.
    pub fn mounted(&self) -> bool {
        lock(&self.connection).is_some()
    }

    /// Disconnected (the engine ends): the sync root stays registered.
    pub fn shutdown(&self) {
        lock(&self.connection).take();
        lock_write(&self.registry.table).clear();
    }

    /// Serving stopped: the drive stays listed, its fetches refused.
    fn idle(&self, pair: i64) {
        if let Some(listed) = lock_read(&self.registry.table).get(&pair) {
            *lock(&listed.fetcher) = None;
        }
    }

    fn attach(
        &self,
        pair: &Pair,
        folder: &Path,
        fetcher: Option<Fetcher>,
    ) -> io::Result<Arc<Listed>> {
        let drives = folder
            .parent()
            .ok_or_else(|| io::Error::other("a drive has no folder to be listed in"))?;
        let name = folder
            .file_name()
            .ok_or_else(|| io::Error::other("a drive has no name"))?
            .to_owned();
        let mut connection = lock(&self.connection);
        if connection.is_none() {
            let root = long(drives);
            let filter = Filter {
                registry: self.registry.clone(),
                root: root.clone(),
            };
            *connection = Some(Session::new().connect(&root, filter).map_err(failed)?);
        }
        let mut table = lock_write(&self.registry.table);
        if let Some(listed) = table.get(&pair.id) {
            // Listed already: what serves it changed.
            *lock(&listed.fetcher) = fetcher;
            return Ok(listed.clone());
        }
        let listed = Arc::new(Listed {
            name,
            pair: pair.clone(),
            fetcher: Mutex::new(fetcher),
            busy: Busy::default(),
        });
        table.insert(pair.id, listed.clone());
        Ok(listed)
    }
}

pub struct Filter {
    registry: Arc<Registry>,
    /// The drives folder, as Windows names it in its callbacks (`long`).
    root: PathBuf,
}

impl Filter {
    /// Whether the state has a drive whose folder is this one (`rel`, below
    /// the drives folder): not one being removed. Unknown: it has.
    fn is_a_drive_of_the_state(&self, rel: &Path) -> bool {
        let Some(wanted) = first(rel) else {
            return true;
        };
        let Some(path) = self.registry.state.get() else {
            return true;
        };
        let Ok(state) = State::open(path) else {
            return true;
        };
        state.pairs().map_or(true, |pairs| {
            pairs
                .iter()
                .filter(|p| p.mode == Mode::Drive)
                .any(|p| p.path.file_name().map(key).as_deref() == Some(wanted.as_str()))
        })
    }

    /// `path` below the drives folder.
    fn within(&self, path: &Path) -> Option<PathBuf> {
        path.strip_prefix(&self.root).ok().map(Path::to_owned)
    }

    /// The drive a path is in, and the path in it.
    fn drive(&self, path: &Path) -> Option<(Arc<Listed>, PathBuf)> {
        let rel = self.within(path)?;
        let mut parts = rel.components();
        let wanted = key(parts.next()?.as_os_str());
        let inside = parts.as_path().to_owned();
        lock_read(&self.registry.table)
            .values()
            .find(|l| key(&l.name) == wanted)
            .map(|l| (l.clone(), inside))
    }
}

/// The first folder of a path below the drives folder: the drive's.
fn first(rel: &Path) -> Option<String> {
    rel.components().next().map(|c| key(c.as_os_str()))
}

/// A call from this very process (a round, a removal) isn't a user's.
fn ours(request: &Request) -> bool {
    request
        .process()
        .is_some_and(|p| p.id() == std::process::id())
}

impl SyncFilter for Filter {
    fn fetch_data(
        &self,
        request: Request,
        ticket: ticket::FetchData,
        _info: info::FetchData,
    ) -> CResult<()> {
        let (Some((drive, rel)), Ok(node)) = (
            self.drive(&request.path()),
            Uuid::from_slice(request.file_blob()),
        ) else {
            return Err(CloudErrorKind::InvalidRequest);
        };
        // A drive paused or stopped fetches nothing.
        let Some(fetcher) = lock(&drive.fetcher).clone() else {
            return Err(CloudErrorKind::Unsuccessful);
        };
        let size = request.file_size();
        let (pair, busy) = (drive.pair.clone(), drive.busy.clone());
        // Served from a thread of its own: the callback returns at once.
        std::thread::spawn(move || {
            let _taken = busy.take(&rel);
            let sink = Transfer::new(ticket, size);
            if let Err(why) = fetcher.fetch_node(&pair, node, &rel, Box::new(sink)) {
                tracing::warn!(pair = pair.id, path = %rel.display(), "not fetched: {why}");
            }
        });
        Ok(())
    }

    /// Opened by another process: used. Our own (a round reading it to
    /// upload) isn't a use.
    fn opened(&self, request: Request, _info: info::Opened) {
        if !ours(&request)
            && let Some((drive, rel)) = self.drive(&request.path())
            && let Some(fetcher) = lock(&drive.fetcher).as_ref()
        {
            fetcher.used(&drive.pair, &rel);
        }
    }

    fn dehydrate(
        &self,
        _request: Request,
        ticket: ticket::Dehydrate,
        _info: info::Dehydrate,
    ) -> CResult<()> {
        ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful)
    }

    /// A drive's folder isn't the user's to delete: it is a library, gone
    /// from here when it is removed (the pair out of the state first, by
    /// whichever process: this one's engine hears of every deletion).
    fn delete(&self, request: Request, ticket: ticket::Delete, _info: info::Delete) -> CResult<()> {
        if let Some(rel) = self.within(&request.path())
            && rel.components().count() == 1
            && self.is_a_drive_of_the_state(&rel)
        {
            return Err(CloudErrorKind::AccessDenied);
        }
        ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful)
    }

    /// Nothing is renamed into or out of being a drive's folder, and nothing
    /// moves from a drive to another: two libraries.
    fn rename(&self, request: Request, ticket: ticket::Rename, info: info::Rename) -> CResult<()> {
        let source = self.within(&request.path());
        let target = info
            .target_in_scope()
            .then(|| self.within(&info.target_path()))
            .flatten();
        let a_folder =
            |rel: &Option<PathBuf>| rel.as_ref().is_some_and(|r| r.components().count() == 1);
        if a_folder(&source) || a_folder(&target) {
            return Err(CloudErrorKind::AccessDenied);
        }
        if let (Some(from), Some(to)) = (&source, &target)
            && first(from) != first(to)
        {
            return Err(CloudErrorKind::AccessDenied);
        }
        ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful)
    }
}

/// A download handed to Windows as it arrives (`Runs`: in order, in
/// 4 KiB-aligned runs but the file's end). Dropped unfinished (a fetch that
/// failed before its download, or never ran), it fails Windows' request:
/// otherwise the opening program waits Windows' 60 s.
pub struct Transfer {
    ticket: ticket::FetchData,
    size: u64,
    runs: Mutex<Runs>,
    finished: std::sync::atomic::AtomicBool,
}

impl Drop for Transfer {
    fn drop(&mut self) {
        if !self.finished.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = self.ticket.fail(CloudErrorKind::Unsuccessful);
        }
    }
}

impl Transfer {
    pub fn new(ticket: ticket::FetchData, size: u64) -> Self {
        Self {
            ticket,
            size,
            runs: Mutex::new(Runs::default()),
            finished: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn send(&self, data: &[u8], offset: u64) -> io::Result<()> {
        self.ticket.write_at(data, offset).map_err(failed)?;
        let _ = self
            .ticket
            .report_progress(self.size, offset + data.len() as u64);
        Ok(())
    }
}

impl theca_client::Sink for Transfer {
    fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut runs = self.runs.lock().map_err(|_| io::Error::other("poisoned"))?;
        runs.arrived(offset, data, RUN, |d, o| self.send(d, o))
    }

    fn finish(&self) -> io::Result<()> {
        let mut runs = self.runs.lock().map_err(|_| io::Error::other("poisoned"))?;
        runs.finished(RUN, |d, o| self.send(d, o))?;
        self.finished
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn abort(&self) {
        let _ = self.ticket.fail(CloudErrorKind::Unsuccessful);
    }
}
