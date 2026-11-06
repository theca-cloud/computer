//! The Theca sync client: local folders kept identical to libraries, both
//! ways (docs/decisions.md, Sync; ADR 0004).

pub mod engine;
pub mod exec;
#[cfg(feature = "gui")]
pub mod gui;
pub mod local;
pub mod ops;
pub mod plan;
pub mod state;

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
/// lock, for as long as they run (ADR 0004).
pub fn lock(state: &std::path::Path) -> std::io::Result<std::fs::File> {
    let lock = std::fs::File::create(state.with_extension("lock"))?;
    if lock.try_lock().is_err() {
        return Err(std::io::Error::other(
            "theca is already running on this state (the app, or `theca run`)",
        ));
    }
    Ok(lock)
}
