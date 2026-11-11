//! What drives share on every OS (sprint 16): the drive limit, and which
//! downloaded files leave first to keep under it.

use std::collections::HashSet;
use std::io;
use std::path::Path;

use crate::local::{self, Scan};
use crate::state::{self, State};

pub mod busy;
#[cfg(windows)]
pub mod cloud;
pub mod fetch;
#[cfg(target_os = "linux")]
pub mod fuse;
#[cfg(target_os = "linux")]
pub mod placeholder;
pub mod provider;
pub mod runs;

/// The default limit: at most this, and at most a tenth of the disk holding
/// the client state.
const DEFAULT_CAP: u64 = 10 * 1024 * 1024 * 1024;

/// The drive limit, in bytes: the one set (`theca drive-limit`), else the
/// default.
pub fn limit(state: &State) -> state::Result<u64> {
    Ok(state
        .drive_limit()?
        .unwrap_or_else(|| default_limit(state.path())))
}

/// 10 GiB or a tenth of the disk holding the state, the smaller; 10 GiB when
/// that disk can't be measured (an in-memory state).
pub fn default_limit(state: Option<&Path>) -> u64 {
    let disk = state
        .and_then(Path::parent)
        .and_then(|folder| fs4::total_space(folder).ok());
    disk.map_or(DEFAULT_CAP, |total| (total / 10).min(DEFAULT_CAP))
}

/// A FUSE drive's backing folder, `<data dir>/drives/<pair>/`, beside the
/// state (none for an in-memory one).
#[cfg(target_os = "linux")]
pub fn backing(state: &State, pair: i64) -> Option<std::path::PathBuf> {
    let data = state.path()?.parent()?;
    Some(data.join("drives").join(pair.to_string()))
}

/// A drive served while its engine runs: mounted (FUSE), or connected
/// (Windows' sync root); what it serves is busy for the rounds.
#[cfg(target_os = "linux")]
pub type Served = fuse::Mount;
#[cfg(windows)]
pub type Served = cloud::Root;

/// `pair` (as its rounds see it) served on its own folder: a FUSE drive
/// mounted once that folder is bare; a Windows drive's sync root connected.
/// Why not, otherwise (the pair stops for its folder).
#[cfg(any(target_os = "linux", windows))]
pub fn serve(
    pair: &crate::state::Pair,
    folder: &Path,
    fetcher: fetch::Fetcher,
) -> Result<Served, String> {
    #[cfg(target_os = "linux")]
    return fuse::bare(folder).and_then(|()| {
        fuse::mount(pair, folder, fetcher).map_err(|e| {
            let fuse = if fuse::available() {
                ""
            } else {
                " (install fuse3)"
            };
            format!("{} can't be mounted{fuse}: {e}", folder.display())
        })
    });
    #[cfg(windows)]
    return cloud::connect(pair, fetcher)
        .map_err(|e| format!("{} can't be connected: {e}", folder.display()));
}

/// Where a drive's rounds work: a FUSE drive's backing folder, a Windows
/// drive's own folder (its placeholders in place).
#[cfg(any(target_os = "linux", windows))]
pub fn root_of(state: &State, pair: &crate::state::Pair) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    return backing(state, pair.id);
    #[cfg(windows)]
    {
        let _ = state;
        Some(pair.path.clone())
    }
}

/// A drive's file whose content isn't here: as the file itself says.
#[cfg(any(target_os = "linux", windows))]
pub fn is_placeholder(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    return placeholder::is(path);
    #[cfg(windows)]
    return cloud::is_placeholder(path);
}

/// A drive's downloaded file (`node`) evicted in place: it stays, its
/// content gone.
#[cfg(any(target_os = "linux", windows))]
fn evict(path: &Path, node: uuid::Uuid) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let _ = node;
        placeholder::evict(path)
    }
    #[cfg(windows)]
    return cloud::evict(path, node);
}

/// A drive's file evicted if it is still as synced (`e`), checked here, once
/// the caller holds it from the mount (opens wait on that hold): an edit
/// saved after the caller looked is never evicted. False: not evicted, an
/// edit since.
#[cfg(any(target_os = "linux", windows))]
pub fn evict_synced(path: &Path, e: &state::Entry) -> io::Result<bool> {
    if !local::unchanged(path, e.size, e.mtime, e.file_id.as_deref()) {
        return Ok(false);
    }
    evict(path, e.node).map(|()| true)
}

/// Where a drive's removal is journaled: `<data dir>/drives/<pair>.removal`.
#[cfg(any(target_os = "linux", windows))]
pub fn journal(state: &State, pair: i64) -> Option<std::path::PathBuf> {
    let data = state.path()?.parent()?;
    Some(data.join("drives").join(format!("{pair}.removal")))
}

/// A drive's removal, journaled in Theca's data folder
/// (`drives/<pair>.removal`) before the pair leaves the state: what to move
/// out and where, done again by whoever finds it after a crash (the next
/// add, remove, or engine start). Without it, edits not uploaded would stay
/// in an orphan folder nothing knows, and a later drive reusing the pair's
/// ID would start in it.
#[cfg(any(target_os = "linux", windows))]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Removal {
    pub pair: i64,
    /// Its library (a Windows drive's sync root is named after it).
    pub library: uuid::Uuid,
    pub journal: std::path::PathBuf,
    /// The drive's own folder (its mount cleared, its sync root
    /// unregistered).
    pub mount: std::path::PathBuf,
    /// Where its rounds worked (`root_of`): deleted last.
    pub backing: std::path::PathBuf,
    /// `<default folder>/<library> (not uploaded)`.
    pub aside: std::path::PathBuf,
    /// Below the backing folder: edits not uploaded yet.
    pub waiting: Vec<std::path::PathBuf>,
}

#[cfg(any(target_os = "linux", windows))]
impl Removal {
    /// Written (atomically) before the pair is forgotten.
    pub fn begin(&self) -> io::Result<()> {
        let journal = &self.journal;
        if let Some(folder) = journal.parent() {
            std::fs::create_dir_all(folder)?;
        }
        let part = journal.with_extension("removal.part");
        // Written and flushed through one handle: Windows flushes only one
        // with write access.
        let mut file = std::fs::File::create(&part)?;
        io::Write::write_all(
            &mut file,
            &serde_json::to_vec(self).map_err(io::Error::other)?,
        )?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&part, journal)
    }

    /// The edits moved out (never over a file there), the mount cleared,
    /// the backing folder and the journal deleted: the files moved.
    pub fn finish(&self) -> io::Result<usize> {
        let step = |what: &'static str| {
            move |e: io::Error| io::Error::new(e.kind(), format!("{what}: {e}"))
        };
        #[cfg(target_os = "linux")]
        fuse::clear(&self.mount);
        // Windows deletes the placeholders not downloaded, leaves the others.
        #[cfg(windows)]
        cloud::unregister(self.library).map_err(step("its sync root not removed"))?;
        let mut moved = 0;
        for rel in &self.waiting {
            let (from, to) = (self.backing.join(rel), self.aside.join(rel));
            if from.symlink_metadata().is_err() {
                // Moved before a crash (a link checked itself, not its target).
                continue;
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).map_err(step("its edits' folder not made"))?;
            }
            match local::rename_new(&from, &to) {
                // Theca's data on another disk than the default folder.
                Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                    std::fs::File::create_new(&to)
                        .and_then(|_| std::fs::copy(&from, &to))
                        .and_then(|_| std::fs::remove_file(&from))
                        .map_err(step("an edit not copied out"))?;
                }
                other => other.map_err(step("an edit not moved out"))?,
            }
            moved += 1;
        }
        crate::crash_point("drive-removal-moved");
        match std::fs::remove_dir_all(&self.backing) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(step("its folder not deleted")(e));
            }
            _ => {}
        }
        std::fs::remove_file(&self.journal).map_err(step("its journal not deleted"))?;
        Ok(moved)
    }
}

/// Drive removals a crash left half done, finished (the drives folder
/// beside `state`); what can't be finished stays, logged.
#[cfg(any(target_os = "linux", windows))]
pub fn finish_removals(state: &State) {
    let Some(drives) = state
        .path()
        .and_then(Path::parent)
        .map(|d| d.join("drives"))
    else {
        return;
    };
    let Ok(listed) = std::fs::read_dir(&drives) else {
        return;
    };
    for entry in listed.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "removal") {
            continue;
        }
        let finished = std::fs::read(&path)
            .and_then(|j| serde_json::from_slice::<Removal>(&j).map_err(io::Error::other))
            .and_then(|r| r.finish());
        match finished {
            Ok(moved) => {
                tracing::info!(journal = %path.display(), moved, "a drive's removal finished")
            }
            Err(e) => {
                tracing::error!(journal = %path.display(), "a drive's removal not finished: {e}")
            }
        }
    }
}

/// A drive's folder scanned: a sync pair's scan, each file saying whether
/// its content is here (a placeholder's isn't).
pub fn scan(root: &Path, known: &HashSet<Vec<String>>) -> io::Result<Scan> {
    #[allow(unused_mut)] // macOS has no scan of its drives.
    let mut scan = local::scan_entering(root, known)?;
    #[cfg(any(target_os = "linux", windows))]
    for item in scan.items.iter_mut() {
        let path = item.path.iter().fold(root.to_owned(), |p, n| p.join(n));
        if item.is_file {
            item.downloaded = !is_placeholder(&path);
        }
        // Windows keeps the pin on the file ("Always keep on this device").
        #[cfg(windows)]
        {
            item.kept = cloud::is_pinned(&path);
        }
    }
    Ok(scan)
}

/// A drive's downloaded files (Linux, Windows) the limit counts (by path below its
/// backing folder): not kept (nor below a folder kept), and as synced (an
/// edit not uploaded yet never leaves). `open`: open through its mount.
#[cfg(any(target_os = "linux", windows))]
pub fn downloaded(
    state: &State,
    pair: &crate::state::Pair,
    open: impl Fn(&Path) -> bool,
) -> state::Result<Vec<Downloaded<std::path::PathBuf>>> {
    use std::collections::HashMap;
    let Some(backing) = root_of(state, pair) else {
        return Ok(Vec::new());
    };
    let entries = state.entries(pair.id)?;
    let kept = state.kept(pair.id)?;
    let used = state.used(pair.id)?;
    let by_node: HashMap<_, _> = entries.iter().map(|e| (e.node, e)).collect();
    // A node's path below the root, and whether it or a folder above is kept.
    let place = |mut node| {
        let mut names = Vec::new();
        let mut is_kept = false;
        while node != pair.root {
            let e: &crate::state::Entry = by_node.get(&node)?;
            is_kept |= kept.contains(&node);
            names.push(e.name.clone());
            node = e.parent;
            if names.len() > crate::plan::MAX_DEPTH {
                return None;
            }
        }
        names.reverse();
        Some((names.iter().collect::<std::path::PathBuf>(), is_kept))
    };
    let mut files = Vec::new();
    for e in entries.iter().filter(|e| e.is_file) {
        let Some((rel, false)) = place(e.node) else {
            continue;
        };
        let abs = backing.join(&rel);
        if is_placeholder(&abs) || !local::unchanged(&abs, e.size, e.mtime, e.file_id.as_deref()) {
            continue;
        }
        // Windows' own keep (Explorer's "Always keep on this device") is the
        // pin on the file or a folder above, not in the state.
        #[cfg(windows)]
        if rel.ancestors().any(|a| cloud::is_pinned(&backing.join(a))) {
            continue;
        }
        files.push(Downloaded {
            size: u64::try_from(e.size).unwrap_or_default(),
            used: used.get(&e.node).copied().unwrap_or_default(),
            busy: open(&rel),
            file: rel,
        });
    }
    Ok(files)
}

/// A drive's numbers, for status.
#[derive(Debug, Default, PartialEq)]
pub struct Usage {
    /// Bytes downloaded, kept or not.
    pub downloaded: u64,
    /// Files and folders kept (each counts once, with what is below it).
    pub kept: usize,
    /// Files edited or made here, not uploaded yet.
    pub waiting: usize,
}

/// What a drive holds (Linux, Windows) on this computer.
#[cfg(any(target_os = "linux", windows))]
pub fn usage(state: &State, pair: &crate::state::Pair) -> state::Result<Usage> {
    let Some(backing) = root_of(state, pair) else {
        return Ok(Usage::default());
    };
    let (downloaded, waiting) = local_content(state, pair, &backing)?;
    Ok(Usage {
        downloaded,
        kept: state.kept(pair.id)?.len(),
        waiting: waiting.len(),
    })
}

/// A drive's downloaded bytes (Linux, Windows), and its files not uploaded yet (edited,
/// or made here), by path below `backing`.
#[cfg(any(target_os = "linux", windows))]
pub fn local_content(
    state: &State,
    pair: &crate::state::Pair,
    backing: &Path,
) -> state::Result<(u64, Vec<std::path::PathBuf>)> {
    let entries = state.entries(pair.id)?;
    let synced = paths(&entries, pair.root);
    let Ok(scan) = scan(backing, &Default::default()) else {
        return Ok((0, Vec::new()));
    };
    let mut downloaded = 0;
    let mut waiting = Vec::new();
    for item in scan.items.iter().filter(|i| i.is_file && i.downloaded) {
        downloaded += u64::try_from(item.size).unwrap_or_default();
        let rel: std::path::PathBuf = item.path.iter().collect();
        let as_synced = synced.get(&rel).is_some_and(|e| {
            e.size == item.size && e.mtime == item.mtime && e.file_id == item.file_id
        });
        if !as_synced {
            waiting.push(rel);
        }
    }
    Ok((downloaded, waiting))
}

/// What a drive's removal moves out (Linux, Windows), by path below
/// `backing`: all but what the server has as it is here (a placeholder, a
/// file downloaded and as synced). The whole folder is walked, not scanned:
/// ignored folders, the names the scan skips, links, and a folder that
/// can't be read (moved whole) are the user's, never deleted unseen.
#[cfg(any(target_os = "linux", windows))]
pub fn not_on_server(
    state: &State,
    pair: &crate::state::Pair,
    backing: &Path,
) -> state::Result<Vec<std::path::PathBuf>> {
    let entries = state.entries(pair.id)?;
    let synced = paths(&entries, pair.root);
    let mut out = Vec::new();
    let mut folders = vec![std::path::PathBuf::new()];
    while let Some(rel) = folders.pop() {
        let listed = std::fs::read_dir(backing.join(&rel)).and_then(|dir| {
            dir.map(|e| e.map(|e| e.file_name()))
                .collect::<io::Result<Vec<_>>>()
        });
        let Ok(names) = listed else {
            // The drive's folder itself unreadable: its removal fails there.
            if !rel.as_os_str().is_empty() {
                out.push(rel);
            }
            continue;
        };
        for name in names {
            let child = rel.join(name);
            let abs = backing.join(&child);
            match std::fs::symlink_metadata(&abs) {
                Ok(m) if m.is_dir() => folders.push(child),
                Ok(m) if m.is_file() => {
                    let on_server = is_placeholder(&abs)
                        || synced.get(&child).is_some_and(|e| {
                            e.is_file
                                && local::unchanged(&abs, e.size, e.mtime, e.file_id.as_deref())
                        });
                    if !on_server {
                        out.push(child);
                    }
                }
                _ => out.push(child),
            }
        }
    }
    Ok(out)
}

/// A drive's freed nodes (Linux, Windows) evicted (`theca free`): each downloaded file
/// at or below one, as synced and not kept, goes. `busy`: the mount's, when
/// mounted (an open file waits); none: not mounted, nothing can be open. A
/// node is cleared once nothing of it is left to evict (an edit made since
/// stays); the number still waiting on an open file is returned.
#[cfg(any(target_os = "linux", windows))]
pub fn free_now(
    state: &State,
    pair: &crate::state::Pair,
    busy: Option<&busy::Busy>,
) -> state::Result<usize> {
    let freed = state.freed(pair.id)?;
    if freed.is_empty() {
        return Ok(0);
    }
    let Some(backing) = root_of(state, pair) else {
        return Ok(0);
    };
    let entries = state.entries(pair.id)?;
    let kept = state.kept(pair.id)?;
    let by_path = paths(&entries, pair.root);
    let by_node: std::collections::HashMap<_, _> =
        by_path.iter().map(|(p, e)| (e.node, p.clone())).collect();
    let parents: std::collections::HashMap<_, _> =
        entries.iter().map(|e| (e.node, e.parent)).collect();
    // Kept, or below a folder kept, up to the freed node.
    let kept_below = |mut node, top| {
        for _ in 0..crate::plan::MAX_DEPTH {
            if kept.contains(&node) {
                return true;
            }
            if node == top {
                return false;
            }
            match parents.get(&node) {
                Some(p) => node = *p,
                None => return false,
            }
        }
        false
    };
    let mut waiting = 0;
    for node in freed {
        let mut open = false;
        if let Some(top) = by_node.get(&node) {
            for (rel, e) in by_path
                .iter()
                .filter(|(p, e)| e.is_file && p.starts_with(top))
            {
                let abs = backing.join(rel);
                if is_placeholder(&abs)
                    || kept_below(e.node, node)
                    || !local::unchanged(&abs, e.size, e.mtime, e.file_id.as_deref())
                {
                    continue;
                }
                let taken = match busy.map(|b| b.try_take(rel)) {
                    Some(None) => {
                        open = true;
                        continue;
                    }
                    taken => taken,
                };
                // Refused (Windows: open elsewhere): tried again.
                if let Err(err) = evict_synced(&abs, e) {
                    tracing::debug!(pair = pair.id, path = %rel.display(), "not freed yet: {err}");
                    open = true;
                }
                drop(taken);
            }
        }
        if open {
            waiting += 1;
        } else {
            state.clear_free(pair.id, node)?;
        }
    }
    Ok(waiting)
}

/// The entries by their path below the root (none for a node whose parents
/// aren't all known).
#[cfg(any(target_os = "linux", windows))]
fn paths(
    entries: &[crate::state::Entry],
    root: uuid::Uuid,
) -> std::collections::HashMap<std::path::PathBuf, &crate::state::Entry> {
    let by_node: std::collections::HashMap<_, _> = entries.iter().map(|e| (e.node, e)).collect();
    let path = |mut node| {
        let mut names = Vec::new();
        while node != root {
            let e: &crate::state::Entry = by_node.get(&node)?;
            names.push(e.name.as_str());
            node = e.parent;
            if names.len() > crate::plan::MAX_DEPTH {
                return None;
            }
        }
        names.reverse();
        Some(names.iter().collect::<std::path::PathBuf>())
    };
    entries
        .iter()
        .filter_map(|e| Some((path(e.node)?, e)))
        .collect()
}

/// Why this computer can't hold a drive, if it can't (the window greys its
/// Drive tick with it): Linux needs FUSE. Windows' NTFS and Linux's
/// extended attributes are checked when one is added.
pub fn unavailable() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        if !Path::new("/dev/fuse").exists() {
            return Some("this computer has no FUSE (/dev/fuse)".into());
        }
        if !fuse::available() {
            return Some("install fuse3 (fusermount3) for drives".into());
        }
    }
    // App Translocation: Gatekeeper runs a quarantined app moved by anything
    // but Finder from a temporary copy, and macOS disables its File Provider.
    #[cfg(target_os = "macos")]
    if std::env::current_exe().is_ok_and(|e| e.to_string_lossy().contains("/AppTranslocation/")) {
        return Some(
            "macOS runs Theca from a temporary copy: move Theca.app to Applications in Finder, then open it again"
                .into(),
        );
    }
    None
}

/// What a drive keeps on this computer, by path below its folder.
pub fn kept_paths(
    state: &State,
    pair: &crate::state::Pair,
) -> state::Result<Vec<std::path::PathBuf>> {
    let mut paths = Vec::new();
    for node in state.kept(pair.id)? {
        let mut names = Vec::new();
        let mut at = Some(node);
        while let Some(n) = at.filter(|n| *n != pair.root) {
            let Some(e) = state.entry(pair.id, n)? else {
                break;
            };
            names.push(e.name);
            at = Some(e.parent);
            if names.len() > crate::plan::MAX_DEPTH {
                break;
            }
        }
        names.reverse();
        paths.push(names.iter().collect());
    }
    paths.sort();
    Ok(paths)
}

/// A downloaded file the limit counts: not kept, uploaded.
#[derive(Debug, Clone, PartialEq)]
pub struct Downloaded<T> {
    pub file: T,
    pub size: u64,
    /// The last open seen, else the download (Unix seconds).
    pub used: i64,
    /// Open or busy: counted, never evicted this time.
    pub busy: bool,
}

/// What to evict to get under the limit: the oldest `used` first, busy files
/// skipped. Short of the limit still when only busy files are left.
pub fn evictions<T>(mut files: Vec<Downloaded<T>>, limit: u64) -> Vec<T> {
    let mut total: u64 = files.iter().map(|f| f.size).sum();
    files.sort_by_key(|f| f.used);
    let mut out = Vec::new();
    for f in files {
        if total <= limit {
            break;
        }
        if !f.busy {
            total -= f.size;
            out.push(f.file);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file edited after the caller saw it as synced isn't evicted: the
    /// edit stays; as synced, it is.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_edit_since_is_never_evicted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "synced").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let entry = state::Entry {
            node: uuid::Uuid::nil(),
            parent: uuid::Uuid::nil(),
            name: "a.txt".into(),
            is_file: true,
            version: None,
            size: meta.len() as i64,
            mtime: local::mtime(&meta),
            file_id: local::file_id(&path),
        };
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "edited").unwrap();
        assert!(!evict_synced(&path, &entry).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited");
        let meta = std::fs::metadata(&path).unwrap();
        let synced = state::Entry {
            mtime: local::mtime(&meta),
            ..entry
        };
        assert!(evict_synced(&path, &synced).unwrap());
        assert!(is_placeholder(&path));
    }

    fn file(file: &'static str, size: u64, used: i64) -> Downloaded<&'static str> {
        Downloaded {
            file,
            size,
            used,
            busy: false,
        }
    }

    /// Under the limit, or at it, nothing leaves; over it, the oldest used
    /// first, only as many as it takes; a busy file stays, the next one
    /// leaves in its place.
    #[test]
    fn the_least_recently_used_leave_first() {
        let files = vec![file("new", 4, 30), file("old", 4, 10), file("mid", 4, 20)];
        assert_eq!(evictions(files.clone(), 12), Vec::<&str>::new());
        assert_eq!(evictions(files.clone(), 8), ["old"]);
        assert_eq!(evictions(files.clone(), 7), ["old", "mid"]);
        assert_eq!(evictions(files.clone(), 0), ["old", "mid", "new"]);
        let mut busy = files.clone();
        busy[1].busy = true;
        assert_eq!(evictions(busy.clone(), 8), ["mid"]);
        // Only busy files left: short of the limit, this time.
        assert_eq!(evictions(busy, 0), ["mid", "new"]);
    }

    /// The default: a tenth of a small disk, 10 GiB on a big one, 10 GiB
    /// with no disk to measure.
    #[test]
    fn the_default_limit_is_a_tenth_of_the_disk_at_most_10_gib() {
        assert_eq!(default_limit(None), DEFAULT_CAP);
        let dir = std::env::temp_dir().join("state.sqlite");
        let total = fs4::total_space(std::env::temp_dir()).unwrap();
        assert_eq!(default_limit(Some(&dir)), (total / 10).min(DEFAULT_CAP));
    }
}
