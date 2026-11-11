//! The extension's own log file, bounded: past `MAX` bytes it becomes
//! `<name>.1` (the one before dropped), and a new one starts. Sandboxed, the
//! extension can't write into the app's log folder (`theca logs` names this
//! one).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const MAX: u64 = 1_000_000;

/// `line` appended to `path`, rotated first when over `max`.
pub fn append(path: &Path, line: &str, max: u64) -> io::Result<()> {
    if fs::metadata(path).is_ok_and(|m| m.len() > max) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        fs::rename(path, PathBuf::from(old))?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Past its size it starts over, the old one kept once; under it, lines
    /// are appended.
    #[test]
    fn the_log_rotates_once_past_its_size() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("drive.log");
        let old = dir.path().join("drive.log.1");
        append(&log, "one", 6).unwrap();
        append(&log, "two", 6).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "one\ntwo\n");
        append(&log, "three", 6).unwrap();
        assert_eq!(fs::read_to_string(&old).unwrap(), "one\ntwo\n");
        assert_eq!(fs::read_to_string(&log).unwrap(), "three\n");
        append(&log, &"x".repeat(20), 6).unwrap();
        append(&log, "five", 6).unwrap();
        assert_eq!(
            fs::read_to_string(&old).unwrap(),
            format!("three\n{}\n", "x".repeat(20))
        );
        assert_eq!(fs::read_to_string(&log).unwrap(), "five\n");
    }
}
