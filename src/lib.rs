//! The Theca sync client: local folders kept identical to libraries, both
//! ways (wiki: Sync client; ADR 0004).

pub mod drive;
pub mod engine;
pub mod exec;
#[cfg(feature = "gui")]
pub mod gui;
pub mod keychain;
pub mod local;
pub mod logs;
pub mod ops;
pub mod plan;
pub mod state;

/// The CLI's exit codes besides 0, success. Indications only: what 1.x
/// promises is 0 or not (`USING.md` lists them).
pub mod exit {
    pub const ERROR: u8 = 1;
    /// Invalid arguments: clap's own.
    pub const USAGE: u8 = 2;
    /// `status`: a pair isn't syncing (stopped, or its account's engine on an
    /// error).
    pub const NOT_SYNCING: u8 = 3;
    /// An account is signed out, or its app password is gone from the
    /// keychain: log in again.
    pub const SIGNED_OUT: u8 = 4;
    pub const ALL: [u8; 4] = [ERROR, USAGE, NOT_SYNCING, SIGNED_OUT];
}

/// Aborts the process at a named point when `THECA_CRASH_AT` names it
/// (`downloaded`, or `downloaded#2` for its second time): the crash tests'
/// `kill -9` (tests/crash.rs). Nothing without the `crash-points` feature.
#[inline]
pub fn crash_point(name: &str) {
    #[cfg(feature = "crash-points")]
    if let Ok(at) = std::env::var("THECA_CRASH_AT") {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static HITS: AtomicUsize = AtomicUsize::new(0);
        let (point, nth) = at
            .split_once('#')
            .map_or((at.as_str(), 1), |(p, n)| (p, n.parse().unwrap_or(1)));
        if point == name && HITS.fetch_add(1, Ordering::SeqCst) + 1 == nth {
            std::process::abort();
        }
    }
    let _ = name;
}

/// The engine runs alone on a state: the app and `theca run` both take this
/// lock, for as long as they run (ADR 0004). Named after the state's whole
/// file name (`state.sqlite.lock`): two states never share one.
pub fn lock(state: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut name = state.file_name().unwrap_or_default().to_owned();
    name.push(".lock");
    let lock = std::fs::File::create(state.with_file_name(name))?;
    if lock.try_lock().is_err() {
        return Err(std::io::Error::other(
            "theca is already running on this state (the app, or `theca run`)",
        ));
    }
    Ok(lock)
}

#[cfg(test)]
mod tests {
    /// Each state has its own lock (`a.db` and `a.sqlite` shared `a.lock`),
    /// and a state named like a lock isn't truncated by taking it.
    #[test]
    fn a_lock_per_state() {
        let dir = tempfile::tempdir().unwrap();
        let _a = super::lock(&dir.path().join("a.db")).unwrap();
        assert!(super::lock(&dir.path().join("a.sqlite")).is_ok());
        let state = dir.path().join("x.lock");
        std::fs::write(&state, b"state").unwrap();
        let _x = super::lock(&state).unwrap();
        assert_eq!(std::fs::read(&state).unwrap(), b"state");
    }
}
