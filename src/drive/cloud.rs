//! A drive on Windows, through the Cloud Files API (`cloud-filter`, safe):
//! its folder a registered sync root, its files placeholders (NTFS files
//! without their content until opened, `0x400000` in their attributes; kept:
//! pinned, `0x80000`). Rounds scan the folder itself. Windows asks for a
//! placeholder's data when it is opened: the callback returns at once, and
//! the account's fetches serve it as it arrives, in the 4 KiB-aligned runs
//! Windows takes.

use std::fs;
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
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
use crate::state::Pair;

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

/// A drive's sync root: Theca's, the user's, its library's (unique on the
/// computer; a pair's ID isn't: every state numbers its pairs from 1).
fn id(library: Uuid) -> io::Result<SyncRootId> {
    Ok(SyncRootIdBuilder::new("Theca")
        .user_security_id(SecurityId::current_user().map_err(failed)?)
        .account_name(library.simple().to_string())
        .build())
}

/// The drive's folder registered as a sync root (NTFS only: Windows refuses
/// another file system).
pub fn register(pair: &Pair) -> io::Result<()> {
    let info = SyncRootInfo::default()
        .with_display_name(format!("Theca - {}", pair.library_name))
        .with_icon("%SystemRoot%\\system32\\imageres.dll,-1043")
        .with_version(env!("CARGO_PKG_VERSION"))
        .with_hydration_type(HydrationType::Full)
        .with_hydration_policy(HydrationPolicy::AutoDehydrationAllowed)
        .with_population_type(PopulationType::AlwaysFull)
        .with_allow_pinning(true)
        .with_path(long(&pair.path))
        .map_err(failed)?;
    let _one = registry();
    id(pair.library)?.register(info).map_err(failed)
}

/// Its sync root still registered (an uninstall, or by hand, unregisters it:
/// every placeholder would read as deleted).
pub fn registered(pair: &Pair) -> bool {
    match id(pair.library).and_then(|id| id.is_registered().map_err(failed)) {
        Ok(true) => true,
        Ok(false) => {
            let at = SyncRootId::from_path(long(&pair.path)).map(|id| id.to_os_string());
            tracing::warn!(
                pair = pair.id,
                "no sync root registered for it (at its folder: {at:?})"
            );
            false
        }
        // Not a "not found": unknown, the drive goes on (said).
        Err(e) => {
            tracing::warn!(pair = pair.id, "its sync root not checked: {e}");
            true
        }
    }
}

/// Unregistered: Windows deletes the placeholders not downloaded, and
/// leaves the others plain.
pub fn unregister(library: Uuid) -> io::Result<()> {
    let _one = registry();
    let id = id(library)?;
    if id.is_registered().map_err(failed)? {
        id.unregister().map_err(failed)?;
    }
    Ok(())
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

/// A drive connected: Windows calls this process for its placeholders while
/// it lives.
pub struct Root {
    _connection: Connection<Filter>,
    pub busy: Busy,
}

/// Connects `pair`'s sync root (registered by `add`; one lost stops the pair,
/// through the root check).
pub fn connect(pair: &Pair, fetcher: Fetcher) -> io::Result<Root> {
    let busy = Busy::default();
    let root = long(&pair.path);
    let filter = Filter {
        pair: pair.clone(),
        root: root.clone(),
        fetcher,
        busy: busy.clone(),
    };
    let connection = Session::new().connect(&root, filter).map_err(failed)?;
    Ok(Root {
        _connection: connection,
        busy,
    })
}

pub struct Filter {
    pair: Pair,
    /// The folder, as Windows names it in its callbacks (`long`).
    root: PathBuf,
    fetcher: Fetcher,
    busy: Busy,
}

impl Filter {
    fn relative(&self, path: &Path) -> Option<PathBuf> {
        path.strip_prefix(&self.root).ok().map(Path::to_owned)
    }
}

impl SyncFilter for Filter {
    fn fetch_data(
        &self,
        request: Request,
        ticket: ticket::FetchData,
        _info: info::FetchData,
    ) -> CResult<()> {
        let (Some(rel), Ok(node)) = (
            self.relative(&request.path()),
            Uuid::from_slice(request.file_blob()),
        ) else {
            return Err(CloudErrorKind::InvalidRequest);
        };
        let size = request.file_size();
        let (pair, fetcher, busy) = (self.pair.clone(), self.fetcher.clone(), self.busy.clone());
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
        let ours = request
            .process()
            .is_some_and(|p| p.id() == std::process::id());
        if !ours && let Some(rel) = self.relative(&request.path()) {
            self.fetcher.used(&self.pair, &rel);
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

    fn delete(
        &self,
        _request: Request,
        ticket: ticket::Delete,
        _info: info::Delete,
    ) -> CResult<()> {
        ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful)
    }

    fn rename(
        &self,
        _request: Request,
        ticket: ticket::Rename,
        _info: info::Rename,
    ) -> CResult<()> {
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
