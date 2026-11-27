//! A Mac drive's folders read in before anyone opens them (theca/computer#124).
//!
//! macOS reads a drive's folder into its own index the first time it is
//! listed (a job for the folder, one per child, a wait to flush): about a
//! quarter of a second on a Mac, of which the app's answer is a few
//! milliseconds. So the app lists, in the background, the folders below the
//! one just listed (three levels, folders only, never a file's content): the
//! next clicks find them read in. The planning is here, portable; the reading
//! (`host::list_dir`) is macOS's.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use uuid::Uuid;

use super::lock;
use crate::state::{Result, State};

/// How many levels of folders below the one listed are read ahead.
pub const DEPTH: usize = 3;
/// How many folders one burst reads at most (a big library doesn't keep the
/// system busy).
pub const CAP: usize = 300;
/// The system asks the extension, and so the app, about each folder the app
/// reads (a `List`, a moment after): those asks start nothing, or one click
/// would walk the whole library.
const OURS_FOR: Duration = Duration::from_secs(3);

/// The folders below `from` (a node of the pair's), breadth first, down to
/// `depth` levels, `cap` at most: each with its names from `from`.
pub fn folders_below(
    state: &State,
    pair: i64,
    from: Uuid,
    depth: usize,
    cap: usize,
) -> Result<Vec<(Uuid, Vec<String>)>> {
    let mut found: Vec<(Uuid, Vec<String>)> = Vec::new();
    let mut level: Vec<(Uuid, Vec<String>)> = vec![(from, Vec::new())];
    for _ in 0..depth {
        let mut next = Vec::new();
        for (parent, names) in &level {
            let mut children = state.children(pair, *parent)?;
            children.retain(|e| !e.is_file);
            // The same order whatever the database's.
            children.sort_by(|a, b| a.name.cmp(&b.name));
            for child in children {
                if found.len() + next.len() >= cap {
                    found.extend(next);
                    return Ok(found);
                }
                let mut path = names.clone();
                path.push(child.name);
                next.push((child.node, path));
            }
        }
        found.extend(next.iter().cloned());
        level = next;
        if level.is_empty() {
            break;
        }
    }
    Ok(found)
}

/// The names from the library's root (`root`) down to `node`, in order: where
/// a folder is below the drive's folder. Empty for the root; none for a node
/// that isn't below it.
pub fn names_to(state: &State, pair: i64, root: Uuid, node: Uuid) -> Result<Option<Vec<String>>> {
    let mut names = Vec::new();
    let mut at = node;
    // A tree is as deep as its names allow: bounded all the same.
    for _ in 0..4096 {
        if at == root {
            names.reverse();
            return Ok(Some(names));
        }
        let Some(entry) = state.entry(pair, at)? else {
            return Ok(None);
        };
        names.push(entry.name);
        at = entry.parent;
    }
    Ok(None)
}

/// A name that stays where it is joined: not `..` (or `.`), no separator
/// (`PathBuf::join` takes an absolute one for the whole path).
fn plain(name: &str) -> bool {
    !matches!(name, "" | "." | "..") && !name.contains(['/', '\\', '\0'])
}

/// The folders to read ahead for the listing of `from` (a node of the pair's):
/// each by its container identifier and its path in the drive's folder
/// (`pair.path`), `from`'s own path first (the names `folders_below` gives are
/// relative to it). None when `from` isn't below the pair's root.
pub fn paths_below(
    state: &State,
    pair: &crate::state::Pair,
    uuid: Uuid,
    from: Uuid,
) -> Result<Option<Vec<(String, PathBuf)>>> {
    let Some(names) = names_to(state, pair.id, pair.root, from)? else {
        return Ok(None);
    };
    // A name the server gave is never a way out of the drive's folder.
    if !names.iter().all(|n| plain(n)) {
        return Ok(None);
    }
    let base = names.iter().fold(pair.path.clone(), |p, n| p.join(n));
    Ok(Some(
        folders_below(state, pair.id, from, DEPTH, CAP)?
            .into_iter()
            .filter(|(_, below)| below.iter().all(|n| plain(n)))
            .map(|(node, below)| {
                (
                    crate::drive::provider::item_id(uuid, Some(node)),
                    below.iter().fold(base.clone(), |p, n| p.join(n)),
                )
            })
            .collect(),
    ))
}

/// Folders to read, in order: each by its container identifier and its path,
/// for the listing of `root`.
#[derive(Debug, PartialEq)]
pub struct Job {
    pub root: String,
    pub folders: Vec<(String, PathBuf)>,
}

impl Seen {
    /// What the app finished reading long enough ago is the system's again.
    fn expire(&mut self) {
        let now = Instant::now();
        self.ours
            .retain(|_, at| at.is_none_or(|at| now.duration_since(at) < OURS_FOR));
    }
}

#[derive(Default)]
struct Seen {
    /// Folders the app is reading, or finished reading a moment ago: their
    /// asks are the system's answer to us.
    ours: HashMap<String, Option<Instant>>,
    /// Folders read, this run: not read again.
    done: HashSet<String>,
    /// Folders a read-ahead started from (listed by the system), this run.
    started: HashSet<String>,
}

/// What was read ahead, and what the app is reading now: one per process.
#[derive(Default)]
pub struct PreRead {
    seen: Mutex<Seen>,
    /// The next burst, the newest asked while one runs (the older are stale:
    /// the user went elsewhere).
    pending: Mutex<Option<Job>>,
    working: std::sync::atomic::AtomicBool,
    /// Drives removed: their bursts end at the next folder.
    forgotten: Mutex<HashSet<Uuid>>,
}

impl PreRead {
    /// Whether the listing of `root` (its container identifier) may start a
    /// read-ahead: not the system's answer to a read of ours, nor one already
    /// started from. Cheap: asked before the folders below are looked for.
    pub fn wanted(&self, root: &str) -> bool {
        let mut seen = lock(&self.seen);
        seen.expire();
        !seen.ours.contains_key(root) && !seen.started.contains(root)
    }

    /// The folders to read for the listing of `root`, given the folders below
    /// it as `folders_below` found them (identifier and path): none when
    /// `root` isn't `wanted`; the ones read already left out.
    pub fn plan(&self, root: &str, below: Vec<(String, PathBuf)>) -> Option<Job> {
        let mut seen = lock(&self.seen);
        seen.expire();
        // Nothing below yet (the drive's entries not loaded, or no folder):
        // nothing is started, so that a later listing tries again.
        if below.is_empty() || seen.ours.contains_key(root) || seen.started.contains(root) {
            return None;
        }
        seen.started.insert(root.to_owned());
        let folders: Vec<_> = below
            .into_iter()
            .filter(|(id, _)| !seen.done.contains(id))
            .collect();
        // Said before they are read: their asks arrive while they are.
        for (id, _) in &folders {
            seen.ours.insert(id.clone(), None);
        }
        (!folders.is_empty()).then(|| Job {
            root: root.to_owned(),
            folders,
        })
    }

    /// A folder of the burst read: left alone for a moment (the system's asks
    /// about it), and not read again. Failed (not there yet): free again.
    pub fn read(&self, id: &str, ok: bool) {
        let mut seen = lock(&self.seen);
        if ok {
            seen.done.insert(id.to_owned());
            seen.ours.insert(id.to_owned(), Some(Instant::now()));
        } else {
            seen.ours.remove(id);
        }
    }

    /// The burst to run, if none runs: false when one does, which takes this
    /// one (replacing an older, not yet started) when it ends.
    pub fn queue(&self, job: Job) -> bool {
        // The one it replaces never runs: its folders are free again, and its
        // root may start another.
        if let Some(old) = lock(&self.pending).replace(job) {
            let mut seen = lock(&self.seen);
            seen.started.remove(&old.root);
            for (id, _) in &old.folders {
                if seen.ours.get(id).is_some_and(Option::is_none) {
                    seen.ours.remove(id);
                }
            }
        }
        !self.working.swap(true, std::sync::atomic::Ordering::SeqCst)
    }

    /// The next burst, or none: the worker ends then.
    pub fn next(&self) -> Option<Job> {
        // `working` ends under the lock that found nothing pending: a burst
        // queued between the two would see a worker still there, and never
        // run.
        let mut pending = lock(&self.pending);
        let job = pending.take();
        if job.is_none() {
            self.working
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        job
    }

    /// A drive no longer served: what is queued or being read for it stops.
    pub fn forget(&self, drive: Uuid) {
        lock(&self.forgotten).insert(drive);
    }

    /// A drive served (again): its reads may run.
    pub fn serve(&self, drive: Uuid) {
        lock(&self.forgotten).remove(&drive);
    }

    /// Whether the drive was removed since its burst was planned.
    pub fn is_forgotten(&self, drive: Uuid) -> bool {
        lock(&self.forgotten).contains(&drive)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folders(names: &[&str]) -> Vec<(String, PathBuf)> {
        names
            .iter()
            .map(|n| (format!("id-{n}"), PathBuf::from(n)))
            .collect()
    }

    /// A listing starts a read-ahead once; asking again for the same folder
    /// starts nothing, and what was read isn't read again.
    #[test]
    fn a_folder_is_read_ahead_once_and_what_is_read_is_left() {
        let ahead = PreRead::default();
        let job = ahead.plan("root", folders(&["a", "b"])).unwrap();
        assert_eq!(job.folders.len(), 2);
        assert_eq!(ahead.plan("root", folders(&["a", "b"])), None);
        ahead.read("id-a", true);
        // Another folder listed, whose burst holds one already read.
        let again = ahead.plan("other", folders(&["a", "c"])).unwrap();
        assert_eq!(again.folders, folders(&["c"]));
    }

    /// A listing with no folder below it (the drive's entries not loaded yet)
    /// starts nothing and leaves the folder free to start one later.
    #[test]
    fn nothing_below_starts_nothing_and_keeps_the_folder_free() {
        let ahead = PreRead::default();
        assert_eq!(ahead.plan("root", Vec::new()), None);
        assert!(ahead.wanted("root"));
        assert!(ahead.plan("root", folders(&["a"])).is_some());
        assert!(!ahead.wanted("root"));
    }

    /// The asks the system makes about what the app reads start nothing,
    /// whether the read is under way or just ended; a read that failed is
    /// free again.
    #[test]
    fn the_asks_about_our_reads_start_nothing() {
        let ahead = PreRead::default();
        ahead.plan("root", folders(&["a", "b"])).unwrap();
        // Being read.
        assert_eq!(ahead.plan("id-a", folders(&["x"])), None);
        // Just read.
        ahead.read("id-a", true);
        assert_eq!(ahead.plan("id-a", folders(&["x"])), None);
        // Failed: listed by the system later, it starts one.
        ahead.read("id-b", false);
        assert!(ahead.plan("id-b", folders(&["y"])).is_some());
    }

    /// One burst at a time; the newest asked meanwhile replaces the older,
    /// whose folders are free and whose root may start another.
    #[test]
    fn one_burst_runs_at_a_time_and_the_newest_waits() {
        let ahead = PreRead::default();
        let job = |ahead: &PreRead, n: &str| ahead.plan(n, folders(&[n])).unwrap();
        let (one, two, three) = (job(&ahead, "one"), job(&ahead, "two"), job(&ahead, "three"));
        assert!(ahead.queue(one));
        assert!(!ahead.queue(two));
        assert!(!ahead.queue(three));
        // `two` never ran: its root asks again, its folder is not ours.
        assert!(ahead.wanted("two"));
        assert!(ahead.wanted("id-two"));
        assert!(!ahead.wanted("three"));
        assert_eq!(ahead.next().map(|j| j.root), Some("three".to_owned()));
        assert_eq!(ahead.next(), None);
        // The worker ended: the next starts one.
        let four = job(&ahead, "four");
        assert!(ahead.queue(four));
    }

    /// A burst queued while the worker finds nothing left is run by that
    /// worker or by a new one, never by none (a lost burst left its folders
    /// "ours" for good).
    #[test]
    fn a_burst_queued_as_the_worker_ends_is_not_lost() {
        use std::sync::{Arc, Barrier};
        // The worker's end and a queue start together, the queue's delay
        // sweeping the few hundred nanoseconds the worker takes to end.
        for round in 0..60_000 {
            let ahead = Arc::new(PreRead::default());
            let first = ahead.plan("one", folders(&["one"])).unwrap();
            let second = ahead.plan("two", folders(&["two"])).unwrap();
            assert!(ahead.queue(first));
            let start = Arc::new(Barrier::new(2));
            let worker = {
                let (ahead, start) = (ahead.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    while ahead.next().is_some() {}
                })
            };
            start.wait();
            for _ in 0..(round % 64) {
                std::hint::spin_loop();
            }
            // A new worker when none is left, as the provider spawns one.
            let again = ahead.queue(second).then(|| {
                let ahead = ahead.clone();
                std::thread::spawn(move || while ahead.next().is_some() {})
            });
            worker.join().unwrap();
            if let Some(again) = again {
                again.join().unwrap();
            }
            assert!(
                ahead.pending.lock().unwrap().is_none(),
                "round {round}: a burst was left with no worker"
            );
        }
    }

    #[test]
    fn a_name_that_leaves_the_folder_is_not_plain() {
        for bad in ["", ".", "..", "/etc", "a/b", "a\\b", "a\0b"] {
            assert!(!plain(bad), "{bad:?}");
        }
        for good in ["a", "my file.txt", "...", ".hidden", "Photos.app"] {
            assert!(plain(good), "{good:?}");
        }
    }
}
