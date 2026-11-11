//! The client's log files (sprint 17): every process on a state appends to
//! the day's file in its log folder (`theca.<YYYY-MM-DD>.log`, UTC), each
//! line with its spans (account, pair, round, fetch, request). Finished days
//! are gzipped, and the folder kept within its days and its size, by the app
//! and `theca run` (`keep`). Four settings in the state, read at start.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{NaiveDate, Utc};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::state::{self, State};

pub const LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];
pub const DEFAULT_LEVEL: &str = "info";
pub const DEFAULT_DAYS: u32 = 14;
/// Ten years: more is read as this (a date that far back still exists).
pub const MAX_DAYS: u32 = 3650;
pub const DEFAULT_MAX_SIZE: u64 = 100_000_000;

/// A command's outcome: in the file, never on stderr (the command prints it).
pub const EXIT: &str = "theca::exit";

const PREFIX: &str = "theca";
const SUFFIX: &str = "log";
/// A finished day's file still written this recently may have a writer that
/// hasn't rolled over yet: compressed later.
const SETTLED: Duration = Duration::from_secs(600);
/// A compression this old didn't finish (its process gone): done again.
const STALE: Duration = Duration::from_secs(3600);

/// The logs' settings, as the state has them (defaults where unset or
/// invalid).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Settings {
    pub level: String,
    pub days: u32,
    pub max_size: u64,
    /// The folder; none for a state without a file (the tests' in-memory
    /// ones).
    pub folder: Option<PathBuf>,
    /// The folder is the default one (`<data dir>/logs`).
    pub default_folder: bool,
}

/// The settings, an invalid one read as its default (and said).
pub fn settings(state: &State) -> state::Result<Settings> {
    let stored = |key: &str| state.setting(key);
    let read = [
        stored("log_level")?,
        stored("log_days")?,
        stored("log_max_size")?,
        stored("log_path")?,
    ];
    Ok(parse(read, state.path()))
}

/// The settings of the state at `state_path`, read before it is opened (its
/// migration, if any, is logged then); none stored: the defaults.
pub fn settings_at(state_path: &Path) -> Settings {
    let stored = State::stored_settings(state_path);
    let read =
        ["log_level", "log_days", "log_max_size", "log_path"].map(|k| stored.get(k).cloned());
    parse(read, Some(state_path))
}

fn parse(
    [level, days, max_size, path]: [Option<String>; 4],
    state_path: Option<&Path>,
) -> Settings {
    let level = level
        .filter(|l| {
            let valid = LEVELS.contains(&l.as_str());
            if !valid {
                tracing::warn!(value = l, "log_level isn't a level: the default");
            }
            valid
        })
        .unwrap_or_else(|| DEFAULT_LEVEL.to_owned());
    let days = days
        .and_then(|d| d.parse::<u32>().ok().filter(|d| *d >= 1))
        .map_or(DEFAULT_DAYS, |d| d.min(MAX_DAYS));
    let max_size = max_size
        .and_then(|s| s.parse().ok().filter(|s| *s >= 1))
        .unwrap_or(DEFAULT_MAX_SIZE);
    let set = path.map(PathBuf::from);
    let default_folder = set.is_none();
    let folder = set.or_else(|| Some(state_path?.parent()?.join("logs")));
    Settings {
        level,
        days,
        max_size,
        folder,
        default_folder,
    }
}

/// A level as given (`error` to `trace`), or why not.
pub fn level(value: &str) -> Result<String, String> {
    LEVELS
        .contains(&value)
        .then(|| value.to_owned())
        .ok_or_else(|| format!("{value:?} isn't a level: {}", LEVELS.join(", ")))
}

/// One setting changed; `None`: back to its default.
pub enum Change {
    Level(Option<String>),
    Days(Option<u32>),
    MaxSize(Option<u64>),
    Folder(Option<PathBuf>),
}

pub fn set(state: &State, change: Change) -> state::Result<()> {
    let (key, value) = match change {
        Change::Level(v) => ("log_level", v),
        Change::Days(v) => ("log_days", v.map(|d| d.clamp(1, MAX_DAYS).to_string())),
        Change::MaxSize(v) => ("log_max_size", v.map(|s| s.max(1).to_string())),
        Change::Folder(v) => ("log_path", v.map(|p| p.to_string_lossy().into_owned())),
    };
    match value {
        Some(v) => state.set_setting(key, &v),
        None => state.clear_setting(key),
    }
}

/// Logging until dropped (the file's writer flushes then).
pub struct Logging {
    _file: Option<tracing_appender::non_blocking::WorkerGuard>,
}

/// Starts logging: to stderr, and to the day's file in the settings' folder
/// (none: stderr only). `RUST_LOG`, set, wins over the level.
pub fn init(settings: Option<&Settings>) -> Logging {
    let level = settings.map_or(DEFAULT_LEVEL, |s| s.level.as_str());
    let filter = || {
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(format!("theca={level},theca_fileprovider={level}")))
    };
    let mut problem = None;
    let (file, guard) = match settings.and_then(|s| s.folder.as_deref()) {
        Some(folder) => match appender(folder) {
            Ok(appender) => {
                let (writer, guard) = tracing_appender::non_blocking(appender);
                let layer = tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(writer)
                    .with_filter(filter());
                (Some(layer), Some(guard))
            }
            Err(e) => {
                problem = Some(format!("no log file in {}: {e}", folder.display()));
                (None, None)
            }
        },
        None => (None, None),
    };
    let quiet = format!("{EXIT}=off").parse().expect("a directive");
    let stderr = tracing_subscriber::fmt::layer()
        .with_writer(io::stderr)
        .with_filter(filter().add_directive(quiet));
    let _ = tracing_subscriber::registry()
        .with(file)
        .with(stderr)
        .try_init();
    if let Some(problem) = problem {
        tracing::warn!("{problem}");
    }
    Logging { _file: guard }
}

fn appender(folder: &Path) -> io::Result<tracing_appender::rolling::RollingFileAppender> {
    // Made here: theirs alone to read (the logs name the user's files). One
    // that exists is left as its owner set it.
    if !folder.is_dir() {
        fs::create_dir_all(folder)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(folder, fs::Permissions::from_mode(0o700))?;
        }
    }
    tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(PREFIX)
        .filename_suffix(SUFFIX)
        .build(folder)
        .map_err(io::Error::other)
}

/// The folder kept within the settings now and every hour after (on a
/// thread of its own), for as long as the process runs: the app and
/// `theca run`.
pub fn keep(settings: &Settings) {
    let Some(folder) = settings.folder.clone() else {
        return;
    };
    let (days, max_size) = (settings.days, settings.max_size);
    std::thread::spawn(move || {
        loop {
            if let Err(e) = prune(&folder, days, max_size, Utc::now().date_naive()) {
                tracing::warn!(folder = %folder.display(), "logs not pruned: {e}");
            }
            std::thread::sleep(Duration::from_secs(3600));
        }
    });
}

/// A log file of the folder: its day, and whether it is gzipped.
struct Day {
    date: NaiveDate,
    path: PathBuf,
    gzipped: bool,
    size: u64,
}

fn day_of(name: &str) -> Option<(NaiveDate, bool)> {
    let rest = name.strip_prefix(PREFIX)?.strip_prefix('.')?;
    let (date, gzipped) = match rest.strip_suffix(".log.gz") {
        Some(date) => (date, true),
        None => (rest.strip_suffix(".log")?, false),
    };
    Some((NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?, gzipped))
}

/// Finished days gzipped (never today's), then the days older than `days`
/// deleted, then the oldest while the folder is over `max_size` (never
/// today's). Safe to run in two processes at once: a compression is written
/// to a `.part` only one can create.
pub fn prune(folder: &Path, days: u32, max_size: u64, today: NaiveDate) -> io::Result<()> {
    let now = SystemTime::now();
    let age = |path: &Path| {
        fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .unwrap_or_default()
    };
    let names = || -> io::Result<Vec<(PathBuf, String)>> {
        fs::read_dir(folder)?
            .map(|e| {
                let path = e?.path();
                let name = path.file_name().and_then(|n| n.to_str());
                let name = name.unwrap_or_default().to_owned();
                Ok((path, name))
            })
            .collect()
    };
    // Compressions whose process is gone, first: their day is done again
    // below, whatever order the folder lists them in.
    for (path, name) in names()? {
        if name.ends_with(".gz.part") && age(&path) > STALE {
            let _ = fs::remove_file(&path);
        }
    }
    for (path, name) in names()? {
        let Some((date, false)) = day_of(&name) else {
            continue;
        };
        if date >= today || age(&path) < SETTLED {
            continue;
        }
        // One that can't be (another's file, a full disk): said, the rest
        // goes on.
        if let Err(e) = compress(&path) {
            tracing::warn!(file = %path.display(), "log not compressed: {e}");
        }
    }
    let mut files: Vec<Day> = fs::read_dir(folder)?
        .filter_map(|e| {
            let path = e.ok()?.path();
            let (date, gzipped) = day_of(path.file_name()?.to_str()?)?;
            let size = fs::metadata(&path).ok()?.len();
            Some(Day {
                date,
                path,
                gzipped,
                size,
            })
        })
        .collect();
    files.sort_by_key(|f| (f.date, f.gzipped));
    let back = chrono::Days::new(u64::from(days.clamp(1, MAX_DAYS)) - 1);
    let oldest_kept = today.checked_sub_days(back).unwrap_or(NaiveDate::MIN);
    let mut total: u64 = files.iter().map(|f| f.size).sum();
    for f in &files {
        if f.date >= today {
            continue;
        }
        if f.date < oldest_kept || total > max_size {
            match fs::remove_file(&f.path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            total = total.saturating_sub(f.size);
        }
    }
    Ok(())
}

/// A finished day's file gzipped beside it, then removed; skipped when
/// another process is at it.
fn compress(plain: &Path) -> io::Result<()> {
    let mut gz = plain.as_os_str().to_owned();
    gz.push(".gz");
    let gz = PathBuf::from(gz);
    if gz.exists() {
        // Compressed before a crash, not removed yet.
        return remove_if_there(plain);
    }
    let mut part = gz.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let out = match File::create_new(&part) {
        Ok(out) => out,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e),
    };
    let written = (|| {
        let mut encoder = flate2::write::GzEncoder::new(out, flate2::Compression::default());
        io::copy(&mut File::open(plain)?, &mut encoder)?;
        encoder.finish()?.sync_all()?;
        fs::rename(&part, &gz)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&part);
        return Err(e);
    }
    remove_if_there(plain)
}

fn remove_if_there(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    /// A day's file, written `minutes` ago.
    fn day(folder: &Path, name: &str, content: &str, minutes: u64) -> PathBuf {
        let path = folder.join(name);
        fs::write(&path, content).unwrap();
        let file = File::options().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(minutes * 60))
            .unwrap();
        path
    }

    fn names(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A finished day gzipped, readable back; today's and one still written
    /// a moment ago left plain; other files untouched.
    #[test]
    fn finished_days_are_gzipped() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path();
        day(folder, "theca.2026-11-09.log", "monday\n", 60 * 30);
        day(folder, "theca.2026-11-10.log", "late writer\n", 1);
        day(folder, "theca.2026-11-11.log", "today\n", 60);
        day(folder, "notes.txt", "mine", 60 * 30);
        prune(folder, 14, DEFAULT_MAX_SIZE, date("2026-11-11")).unwrap();
        assert_eq!(
            names(folder),
            [
                "notes.txt",
                "theca.2026-11-09.log.gz",
                "theca.2026-11-10.log",
                "theca.2026-11-11.log"
            ]
        );
        let mut read = String::new();
        flate2::read::GzDecoder::new(File::open(folder.join("theca.2026-11-09.log.gz")).unwrap())
            .read_to_string(&mut read)
            .unwrap();
        assert_eq!(read, "monday\n");
    }

    /// Older than the days kept: deleted. Over the size: the oldest first,
    /// today's never, even alone over it.
    #[test]
    fn the_folder_keeps_its_days_and_its_size() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path();
        for d in ["2026-10-28", "2026-10-29", "2026-11-10"] {
            day(folder, &format!("theca.{d}.log.gz"), &"x".repeat(40), 0);
        }
        day(folder, "theca.2026-11-11.log", &"y".repeat(100), 0);
        // 14 days to 2026-11-11: from 2026-10-29 on.
        prune(folder, 14, DEFAULT_MAX_SIZE, date("2026-11-11")).unwrap();
        assert_eq!(
            names(folder),
            [
                "theca.2026-10-29.log.gz",
                "theca.2026-11-10.log.gz",
                "theca.2026-11-11.log"
            ]
        );
        // 180 bytes over 150: the oldest goes, the next fits.
        prune(folder, 14, 150, date("2026-11-11")).unwrap();
        assert_eq!(
            names(folder),
            ["theca.2026-11-10.log.gz", "theca.2026-11-11.log"]
        );
        prune(folder, 14, 10, date("2026-11-11")).unwrap();
        assert_eq!(names(folder), ["theca.2026-11-11.log"]);
        // Any number of days: no date out of range.
        prune(folder, u32::MAX, DEFAULT_MAX_SIZE, date("2026-11-11")).unwrap();
    }

    /// Two prunings at once lose nothing: the second finds the compression
    /// begun (its `.part`) and leaves it; one a crash left is done again
    /// once stale.
    #[test]
    fn a_compression_begun_elsewhere_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path();
        day(folder, "theca.2026-11-09.log", "monday\n", 60 * 30);
        let part = day(folder, "theca.2026-11-09.log.gz.part", "", 1);
        prune(folder, 14, DEFAULT_MAX_SIZE, date("2026-11-11")).unwrap();
        assert!(folder.join("theca.2026-11-09.log").exists(), "left");
        let file = File::options().write(true).open(&part).unwrap();
        file.set_modified(SystemTime::now() - 2 * STALE).unwrap();
        prune(folder, 14, DEFAULT_MAX_SIZE, date("2026-11-11")).unwrap();
        assert_eq!(names(folder), ["theca.2026-11-09.log.gz"]);
    }

    /// Several processes pruning at the same moment (the app and `theca
    /// run` starting together): every finished day ends gzipped once, whole.
    #[test]
    fn prunings_at_once_lose_no_line() {
        for _ in 0..20 {
            let dir = tempfile::tempdir().unwrap();
            let folder = dir.path();
            let lines = "a line of the day\n".repeat(20_000);
            for d in 1..=6 {
                day(folder, &format!("theca.2026-11-0{d}.log"), &lines, 60 * 30);
            }
            std::thread::scope(|s| {
                for _ in 0..4 {
                    s.spawn(|| prune(folder, 14, DEFAULT_MAX_SIZE, date("2026-11-11")).unwrap());
                }
            });
            let days: Vec<String> = (1..=6)
                .map(|d| format!("theca.2026-11-0{d}.log.gz"))
                .collect();
            assert_eq!(names(folder), days);
            for d in days {
                let mut read = String::new();
                flate2::read::GzDecoder::new(File::open(folder.join(d)).unwrap())
                    .read_to_string(&mut read)
                    .unwrap();
                assert!(read == lines, "a day's lines lost");
            }
        }
    }

    /// Unset, invalid: the defaults; set: as given; `None` resets.
    #[test]
    fn settings_fall_back_to_their_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open(&dir.path().join("state.sqlite")).unwrap();
        let defaults = settings(&state).unwrap();
        assert_eq!(
            defaults,
            Settings {
                level: "info".into(),
                days: 14,
                max_size: DEFAULT_MAX_SIZE,
                folder: Some(dir.path().join("logs")),
                default_folder: true,
            }
        );
        set(&state, Change::Level(Some("debug".into()))).unwrap();
        set(&state, Change::Days(Some(30))).unwrap();
        set(&state, Change::MaxSize(Some(1_000))).unwrap();
        set(&state, Change::Folder(Some("/var/log/theca".into()))).unwrap();
        let s = settings(&state).unwrap();
        assert_eq!(
            (
                s.level.as_str(),
                s.days,
                s.max_size,
                s.folder,
                s.default_folder
            ),
            ("debug", 30, 1_000, Some("/var/log/theca".into()), false)
        );
        state.set_setting("log_level", "loud").unwrap();
        state.set_setting("log_days", "0").unwrap();
        state.set_setting("log_max_size", "big").unwrap();
        set(&state, Change::Folder(None)).unwrap();
        assert_eq!(settings(&state).unwrap(), defaults);
        set(&state, Change::Days(Some(100_000_000))).unwrap();
        assert_eq!(settings(&state).unwrap().days, MAX_DAYS);
        assert!(level("loud").is_err());
        assert_eq!(level("trace").unwrap(), "trace");
    }
}
