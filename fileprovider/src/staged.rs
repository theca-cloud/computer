//! The staging folder's leftovers: a local change's content is copied there
//! for the app (`upload-…`) and removed once answered, but the system may
//! kill the extension in between, and nothing else would ever remove the
//! copy.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Older than any upload still running.
pub const STALE: Duration = Duration::from_secs(24 * 3600);

/// The copies in `folder` last written more than `stale` ago, removed.
pub fn sweep(folder: &Path, stale: Duration, now: SystemTime) {
    let Ok(files) = fs::read_dir(folder) else {
        return;
    };
    for file in files.flatten() {
        let old = file
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|at| now.duration_since(at).is_ok_and(|age| age > stale));
        if old && file.file_name().to_string_lossy().starts_with("upload-") {
            let _ = fs::remove_file(file.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only an old copy goes: not a recent one (its upload may run), not a
    /// fetch's file.
    #[test]
    fn only_old_copies_are_swept() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["upload-1", "fetched-1"] {
            fs::write(dir.path().join(name), "x").unwrap();
        }
        let now = SystemTime::now();
        sweep(dir.path(), STALE, now);
        assert!(dir.path().join("upload-1").exists(), "recent");
        sweep(dir.path(), STALE, now + STALE + Duration::from_secs(60));
        assert!(!dir.path().join("upload-1").exists(), "old");
        assert!(dir.path().join("fetched-1").exists(), "not a copy");
    }
}
