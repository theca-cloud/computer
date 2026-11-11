//! A FUSE drive's placeholders (Linux): files in the backing folder with
//! their size and mtime but no data, marked by an extended attribute, since
//! their zeros can't say it. Each is written as a part file, then renamed in,
//! and an eviction marks the file before dropping its data: a crash never
//! leaves zeros unmarked, nor a file missing.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use rustix::fs::{FallocateFlags, XattrFlags};

const ATTRIBUTE: &str = "user.theca.placeholder";

/// The file at `path` is a placeholder (a link is never one).
pub fn is(path: &Path) -> bool {
    rustix::fs::lgetxattr_query(path, ATTRIBUTE).is_ok()
}

/// Makes a new file (a part file, before it is renamed in) a placeholder of
/// `size` bytes.
pub fn write(file: &File, size: u64) -> io::Result<()> {
    file.set_len(size)?;
    mark(file)
}

/// The file system holding `folder` keeps the mark: tried on the folder
/// itself, then taken off.
pub fn supported(folder: &Path) -> io::Result<()> {
    let folder = File::open(folder)?;
    mark(&folder)?;
    Ok(rustix::fs::fremovexattr(&folder, ATTRIBUTE)?)
}

/// A placeholder truncated through the mount: its content is now real
/// (empty), unmarked so that it is uploaded.
pub fn unmark(file: &File) -> io::Result<()> {
    match rustix::fs::fremovexattr(file, ATTRIBUTE) {
        Err(rustix::io::Errno::NODATA) | Ok(()) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn mark(file: &File) -> io::Result<()> {
    Ok(rustix::fs::fsetxattr(
        file,
        ATTRIBUTE,
        b"",
        XattrFlags::empty(),
    )?)
}

/// Evicts a downloaded file in place (its inode, size and mtime kept):
/// marked first, then its data dropped. A crash in between leaves it marked
/// with its data still there.
pub fn evict(path: &Path) -> io::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    let meta = file.metadata()?;
    mark(&file)?;
    crate::crash_point("evicted-marked");
    rustix::fs::fallocate(
        &file,
        FallocateFlags::PUNCH_HOLE | FallocateFlags::KEEP_SIZE,
        0,
        meta.len(),
    )?;
    file.set_modified(meta.modified()?)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;

    use super::*;

    /// A placeholder has its size, no data, and says so; a file evicted keeps
    /// its inode, size and mtime, and says so too; a plain file doesn't, nor a
    /// rename lose it.
    #[test]
    fn a_placeholder_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        let file = File::create_new(&path).unwrap();
        write(&file, 5_000_000).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!((meta.len(), meta.blocks()), (5_000_000, 0));
        assert!(is(&path));
        let moved = dir.path().join("b.bin");
        std::fs::rename(&path, &moved).unwrap();
        assert!(is(&moved));

        let plain = dir.path().join("c.bin");
        File::create_new(&plain)
            .unwrap()
            .write_all(&[7; 100_000])
            .unwrap();
        assert!(!is(&plain));
        let before = std::fs::metadata(&plain).unwrap();
        evict(&plain).unwrap();
        let after = std::fs::metadata(&plain).unwrap();
        assert!(is(&plain));
        assert_eq!(
            (after.ino(), after.len(), after.modified().unwrap()),
            (before.ino(), before.len(), before.modified().unwrap())
        );
        // Whole blocks are freed: the partial last one stays, zeroed.
        assert!(after.blocks() <= 8, "{} blocks", after.blocks());
        assert!(std::fs::read(&plain).unwrap().iter().all(|b| *b == 0));
        assert!(!is(&dir.path().join("missing")));
        supported(dir.path()).unwrap();
        assert!(!is(dir.path()), "the folder tried, unmarked");
    }
}
