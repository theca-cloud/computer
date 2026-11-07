//! The local side: scanning a pair's folder (one `stat` per entry, no reads),
//! names compared like the server's, file IDs (docs/decisions.md, Sync).

use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};

use theca_common::name::is_ignored;

/// What a download writes before moving it into place.
pub const PART_SUFFIX: &str = ".theca-part";

/// A name as the server compares it (NFC, then PostgreSQL's `lower()`): two
/// local names with the same key can't both be synced.
pub use theca_common::name::key;

/// Names the scan skips: what operating systems leave, unfinished downloads.
pub fn skipped(name: &str) -> bool {
    is_ignored(name) || is_part(name)
}

/// A download's part file, as named (`.<name>.<8 hex>.theca-part`): never a
/// user's own `x.theca-part`, which is synced like any file (skipped, it
/// would be deleted with its folder, and it was never uploaded).
fn is_part(name: &str) -> bool {
    name.strip_suffix(PART_SUFFIX)
        .filter(|_| name.starts_with('.'))
        .and_then(|rest| rest.rsplit_once('.'))
        .is_some_and(|(_, random)| {
            random.len() == 8 && random.bytes().all(|b| b.is_ascii_hexdigit())
        })
}

/// A file or folder found by a scan.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// Below the pair's folder, each name spelled as on disk.
    pub path: Vec<String>,
    pub is_file: bool,
    pub size: i64,
    /// Nanoseconds since the epoch.
    pub mtime: i64,
    pub file_id: Option<String>,
}

impl Item {
    pub fn name(&self) -> &str {
        self.path.last().map_or("", String::as_str)
    }

    pub fn display(&self) -> String {
        self.path.join("/")
    }
}

/// What a scan found: items parents first, and what it skipped.
#[derive(Debug, Default)]
pub struct Scan {
    pub items: Vec<Item>,
    /// Symbolic links (never followed: they can leave the folder, or loop).
    pub links: Vec<String>,
    /// Folders that couldn't be read, with why.
    pub unreadable: Vec<(String, String)>,
    /// Folders on another disk than the pair's (a disk mounted inside it):
    /// skipped, so unmounting it deletes nothing.
    pub elsewhere: Vec<String>,
}

/// Walks the pair's folder. Ignored files (what operating systems leave) and
/// unfinished downloads are skipped.
pub fn scan(root: &Path) -> io::Result<Scan> {
    let mut scan = Scan::default();
    #[cfg(unix)]
    let device = std::os::unix::fs::MetadataExt::dev(&std::fs::metadata(root)?);
    // Each folder opened when its turn comes, one at a time: a folder of
    // hundreds of subfolders would otherwise hold as many open at once, past
    // macOS's 256.
    let mut pending = vec![(Vec::<String>::new(), root.to_owned())];
    while let Some((prefix, full)) = pending.pop() {
        let dir = match std::fs::read_dir(&full) {
            Ok(dir) => dir,
            // The root itself must be readable: otherwise the pair stops.
            Err(e) if prefix.is_empty() => return Err(e),
            Err(e) => {
                scan.unreadable.push((prefix.join("/"), e.to_string()));
                continue;
            }
        };
        let mut children = Vec::new();
        for entry in dir {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    scan.unreadable.push((prefix.join("/"), e.to_string()));
                    continue;
                }
            };
            let Ok(name) = entry.file_name().into_string() else {
                // Not UTF-8: no name on the server can match it (listed under
                // its own path, which no base node has, not its folder's).
                let mut path = prefix.clone();
                path.push(entry.file_name().to_string_lossy().into_owned());
                scan.unreadable
                    .push((path.join("/"), "not a UTF-8 name".into()));
                continue;
            };
            if skipped(&name) {
                continue;
            }
            let mut path = prefix.clone();
            path.push(name);
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(e) => {
                    scan.unreadable.push((path.join("/"), e.to_string()));
                    continue;
                }
            };
            if meta.is_symlink() {
                scan.links.push(path.join("/"));
                continue;
            }
            children.push((path, meta, entry.path()));
        }
        // Parents first, and a stable order.
        children.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, meta, full) in children {
            let is_file = meta.is_file();
            // ponytail: Unix only; on Windows a volume mounted in a folder is a
            // reparse point, not told apart yet.
            #[cfg(unix)]
            if !is_file && std::os::unix::fs::MetadataExt::dev(&meta) != device {
                scan.elsewhere.push(path.join("/"));
                continue;
            }
            scan.items.push(item(path.clone(), &meta, &full, is_file));
            if !is_file {
                pending.push((path, full));
            }
        }
    }
    // A depth-first walk with a stack: sort so parents still come first.
    scan.items.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(scan)
}

fn item(path: Vec<String>, meta: &Metadata, full: &Path, is_file: bool) -> Item {
    Item {
        path,
        is_file,
        size: if is_file { meta.len() as i64 } else { 0 },
        mtime: mtime(meta),
        file_id: file_id(full),
    }
}

pub fn mtime(meta: &Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| chrono::DateTime::<chrono::Utc>::from(t).timestamp_nanos_opt())
        .unwrap_or_default()
}

/// A file's identity in its pair (the inode, or the volume serial and NTFS's
/// file ID): what tells a move from a delete and a create. Without the device
/// on Unix: every file of a pair is on its root's (another disk inside is
/// skipped), and a disk remounted takes another device number.
pub fn file_id(path: &Path) -> Option<String> {
    file_id::get_file_id(path).ok().map(|id| match id {
        file_id::FileId::Inode { inode_number, .. } => inode_number.to_string(),
        file_id::FileId::LowRes {
            volume_serial_number,
            file_index,
        } => format!("{volume_serial_number}:{file_index}"),
        file_id::FileId::HighRes {
            volume_serial_number,
            file_id,
        } => format!("{volume_serial_number}:{file_id}"),
    })
}

/// A pair's root's identity: its file ID, and on Unix its device too
/// (`device:inode`), so that another disk mounted at the same place (every
/// ext4 root is inode 2) isn't taken for it.
pub fn root_id(path: &Path) -> Option<String> {
    match file_id::get_file_id(path).ok()? {
        file_id::FileId::Inode {
            device_id,
            inode_number,
        } => Some(format!("{device_id}:{inode_number}")),
        _ => file_id(path),
    }
}

/// Two root IDs of the same inode on different devices (`device:inode`): the
/// same disk remounted, or another one mounted at the same place.
pub fn remounted(known: &str, now: &str) -> bool {
    // Windows' IDs are `serial:index`: a volume serial stays with its volume.
    if cfg!(windows) {
        return false;
    }
    match (known.split_once(':'), now.split_once(':')) {
        (Some((d1, i1)), Some((d2, i2))) => i1 == i2 && d1 != d2,
        _ => false,
    }
}

/// The local path of `steps` below `root`: each a name as the server spells
/// it, and the file ID the base has for it. Found as the plan claims (`plan.rs`,
/// `match_local`): among the spellings of the name on disk (NFC/NFD, case),
/// the one with the base's file ID, else the exact name, else the only one.
/// Anything else is absent: the path as spelled, where nothing is. Never the
/// first spelling listed, which on a disk telling case apart (ext4) could be an
/// unsynced variant beside the synced file.
pub fn resolve(root: &Path, steps: &[(&str, Option<&str>)]) -> PathBuf {
    let mut path = root.to_path_buf();
    for &(name, id) in steps {
        let exact = path.join(name);
        let here = exact.symlink_metadata().is_ok();
        // The common case, without listing the folder.
        if here && id.is_none_or(|id| file_id(&exact).as_deref() == Some(id)) {
            path = exact;
            continue;
        }
        let wanted = key(name);
        let spellings: Vec<std::ffi::OsString> = std::fs::read_dir(&path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name())
            .filter(|n| n.to_str().is_some_and(|n| key(n) == wanted))
            .collect();
        let by_id = id.and_then(|id| {
            spellings
                .iter()
                .find(|n| file_id(&path.join(n)).as_deref() == Some(id))
        });
        path = match (by_id, spellings.as_slice()) {
            (Some(on_disk), _) => path.join(on_disk),
            (None, _) if here => exact,
            (None, [only]) => path.join(only),
            (None, _) => exact,
        };
    }
    path
}

/// Renames `from` to `to` only if nothing is at `to` (`AlreadyExists`
/// otherwise): atomic where the OS and the disk can, so a file put there a
/// moment before is never replaced. ponytail: a disk that can't (some network
/// and FUSE file systems) checks, then renames.
pub fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    renamore::rename_exclusive_fallback(from, to).map(|_| ())
}

/// Whether the file at `path` is still the one the base recorded, by the
/// plan's fingerprint: size, mtime, and the file ID when the base has one. A
/// file is deleted or written over only then: the file the plan saw, never one
/// put there since.
pub fn unchanged(path: &Path, size: i64, mtime: i64, id: Option<&str>) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| {
        m.len() as i64 == size
            && self::mtime(&m) == mtime
            && id.is_none_or(|id| file_id(path).as_deref() == Some(id))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_ignore_case_and_normalization() {
        assert_eq!(key("Café.TXT"), key("cafe\u{301}.txt"));
        assert_ne!(key("a.txt"), key("b.txt"));
    }

    #[test]
    fn scan_skips_what_isnt_synced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/c.txt"), "c").unwrap();
        std::fs::write(root.join("a/.DS_Store"), "x").unwrap();
        std::fs::write(root.join(format!(".d.txt.0123abcd{PART_SUFFIX}")), "x").unwrap();
        // A user's own file of that suffix is theirs: synced.
        std::fs::write(root.join(format!("mine{PART_SUFFIX}")), "x").unwrap();
        std::fs::write(root.join(".hidden"), "h").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("a"), root.join("link")).unwrap();
        let scan = scan(root).unwrap();
        let paths: Vec<String> = scan.items.iter().map(Item::display).collect();
        assert_eq!(
            paths,
            [".hidden", "a", "a/b", "a/b/c.txt", "mine.theca-part"]
        );
        #[cfg(unix)]
        assert_eq!(scan.links, ["link"]);
        let c = &scan.items[3];
        assert!(c.is_file && c.size == 1 && c.mtime > 0 && c.file_id.is_some());
    }

    #[test]
    fn resolve_finds_the_spelling_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let nfd = "cafe\u{301}";
        std::fs::create_dir(dir.path().join(nfd)).unwrap();
        std::fs::write(dir.path().join(nfd).join("A.txt"), "a").unwrap();
        let found = resolve(dir.path(), &[("café", None), ("a.txt", None)]);
        assert_eq!(std::fs::read(&found).unwrap(), b"a");
        // Missing: the server's spelling.
        let missing = resolve(dir.path(), &[("new", None)]);
        assert_eq!(missing, dir.path().join("new"));
    }

    /// Two spellings of a node's name on disk, neither exact: the base's file
    /// ID tells which is the synced file, and without one neither is taken.
    /// The first one listed was, and could be an unsynced file a remote
    /// delete then removed (a disk telling case and normalization apart: ext4).
    #[test]
    fn two_spellings_are_told_by_file_id_or_neither() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cafe\u{301}.txt"), "").unwrap();
        std::fs::write(dir.path().join("CAFE\u{301}.txt"), "").unwrap();
        if std::fs::read_dir(dir.path()).unwrap().count() < 2 {
            eprintln!("skipped: {} ignores case", dir.path().display());
            return;
        }
        let found = resolve(dir.path(), &[("café.txt", None)]);
        assert!(!found.exists(), "{found:?}");
        // The base's file ID tells, whichever is listed first.
        let synced = dir.path().join("CAFE\u{301}.txt");
        let id = file_id(&synced);
        assert_eq!(resolve(dir.path(), &[("café.txt", id.as_deref())]), synced);
    }

    /// A file replaced by another of the same size and mtime (an atomic save,
    /// a copy restoring times) is not the synced one: only its file ID says.
    #[test]
    fn a_replaced_file_is_not_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "a").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let (size, at) = (meta.len() as i64, mtime(&meta));
        let id = file_id(&path);
        assert!(unchanged(&path, size, at, id.as_deref()));
        let other = dir.path().join("b.txt");
        std::fs::write(&other, "b").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&other)
            .unwrap()
            .set_modified(meta.modified().unwrap())
            .unwrap();
        std::fs::rename(&other, &path).unwrap();
        assert!(!unchanged(&path, size, at, id.as_deref()));
        // Without a file ID in the base, size and mtime decide.
        assert!(unchanged(&path, size, at, None));
    }

    /// B2: a folder of many subfolders is scanned with few files open (run
    /// again in a shell lowering the limit: it holds for the whole process).
    #[cfg(unix)]
    #[test]
    fn a_wide_folder_scans_with_few_open_files() {
        if std::env::var_os("THECA_FEW_FILES").is_none() {
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg("ulimit -n 64 && exec \"$0\" --exact local::tests::a_wide_folder_scans_with_few_open_files")
                .arg(std::env::current_exe().unwrap())
                .env("THECA_FEW_FILES", "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        for i in 0..200 {
            std::fs::create_dir(dir.path().join(format!("d{i}"))).unwrap();
            std::fs::write(dir.path().join(format!("d{i}/f")), "x").unwrap();
        }
        let scan = scan(dir.path()).unwrap();
        assert_eq!(scan.unreadable, []);
        assert_eq!(scan.items.len(), 400);
    }

    /// B3: a disk mounted inside the folder is listed, not walked: unmounting
    /// it then deletes nothing (macOS: a disk image, no root needed).
    #[cfg(target_os = "macos")]
    #[test]
    fn another_disk_inside_is_not_walked() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("disk.dmg");
        let mount = dir.path().join("pair/mounted");
        std::fs::create_dir_all(&mount).unwrap();
        let hdiutil = |args: &[&std::ffi::OsStr]| {
            let ok = std::process::Command::new("hdiutil")
                .args(args)
                .output()
                .unwrap();
            assert!(
                ok.status.success(),
                "{}",
                String::from_utf8_lossy(&ok.stderr)
            );
        };
        hdiutil(&[
            "create".as_ref(),
            "-size".as_ref(),
            "2m".as_ref(),
            "-fs".as_ref(),
            "HFS+".as_ref(),
            image.as_os_str(),
        ]);
        hdiutil(&[
            "attach".as_ref(),
            "-nobrowse".as_ref(),
            "-mountpoint".as_ref(),
            mount.as_os_str(),
            image.as_os_str(),
        ]);
        std::fs::write(mount.join("a.txt"), "on the other disk").unwrap();
        let scan = scan(&dir.path().join("pair"));
        hdiutil(&["detach".as_ref(), mount.as_os_str()]);
        let scan = scan.unwrap();
        assert_eq!(scan.elsewhere, ["mounted"]);
        assert!(scan.items.is_empty(), "{:?}", scan.items);
    }
}
