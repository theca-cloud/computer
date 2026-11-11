//! A FUSE drive's file system (Linux, `fuser`): mounted on the drive's
//! folder, every operation forwarded to its backing folder, where its rounds
//! work. A placeholder opened is fetched first, on a thread of its own (the
//! session goes on answering); what is open or fetched is busy for the
//! rounds (`Busy`). Part files never show. The kernel keeps attributes and
//! names a second only: rounds change the backing folder behind its back.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File, FileTimes, Metadata, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
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
use crate::local::{self, PART_SUFFIX};
use crate::state::Pair;

const TTL: Duration = Duration::from_secs(1);

/// A drive mounted: unmounted when dropped.
pub struct Mount {
    path: PathBuf,
    session: Option<BackgroundSession>,
    pub busy: Busy,
}

/// Mounts `pair` (as its rounds see it: its path the backing folder) on
/// `folder`: first clearing a mount a crash left there (`ENOTCONN`, nothing
/// saved into it).
pub fn mount(pair: &Pair, folder: &Path, fetcher: Fetcher) -> io::Result<Mount> {
    unmount(folder);
    let meta = fs::metadata(&pair.path)?;
    let busy = Busy::default();
    let drive = Drive(Arc::new(Shared {
        pair: pair.clone(),
        inodes: Mutex::new(Inodes::new()),
        handles: Mutex::new(HashMap::new()),
        next_handle: AtomicU64::new(1),
        busy: busy.clone(),
        fetcher,
        uid: meta.uid(),
        gid: meta.gid(),
    }));
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName(format!("theca-{}", pair.id)),
        MountOption::Subtype("theca".into()),
        MountOption::DefaultPermissions,
        MountOption::NoDev,
        MountOption::NoSuid,
    ];
    config.acl = SessionACL::Owner;
    // Several workers: an operation waiting on a file the round holds (a
    // kept download, an upload) leaves the rest of the mount answering.
    config.n_threads = Some(4);
    let session = fuser::spawn_mount(drive, folder, &config)?;
    Ok(Mount {
        path: folder.to_owned(),
        session: Some(session),
        busy,
    })
}

/// A drive's mount cleared from outside its engine (removing the drive):
/// that engine's session then ends.
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

impl Drop for Mount {
    fn drop(&mut self) {
        unmount(&self.path);
        if let Some(session) = self.session.take() {
            let _ = session.join();
        }
    }
}

/// A drive's own folder, while it isn't mounted, holds nothing but ignored
/// names (what operating systems leave): anything else was saved there while
/// Theca wasn't running, and the mount would hide it. A mount a crash left is
/// cleared first; a folder gone is made again (it is only where it mounts).
pub fn bare(folder: &Path) -> Result<(), String> {
    unmount(folder);
    if !folder.exists() {
        fs::create_dir_all(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
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

/// Inode numbers by path below the backing folder, the root 1: the kernel
/// names files by them.
struct Inodes {
    paths: HashMap<u64, PathBuf>,
    numbers: HashMap<PathBuf, u64>,
    next: u64,
}

impl Inodes {
    fn new() -> Self {
        let mut i = Inodes {
            paths: HashMap::new(),
            numbers: HashMap::new(),
            next: 2,
        };
        i.paths.insert(1, PathBuf::new());
        i.numbers.insert(PathBuf::new(), 1);
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
    fetcher: Fetcher,
    uid: u32,
    gid: u32,
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
    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
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
        self.fetcher.fetch(&self.pair, rel).map_err(|why| {
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
        self.fetcher.used(&self.pair, rel);
        Ok(self.new_handle(rel, file))
    }
}

impl Filesystem for Drive {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        // `open` sees O_TRUNC: a placeholder truncated is never fetched.
        let _ = config.add_capabilities(InitFlags::FUSE_ATOMIC_O_TRUNC);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.0.child(parent, name).and_then(|p| self.0.attr(&p)) {
            Ok(a) => reply.entry(&TTL, &a, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.0.path(ino).and_then(|p| self.0.attr(&p)) {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
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
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
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

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
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

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
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
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
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

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
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
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
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
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
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
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if let Some((rel, _)) = Shared::lock(&self.0.handles).remove(&fh.0) {
            self.0.busy.closed(&rel);
        }
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
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
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
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
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
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

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
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
