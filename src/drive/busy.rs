//! What a drive's files are going through, shared by its file system and its
//! rounds (by path in the backing folder): files open through the mount, and
//! files a fetch or a round is acting on. A round skips what is busy (the
//! next one does it); a fetch waits for a round's action to end. Never two
//! actors on one file: a fetch landing an old version over a round's newer
//! placeholder, or a round replacing a file open for writing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

#[derive(Debug, Default)]
struct Use {
    opens: usize,
    /// A fetch or a round's action has it.
    taken: bool,
}

#[derive(Clone, Default)]
pub struct Busy(Arc<(Mutex<HashMap<PathBuf, Use>>, Condvar)>);

/// A file taken, given back when dropped.
pub struct Taken {
    busy: Busy,
    path: PathBuf,
}

impl Busy {
    fn files(&self) -> MutexGuard<'_, HashMap<PathBuf, Use>> {
        // A panic while holding it left the map as it was: still usable.
        self.0.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// For a round: `path` taken, unless it or anything below it (a folder
    /// moved or deleted) is open or taken.
    pub fn try_take(&self, path: &Path) -> Option<Taken> {
        let mut files = self.files();
        if files.keys().any(|p| p.starts_with(path)) {
            return None;
        }
        files.insert(
            path.to_owned(),
            Use {
                opens: 0,
                taken: true,
            },
        );
        Some(Taken {
            busy: self.clone(),
            path: path.to_owned(),
        })
    }

    /// For a fetch: `path` taken once nothing has it, nor a folder above it
    /// (a round moving it).
    pub fn take(&self, path: &Path) -> Taken {
        let mut files = self.files();
        while files.iter().any(|(p, u)| u.taken && path.starts_with(p)) {
            files = self.0.1.wait(files).unwrap_or_else(|e| e.into_inner());
        }
        files.entry(path.to_owned()).or_default().taken = true;
        Taken {
            busy: self.clone(),
            path: path.to_owned(),
        }
    }

    /// A file opened through the mount (after its fetch, if it needed one).
    pub fn opened(&self, path: &Path) {
        self.files().entry(path.to_owned()).or_default().opens += 1;
    }

    /// A file closed through the mount.
    pub fn closed(&self, path: &Path) {
        let mut files = self.files();
        if let Some(u) = files.get_mut(path) {
            u.opens = u.opens.saturating_sub(1);
            if u.opens == 0 && !u.taken {
                files.remove(path);
            }
        }
    }

    /// A file renamed through the mount: its opens follow it, and what is
    /// below it, a folder's.
    pub fn moved(&self, from: &Path, to: &Path) {
        let mut files = self.files();
        let below: Vec<PathBuf> = files
            .keys()
            .filter(|p| p.starts_with(from))
            .cloned()
            .collect();
        for p in below {
            let rest = p.strip_prefix(from).expect("below it");
            let new = if rest.as_os_str().is_empty() {
                to.to_owned()
            } else {
                to.join(rest)
            };
            // A hold stays with the path it was taken for (its `Taken` gives
            // that one back); only opens follow.
            let u = files.get_mut(&p).expect("listed above");
            let opens = std::mem::take(&mut u.opens);
            if !u.taken {
                files.remove(&p);
            }
            if opens > 0 {
                files.entry(new).or_default().opens += opens;
            }
        }
    }

    /// Open through the mount: never evicted.
    pub fn is_open(&self, path: &Path) -> bool {
        self.files().get(path).is_some_and(|u| u.opens > 0)
    }
}

impl Drop for Taken {
    fn drop(&mut self) {
        let mut files = self.busy.files();
        if let Some(u) = files.get_mut(&self.path) {
            u.taken = false;
            if u.opens == 0 {
                files.remove(&self.path);
            }
        }
        self.busy.0.1.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// A round never takes a file open, taken, or below a folder it acts on;
    /// once given back, it can.
    #[test]
    fn a_round_skips_what_is_busy() {
        let busy = Busy::default();
        busy.opened(&p("d/a.txt"));
        assert!(busy.try_take(&p("d/a.txt")).is_none());
        assert!(busy.try_take(&p("d")).is_none(), "a folder holding it");
        assert!(busy.try_take(&p("d/b.txt")).is_some(), "given back on drop");
        let taken = busy.try_take(&p("d/b.txt")).unwrap();
        assert!(busy.try_take(&p("d/b.txt")).is_none());
        drop(taken);
        busy.closed(&p("d/a.txt"));
        assert!(busy.try_take(&p("d")).is_some());
        assert!(!busy.is_open(&p("d/a.txt")));
    }

    /// A fetch waits for a round's action on the file, then has it.
    #[test]
    fn a_fetch_waits_for_a_round() {
        let busy = Busy::default();
        let round = busy.try_take(&p("a.txt")).unwrap();
        let fetching = {
            let busy = busy.clone();
            std::thread::spawn(move || {
                let _fetch = busy.take(&p("a.txt"));
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!fetching.is_finished());
        drop(round);
        fetching.join().unwrap();
        // Below a folder a round has, too.
        let round = busy.try_take(&p("d")).unwrap();
        let fetching = {
            let busy = busy.clone();
            std::thread::spawn(move || {
                let _fetch = busy.take(&p("d/a.txt"));
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!fetching.is_finished());
        drop(round);
        fetching.join().unwrap();
        assert!(busy.try_take(&p("a.txt")).is_some());
    }

    /// A file renamed while held: given back, its new path is free (a hold
    /// carried along was never given back, and every later delete waited).
    #[test]
    fn a_hold_stays_with_its_path() {
        let busy = Busy::default();
        let taken = busy.try_take(&p("a.txt")).unwrap();
        busy.moved(&p("a.txt"), &p("b.txt"));
        assert!(busy.try_take(&p("b.txt")).is_some());
        drop(taken);
        assert!(busy.try_take(&p("a.txt")).is_some());
        busy.take(&p("b.txt"));
    }

    /// Opens follow a rename through the mount, a folder's too.
    #[test]
    fn opens_follow_a_rename() {
        let busy = Busy::default();
        busy.opened(&p("d/a.txt"));
        busy.moved(&p("d"), &p("e"));
        assert!(busy.is_open(&p("e/a.txt")));
        assert!(!busy.is_open(&p("d/a.txt")));
        busy.moved(&p("e/a.txt"), &p("b.txt"));
        assert!(busy.is_open(&p("b.txt")));
    }
}
