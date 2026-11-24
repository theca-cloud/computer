//! The drives' file system (Linux, `fuser`): one mount on the drives folder,
//! a folder of it for each drive, so a file manager lists one entry. A request
//! is routed by its first path component to the drive (`Mux`), every operation
//! forwarded to the drive's backing folder, where its rounds work. A
//! placeholder opened is fetched first, on a thread of its own (the session
//! goes on answering); what is open or fetched is busy for the rounds
//! (`Busy`). Part files never show. The kernel keeps attributes and names a
//! second only: rounds change the backing folder behind its back.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, FileTimes, Metadata, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    BackgroundSession, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, MountOption, OpenAccMode, OpenFlags,
    RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyStatfs, ReplyWrite, Request, SessionACL, TimeOrNow, WriteFlags,
};
use rustix::fs::{OFlags, RenameFlags as Rename};

use super::busy::Busy;
use super::fetch::Fetcher;
use super::placeholder;
use super::{lock, lock_read, lock_write};
use crate::local::{self, PART_SUFFIX};
use crate::state::Pair;

const TTL: Duration = Duration::from_secs(1);

/// How many inode numbers a drive owns: the drive's slot is the number's
/// high bits (the top folder is inode 1, slot 0).
const SLOT_BITS: u32 = 40;
/// The top folder, which lists the drives.
const TOP: u64 = 1;

/// A drive served by its engine: its fetches answered, what it serves busy
/// for the rounds. Dropped (its engine stopped, or it stopped), the drive
/// stays listed, as a stopped sync pair's folder stays: what is downloaded
/// opens, a placeholder doesn't.
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

/// The drives of one process listed in one mount of the drives folder.
#[derive(Default)]
pub struct Drives {
    registry: Arc<Registry>,
    mount: Mutex<Option<Mounted>>,
}

/// The one mount: `fusermount3 -u`ed, joined, and its folder removed (if
/// empty: it is Theca's only while mounted) when dropped.
struct Mounted {
    folder: PathBuf,
    session: Option<BackgroundSession>,
}

impl Drop for Mounted {
    fn drop(&mut self) {
        unmount(&self.folder);
        if let Some(session) = self.session.take() {
            let _ = session.join();
        }
        let _ = fs::remove_dir(&self.folder);
    }
}

/// What the file system answers from: the drives listed, by slot.
#[derive(Default)]
struct Registry {
    table: RwLock<Table>,
}

#[derive(Default)]
struct Table {
    by_slot: HashMap<u64, Listed>,
    by_pair: HashMap<i64, u64>,
    next: u64,
}

struct Listed {
    /// The drive's folder in the drives folder.
    name: OsString,
    drive: Arc<Shared>,
}

impl Drives {
    /// `pair` (as its rounds see it: its path the backing folder) served in
    /// its `folder` of the drives folder, which is mounted first if it is the
    /// first drive.
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

    /// `pair` listed, not served (paused, or stopped): its downloaded files
    /// open, its placeholders don't.
    pub fn list(&self, pair: &Pair, folder: &Path) -> io::Result<()> {
        self.attach(pair, folder, None).map(|_| ())
    }

    /// `pair` is no longer a drive (removed): out of the list, and the mount
    /// ends with the last one.
    pub fn withdraw(&self, pair: i64) {
        // The mount's lock first, as `attach` takes them: a drive attached
        // meanwhile isn't left to an unmount made for the one before.
        let mut mount = lock(&self.mount);
        let empty = {
            let mut table = lock_write(&self.registry.table);
            if let Some(slot) = table.by_pair.remove(&pair) {
                table.by_slot.remove(&slot);
            }
            table.by_slot.is_empty()
        };
        if empty {
            mount.take();
        }
    }

    /// The drives folder checked bare (`bare`) while nothing is mounted, under
    /// the mount's lock: a mount made meanwhile by another account isn't
    /// cleared by the check (it unmounts what it finds).
    pub fn bare_unless_mounted(&self, folder: &Path) -> Result<(), String> {
        let mount = lock(&self.mount);
        if mount.is_some() {
            return Ok(());
        }
        bare(folder)
    }

    /// A drive that is listed has what it serves busy for the rounds.
    pub fn busy(&self, pair: i64) -> Option<Busy> {
        let table = lock_read(&self.registry.table);
        let slot = table.by_pair.get(&pair)?;
        Some(table.by_slot.get(slot)?.drive.busy.clone())
    }

    /// The drives folder is mounted.
    pub fn mounted(&self) -> bool {
        lock(&self.mount).is_some()
    }

    /// Unmounted (the engine ends).
    pub fn shutdown(&self) {
        lock(&self.mount).take();
        let mut table = lock_write(&self.registry.table);
        table.by_slot.clear();
        table.by_pair.clear();
    }

    /// Serving stopped: the drive stays listed, its fetches refused.
    fn idle(&self, pair: i64) {
        let table = lock_read(&self.registry.table);
        if let Some(slot) = table.by_pair.get(&pair)
            && let Some(listed) = table.by_slot.get(slot)
        {
            *lock(&listed.drive.fetcher) = None;
        }
    }

    fn attach(
        &self,
        pair: &Pair,
        folder: &Path,
        fetcher: Option<Fetcher>,
    ) -> io::Result<Arc<Shared>> {
        let drives = folder
            .parent()
            .ok_or_else(|| io::Error::other("a drive has no folder to be listed in"))?;
        let name = folder
            .file_name()
            .ok_or_else(|| io::Error::other("a drive has no name"))?
            .to_owned();
        let mut mount = lock(&self.mount);
        if mount.is_none() {
            *mount = Some(self.mount_on(drives)?);
        }
        let mut table = lock_write(&self.registry.table);
        if let Some(slot) = table.by_pair.get(&pair.id).copied()
            && let Some(listed) = table.by_slot.get_mut(&slot)
        {
            // Listed already: what serves it changed, never its numbers.
            listed.name = name;
            *lock(&listed.drive.fetcher) = fetcher;
            return Ok(listed.drive.clone());
        }
        let meta = fs::metadata(&pair.path)?;
        table.next += 1;
        let slot = table.next;
        let drive = Arc::new(Shared {
            pair: pair.clone(),
            inodes: Mutex::new(Inodes::new(slot << SLOT_BITS)),
            handles: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(1),
            busy: Busy::default(),
            fetcher: Mutex::new(fetcher),
            uid: meta.uid(),
            gid: meta.gid(),
            own_call: AtomicBool::new(false),
        });
        table.by_pair.insert(pair.id, slot);
        table.by_slot.insert(
            slot,
            Listed {
                name,
                drive: drive.clone(),
            },
        );
        Ok(drive)
    }

    /// The drives folder made (it exists only while mounted) and mounted, once
    /// it is bare, first clearing a mount a crash left there (`ENOTCONN`,
    /// nothing saved into it).
    fn mount_on(&self, folder: &Path) -> io::Result<Mounted> {
        bare(folder).map_err(io::Error::other)?;
        fs::create_dir_all(folder)?;
        let mux = Mux {
            registry: self.registry.clone(),
            uid: rustix::process::geteuid().as_raw(),
            gid: rustix::process::getegid().as_raw(),
        };
        let mut config = Config::default();
        config.mount_options = vec![
            MountOption::FSName("theca-drives".into()),
            MountOption::Subtype("theca".into()),
            MountOption::DefaultPermissions,
            MountOption::NoDev,
            MountOption::NoSuid,
        ];
        config.acl = SessionACL::Owner;
        // Several workers, for every drive: an operation waiting on a file the
        // round holds (a kept download, an upload) leaves the rest answering.
        config.n_threads = Some(8);
        match fuser::spawn_mount(mux, folder, &config) {
            Ok(session) => Ok(Mounted {
                folder: folder.to_owned(),
                session: Some(session),
            }),
            Err(e) => {
                // Made here, empty, and not mounted: not left behind.
                let _ = fs::remove_dir(folder);
                Err(e)
            }
        }
    }
}

/// A leftover mount cleared from outside (a crash left it).
pub fn clear(folder: &Path) {
    unmount(folder);
}

/// `fusermount3 -u`: what ends a session cleanly (`fuser`'s own unmount
/// doesn't return without `auto_unmount`, which needs root's `fuse.conf`).
fn unmount(folder: &Path) {
    let _ = Command::new("fusermount3")
        .args(["-u", "-q", "-z"])
        .arg(folder)
        .status();
}

/// The drives folder, while it isn't mounted, is missing or holds nothing but
/// ignored names (what operating systems leave): anything else was saved there
/// while Theca wasn't running, and the mount would hide it. A mount a crash
/// left is cleared first.
pub fn bare(folder: &Path) -> Result<(), String> {
    unmount(folder);
    if !folder.exists() {
        return Ok(());
    }
    let listed = fs::read_dir(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    let saved = listed
        .flatten()
        .any(|e| !e.file_name().to_str().is_some_and(local::skipped));
    if saved {
        return Err(format!(
            "files were saved in {} while Theca wasn't running: move them out, and the drive comes back",
            folder.display()
        ));
    }
    Ok(())
}

/// What a drive's own folder lists (a stop for it is checked again once
/// this changes).
pub fn listing(folder: &Path) -> Option<String> {
    let mut names: Vec<String> = fs::read_dir(folder)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Some(names.join("/"))
}

/// A file system is mounted on `folder` (ours, served by a running engine).
pub fn mounted(folder: &Path) -> bool {
    let Ok(folder) = std::path::absolute(folder) else {
        return false;
    };
    let Ok(mounts) = fs::read_to_string("/proc/self/mounts") else {
        return false;
    };
    // Spaces and the like octal-escaped (`\040`), as fstab spells them.
    let wanted = folder
        .to_string_lossy()
        .replace('\\', "\\134")
        .replace(' ', "\\040")
        .replace('\t', "\\011")
        .replace('\n', "\\012");
    mounts
        .lines()
        .any(|l| l.split(' ').nth(1) == Some(wanted.as_str()))
}

/// `fusermount3` installed (the `fuse3` package): drives need it.
pub fn available() -> bool {
    Command::new("fusermount3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Inode numbers by path below the backing folder: the kernel names files by
/// them. A drive's start at its `base` (its slot in the high bits, so no two
/// drives share a number), its root `base + 1`.
struct Inodes {
    paths: HashMap<u64, PathBuf>,
    numbers: HashMap<PathBuf, u64>,
    next: u64,
}

impl Inodes {
    fn new(base: u64) -> Self {
        let mut i = Inodes {
            paths: HashMap::new(),
            numbers: HashMap::new(),
            next: base + 2,
        };
        i.paths.insert(base + 1, PathBuf::new());
        i.numbers.insert(PathBuf::new(), base + 1);
        i
    }

    fn number(&mut self, rel: &Path) -> u64 {
        if let Some(n) = self.numbers.get(rel) {
            return *n;
        }
        let n = self.next;
        self.next += 1;
        self.numbers.insert(rel.to_owned(), n);
        self.paths.insert(n, rel.to_owned());
        n
    }

    /// `rel` and what is below it forgotten (deleted).
    fn forget(&mut self, rel: &Path) {
        let gone: Vec<PathBuf> = self
            .numbers
            .keys()
            .filter(|p| p.starts_with(rel))
            .cloned()
            .collect();
        for p in gone {
            if let Some(n) = self.numbers.remove(&p) {
                self.paths.remove(&n);
            }
        }
    }

    /// `from` and what is below it renamed to `to`, keeping their numbers.
    fn moved(&mut self, from: &Path, to: &Path) {
        self.forget(to);
        let below: Vec<(PathBuf, u64)> = self
            .numbers
            .iter()
            .filter(|(p, _)| p.starts_with(from))
            .map(|(p, n)| (p.clone(), *n))
            .collect();
        for (p, n) in below {
            let rest = p.strip_prefix(from).expect("below it");
            // `to.join("")` would end in a slash: ENOTDIR on every stat.
            let new = if rest.as_os_str().is_empty() {
                to.to_owned()
            } else {
                to.join(rest)
            };
            self.numbers.remove(&p);
            self.numbers.insert(new.clone(), n);
            self.paths.insert(n, new);
        }
    }
}

struct Shared {
    /// As its rounds see it: its path is the backing folder.
    pair: Pair,
    inodes: Mutex<Inodes>,
    handles: Mutex<HashMap<u64, (PathBuf, Arc<File>)>>,
    next_handle: AtomicU64,
    busy: Busy,
    /// What fetches for it while it is served; none while it is paused or
    /// stopped (a placeholder then fails to open).
    fetcher: Mutex<Option<Fetcher>>,
    uid: u32,
    gid: u32,
    /// This process called into its own drive (said once per drive).
    own_call: AtomicBool,
}

/// What the warning of a call from the drive's own process says: the tests
/// fail a scenario whose daemon logged it.
pub const OWN_CALL: &str = "called into its own drive";

/// `pid`, a request's caller (a thread's ID), is a thread of this process.
fn own(pid: u32) -> bool {
    pid != 0 && Path::new(&format!("/proc/self/task/{pid}")).exists()
}

struct Drive(Arc<Shared>);

fn errno(e: io::Error) -> Errno {
    Errno::from(e)
}

fn time(sec: i64, nsec: i64) -> SystemTime {
    match u64::try_from(sec) {
        Ok(sec) => UNIX_EPOCH + Duration::new(sec, nsec as u32),
        Err(_) => {
            UNIX_EPOCH - Duration::from_secs(sec.unsigned_abs()) + Duration::from_nanos(nsec as u64)
        }
    }
}

fn hidden(name: &OsStr) -> bool {
    name.to_string_lossy().ends_with(PART_SUFFIX)
}

impl Shared {
    /// A request from this very process is answered, and said once: the
    /// thread making it waits on the threads serving it, and if the process
    /// is killed meanwhile, nothing can end it (theca/computer#1).
    fn note_caller(&self, req: &Request, operation: &str) {
        if own(req.pid()) && !self.own_call.swap(true, Ordering::Relaxed) {
            tracing::warn!(pair = self.pair.id, operation, "Theca {OWN_CALL}");
        }
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        lock(m)
    }

    fn abs(&self, rel: &Path) -> PathBuf {
        self.pair.path.join(rel)
    }

    fn number(&self, rel: &Path) -> u64 {
        Self::lock(&self.inodes).number(rel)
    }

    fn path(&self, ino: INodeNo) -> Result<PathBuf, Errno> {
        Self::lock(&self.inodes)
            .paths
            .get(&ino.0)
            .cloned()
            .ok_or(Errno::ENOENT)
    }

    fn child(&self, parent: INodeNo, name: &OsStr) -> Result<PathBuf, Errno> {
        if hidden(name) {
            return Err(Errno::ENOENT);
        }
        Ok(self.path(parent)?.join(name))
    }

    fn attr(&self, rel: &Path) -> Result<FileAttr, Errno> {
        let m = fs::symlink_metadata(self.abs(rel)).map_err(errno)?;
        Ok(self.attr_of(self.number(rel), &m))
    }

    fn attr_of(&self, ino: u64, m: &Metadata) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size: m.size(),
            blocks: m.blocks(),
            atime: time(m.atime(), m.atime_nsec()),
            mtime: time(m.mtime(), m.mtime_nsec()),
            ctime: time(m.ctime(), m.ctime_nsec()),
            crtime: UNIX_EPOCH,
            kind: FileType::from_std(m.file_type()).unwrap_or(FileType::RegularFile),
            perm: (m.mode() & 0o7777) as u16,
            nlink: m.nlink() as u32,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn handle(&self, fh: FileHandle) -> Result<Arc<File>, Errno> {
        Self::lock(&self.handles)
            .get(&fh.0)
            .map(|(_, f)| f.clone())
            .ok_or(Errno::EBADF)
    }

    fn new_handle(&self, rel: &Path, file: File) -> u64 {
        let fh = self.next_handle.fetch_add(1, Ordering::Relaxed);
        Self::lock(&self.handles).insert(fh, (rel.to_owned(), Arc::new(file)));
        self.busy.opened(rel);
        fh
    }

    /// A placeholder's content brought here, unless already (the caller
    /// holds the file in `Busy`).
    fn fetched(&self, rel: &Path) -> Result<(), Errno> {
        if !placeholder::is(&self.abs(rel)) {
            return Ok(());
        }
        let Some(fetcher) = lock(&self.fetcher).clone() else {
            tracing::debug!(pair = self.pair.id, path = %rel.display(), "not fetched: the drive isn't syncing");
            return Err(Errno::EIO);
        };
        fetcher.fetch(&self.pair, rel).map_err(|why| {
            tracing::warn!(pair = self.pair.id, path = %rel.display(), "not fetched: {why}");
            Errno::EIO
        })
    }

    fn open(&self, rel: &Path, flags: OpenFlags) -> Result<u64, Errno> {
        let writing = flags.acc_mode() != OpenAccMode::O_RDONLY;
        let truncating = writing && flags.0 & OFlags::TRUNC.bits() as i32 != 0;
        let _taken = self.busy.take(rel);
        let was_placeholder = placeholder::is(&self.abs(rel));
        // Truncated: nothing to fetch.
        if !truncating {
            self.fetched(rel)?;
        }
        let mut options = OpenOptions::new();
        match flags.acc_mode() {
            OpenAccMode::O_RDONLY => options.read(true),
            OpenAccMode::O_WRONLY => options.write(true),
            OpenAccMode::O_RDWR => options.read(true).write(true),
        };
        options.truncate(truncating);
        let file = options.open(self.abs(rel)).map_err(errno)?;
        if truncating && was_placeholder {
            placeholder::unmark(&file).map_err(errno)?;
        }
        if let Some(fetcher) = lock(&self.fetcher).as_ref() {
            fetcher.used(&self.pair, rel);
        }
        Ok(self.new_handle(rel, file))
    }
}

impl Filesystem for Drive {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        // `open` sees O_TRUNC: a placeholder truncated is never fetched.
        let _ = config.add_capabilities(InitFlags::FUSE_ATOMIC_O_TRUNC);
        Ok(())
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.0.note_caller(req, "lookup");
        match self.0.child(parent, name).and_then(|p| self.0.attr(&p)) {
            Ok(a) => reply.entry(&TTL, &a, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.0.note_caller(req, "getattr");
        match self.0.path(ino).and_then(|p| self.0.attr(&p)) {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        self.0.note_caller(req, "setattr");
        let s = &self.0;
        let res = (|| -> Result<FileAttr, Errno> {
            let rel = s.path(ino)?;
            let abs = s.abs(&rel);
            if let Some(m) = mode {
                fs::set_permissions(&abs, fs::Permissions::from_mode(m & 0o7777)).map_err(errno)?;
            }
            if let Some(size) = size {
                let _taken = s.busy.take(&rel);
                // A placeholder's content is needed, unless none is kept.
                if size > 0 {
                    s.fetched(&rel)?;
                }
                let file = match fh.map(|h| s.handle(h)) {
                    Some(Ok(f)) => f,
                    _ => Arc::new(OpenOptions::new().write(true).open(&abs).map_err(errno)?),
                };
                file.set_len(size).map_err(errno)?;
                placeholder::unmark(&file).map_err(errno)?;
            }
            if atime.is_some() || mtime.is_some() {
                let at = |t: TimeOrNow| match t {
                    TimeOrNow::SpecificTime(t) => t,
                    TimeOrNow::Now => SystemTime::now(),
                };
                let mut times = FileTimes::new();
                if let Some(a) = atime {
                    times = times.set_accessed(at(a));
                }
                if let Some(m) = mtime {
                    times = times.set_modified(at(m));
                }
                File::open(&abs)
                    .and_then(|f| f.set_times(times))
                    .map_err(errno)?;
            }
            s.attr(&rel)
        })();
        match res {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        self.0.note_caller(req, "mkdir");
        let res = self.0.child(parent, name).and_then(|rel| {
            fs::DirBuilder::new()
                .mode(mode & !umask)
                .create(self.0.abs(&rel))
                .map_err(errno)?;
            self.0.attr(&rel)
        });
        match res {
            Ok(a) => reply.entry(&TTL, &a, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.0.note_caller(req, "unlink");
        let s = &self.0;
        let res = s.child(parent, name).and_then(|rel| {
            let _taken = s.busy.take(&rel);
            fs::remove_file(s.abs(&rel)).map_err(errno)?;
            Shared::lock(&s.inodes).forget(&rel);
            Ok(())
        });
        match res {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.0.note_caller(req, "rmdir");
        let s = &self.0;
        let res = s.child(parent, name).and_then(|rel| {
            fs::remove_dir(s.abs(&rel)).map_err(errno)?;
            Shared::lock(&s.inodes).forget(&rel);
            Ok(())
        });
        match res {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        self.0.note_caller(req, "rename");
        let s = &self.0;
        let res = (|| -> Result<(), Errno> {
            let from = s.child(parent, name)?;
            let to = s.child(newparent, newname)?;
            let flags = Rename::from_bits(flags.bits()).ok_or(Errno::EINVAL)?;
            // Exchanging two files: a delete and a create to the scan, at
            // best. Refused, as on file systems without it.
            if flags.contains(Rename::EXCHANGE) || flags.contains(Rename::WHITEOUT) {
                return Err(Errno::EINVAL);
            }
            let taken = s.busy.take(&from);
            if flags.contains(Rename::NOREPLACE) {
                local::rename_new(&s.abs(&from), &s.abs(&to)).map_err(errno)?;
            } else {
                fs::rename(s.abs(&from), s.abs(&to)).map_err(errno)?;
            }
            Shared::lock(&s.inodes).moved(&from, &to);
            // Given back first: the opens follow the file, the hold stays
            // with the path it was taken for.
            drop(taken);
            s.busy.moved(&from, &to);
            Ok(())
        })();
        match res {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        self.0.note_caller(req, "open");
        let rel = match self.0.path(ino) {
            Ok(r) => r,
            Err(e) => return reply.error(e),
        };
        // A placeholder's fetch takes its time: answered from its own thread,
        // the session goes on serving the others meanwhile.
        let s = self.0.clone();
        let answer = move || match s.open(&rel, flags) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(e),
        };
        if placeholder::is(&self.0.abs(&self.0.path(ino).unwrap_or_default())) {
            std::thread::spawn(answer);
        } else {
            answer();
        }
    }

    fn read(
        &self,
        req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        self.0.note_caller(req, "read");
        let file = match self.0.handle(fh) {
            Ok(f) => f,
            Err(e) => return reply.error(e),
        };
        let mut buf = vec![0u8; size as usize];
        let mut n = 0;
        while n < buf.len() {
            match file.read_at(&mut buf[n..], offset + n as u64) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) => return reply.error(errno(e)),
            }
        }
        reply.data(&buf[..n]);
    }

    fn write(
        &self,
        req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        self.0.note_caller(req, "write");
        let written = self
            .0
            .handle(fh)
            .and_then(|f| f.write_all_at(data, offset).map_err(errno));
        match written {
            Ok(()) => reply.written(data.len() as u32),
            Err(e) => reply.error(e),
        }
    }

    fn flush(
        &self,
        req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        self.0.note_caller(req, "flush");
        reply.ok();
    }

    fn release(
        &self,
        req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.0.note_caller(req, "release");
        if let Some((rel, _)) = Shared::lock(&self.0.handles).remove(&fh.0) {
            self.0.busy.closed(&rel);
        }
        reply.ok();
    }

    fn fsync(
        &self,
        req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.0.note_caller(req, "fsync");
        let synced = self.0.handle(fh).and_then(|f| {
            if datasync {
                f.sync_data()
            } else {
                f.sync_all()
            }
            .map_err(errno)
        });
        match synced {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(
        &self,
        req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        self.0.note_caller(req, "readdir");
        let s = &self.0;
        let rel = match s.path(ino) {
            Ok(r) => r,
            Err(e) => return reply.error(e),
        };
        let listed = match fs::read_dir(s.abs(&rel)) {
            Ok(listed) => listed,
            Err(e) => return reply.error(errno(e)),
        };
        let mut names: Vec<(std::ffi::OsString, FileType)> = listed
            .flatten()
            .filter(|e| !hidden(&e.file_name()))
            .map(|e| {
                let kind = e
                    .file_type()
                    .ok()
                    .and_then(FileType::from_std)
                    .unwrap_or(FileType::RegularFile);
                (e.file_name(), kind)
            })
            .collect();
        names.sort_by(|a, b| a.0.cmp(&b.0));
        let mut entries = vec![
            (ino.0, FileType::Directory, ".".into()),
            (ino.0, FileType::Directory, "..".into()),
        ];
        for (name, kind) in names {
            entries.push((s.number(&rel.join(&name)), kind, name));
        }
        for (i, (n, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(n), (i + 1) as u64, kind, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        self.0.note_caller(req, "create");
        let s = &self.0;
        let res = (|| -> Result<(FileAttr, u64), Errno> {
            let rel = s.child(parent, name)?;
            let mut options = OpenOptions::new();
            options.read(true).write(true).mode(mode & !umask);
            if flags & OFlags::EXCL.bits() as i32 != 0 {
                options.create_new(true);
            } else {
                options.create(true);
            }
            let file = options.open(s.abs(&rel)).map_err(errno)?;
            let attr = s.attr(&rel)?;
            Ok((attr, s.new_handle(&rel, file)))
        })();
        match res {
            Ok((a, fh)) => {
                reply.created(&TTL, &a, Generation(0), FileHandle(fh), FopenFlags::empty())
            }
            Err(e) => reply.error(e),
        }
    }

    fn statfs(&self, req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        self.0.note_caller(req, "statfs");
        match rustix::fs::statvfs(&self.0.pair.path) {
            Ok(v) => reply.statfs(
                v.f_blocks,
                v.f_bfree,
                v.f_bavail,
                v.f_files,
                v.f_ffree,
                v.f_bsize as u32,
                v.f_namemax as u32,
                v.f_frsize as u32,
            ),
            Err(e) => reply.error(errno(e.into())),
        }
    }
}

/// The mount's file system: the top folder lists the drives, and anything
/// below goes to the drive its inode number's slot names, which answers as
/// the one drive's file system did (`Drive`, its numbers now its own range).
/// The top level is Theca's: nothing is made, renamed or deleted at it.
struct Mux {
    registry: Arc<Registry>,
    uid: u32,
    gid: u32,
}

impl Mux {
    /// The drive a number belongs to.
    fn drive(&self, ino: INodeNo) -> Option<Drive> {
        lock_read(&self.registry.table)
            .by_slot
            .get(&(ino.0 >> SLOT_BITS))
            .map(|l| Drive(l.drive.clone()))
    }

    fn named(&self, name: &OsStr) -> Option<Drive> {
        lock_read(&self.registry.table)
            .by_slot
            .values()
            .find(|l| l.name == name)
            .map(|l| Drive(l.drive.clone()))
    }

    /// The top folder: read-only, as the user owning the mount.
    fn top(&self) -> FileAttr {
        let now = SystemTime::now();
        FileAttr {
            ino: INodeNo(TOP),
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: UNIX_EPOCH,
            kind: FileType::Directory,
            perm: 0o555,
            nlink: 2,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
}

/// Answers `ENOENT` where no drive has the number (withdrawn meanwhile).
macro_rules! routed {
    ($self:ident, $ino:expr, $reply:ident, $drive:ident => $call:expr) => {
        match $self.drive($ino) {
            Some($drive) => $call,
            None => $reply.error(Errno::ENOENT),
        }
    };
}

impl Filesystem for Mux {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        // `open` sees O_TRUNC: a placeholder truncated is never fetched.
        let _ = config.add_capabilities(InitFlags::FUSE_ATOMIC_O_TRUNC);
        Ok(())
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if parent.0 != TOP {
            return routed!(self, parent, reply, d => d.lookup(req, parent, name, reply));
        }
        match self.named(name).map(|d| d.0.attr(Path::new(""))) {
            Some(Ok(a)) => reply.entry(&TTL, &a, Generation(0)),
            Some(Err(e)) => reply.error(e),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        if ino.0 == TOP {
            return reply.attr(&TTL, &self.top());
        }
        routed!(self, ino, reply, d => d.getattr(req, ino, fh, reply))
    }

    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        crtime: Option<SystemTime>,
        chgtime: Option<SystemTime>,
        bkuptime: Option<SystemTime>,
        flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        if ino.0 == TOP {
            return reply.error(Errno::EPERM);
        }
        routed!(self, ino, reply, d => d.setattr(
            req, ino, mode, uid, gid, size, atime, mtime, ctime, fh, crtime, chgtime, bkuptime,
            flags, reply,
        ))
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        if parent.0 == TOP {
            return reply.error(Errno::EACCES);
        }
        routed!(self, parent, reply, d => d.mkdir(req, parent, name, mode, umask, reply))
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if parent.0 == TOP {
            return reply.error(Errno::EPERM);
        }
        routed!(self, parent, reply, d => d.unlink(req, parent, name, reply))
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if parent.0 == TOP {
            return reply.error(Errno::EPERM);
        }
        routed!(self, parent, reply, d => d.rmdir(req, parent, name, reply))
    }

    fn rename(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if parent.0 == TOP || newparent.0 == TOP {
            return reply.error(Errno::EPERM);
        }
        // Two drives are two libraries: a move across is a copy, which the
        // caller does itself.
        if parent.0 >> SLOT_BITS != newparent.0 >> SLOT_BITS {
            return reply.error(Errno::EXDEV);
        }
        routed!(self, parent, reply, d => d.rename(req, parent, name, newparent, newname, flags, reply))
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        routed!(self, ino, reply, d => d.open(req, ino, flags, reply))
    }

    fn read(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        flags: OpenFlags,
        lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        routed!(self, ino, reply, d => d.read(req, ino, fh, offset, size, flags, lock_owner, reply))
    }

    fn write(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        write_flags: WriteFlags,
        flags: OpenFlags,
        lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        routed!(self, ino, reply, d => d.write(
            req, ino, fh, offset, data, write_flags, flags, lock_owner, reply,
        ))
    }

    fn flush(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        routed!(self, ino, reply, d => d.flush(req, ino, fh, lock_owner, reply))
    }

    fn release(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        flags: OpenFlags,
        lock_owner: Option<LockOwner>,
        flush: bool,
        reply: ReplyEmpty,
    ) {
        routed!(self, ino, reply, d => d.release(req, ino, fh, flags, lock_owner, flush, reply))
    }

    fn fsync(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        routed!(self, ino, reply, d => d.fsync(req, ino, fh, datasync, reply))
    }

    fn readdir(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        if ino.0 != TOP {
            return routed!(self, ino, reply, d => d.readdir(req, ino, fh, offset, reply));
        }
        let mut drives: Vec<(OsString, u64)> = lock_read(&self.registry.table)
            .by_slot
            .iter()
            .map(|(slot, l)| (l.name.clone(), (slot << SLOT_BITS) + 1))
            .collect();
        drives.sort();
        let mut entries = vec![
            (TOP, FileType::Directory, OsString::from(".")),
            (TOP, FileType::Directory, OsString::from("..")),
        ];
        entries.extend(
            drives
                .into_iter()
                .map(|(name, n)| (n, FileType::Directory, name)),
        );
        for (i, (n, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(n), (i + 1) as u64, kind, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        if parent.0 == TOP {
            return reply.error(Errno::EACCES);
        }
        routed!(self, parent, reply, d => d.create(req, parent, name, mode, umask, flags, reply))
    }

    fn statfs(&self, req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        if ino.0 != TOP {
            return routed!(self, ino, reply, d => d.statfs(req, ino, reply));
        }
        // The top has no disk of its own: the first drive's.
        let first = lock_read(&self.registry.table)
            .by_slot
            .values()
            .map(|l| l.drive.clone())
            .next();
        match first {
            Some(d) => Drive(d).statfs(req, ino, reply),
            None => reply.statfs(0, 0, 0, 0, 0, 4096, 255, 4096),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::own;

    /// This thread's ID (`/proc/thread-self` is `<pid>/task/<tid>`).
    fn tid() -> u32 {
        let me = std::fs::read_link("/proc/thread-self").unwrap();
        me.file_name().unwrap().to_str().unwrap().parse().unwrap()
    }

    /// A thread of this process is its own; the kernel (0) and another
    /// process (the parent) aren't. (A thread ended isn't checked: its entry
    /// can outlive its `join`.)
    #[test]
    fn a_caller_is_own_only_when_a_thread_of_this_process() {
        assert!(own(tid()));
        assert!(own(std::process::id()), "the main thread");
        assert!(!own(0));
        assert!(!own(std::os::unix::process::parent_id()));
    }
}
