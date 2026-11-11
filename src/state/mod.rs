//! The client state: one SQLite file holding everything it keeps (accounts,
//! sync pairs, what was last in sync, notices). No config file: settings
//! change through the CLI or the GUI (docs/decisions.md, Sync). Forward-only:
//! a state an older version wrote is migrated, one step per version
//! (`migrations/`), a copy kept first.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use tokio::sync::broadcast;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use theca_common::api;
use uuid::Uuid;

mod migrations;

/// A fresh state's schema, at `VERSION`. A change to it is a new migration
/// step building the same (the upgrade test compares them).
const SCHEMA: &str = r#"
-- An account on an instance, with this device's app password.
CREATE TABLE account (
    id INTEGER PRIMARY KEY,
    instance TEXT NOT NULL,
    uuid TEXT NOT NULL,
    email TEXT NOT NULL,
    device TEXT NOT NULL,
    -- The app password when it's kept here (no keychain, or `--no-keyring`).
    password TEXT,
    -- Else the name of its entry in the OS keychain (a random ID).
    secret TEXT CHECK ((password IS NULL) <> (secret IS NULL)),
    default_folder TEXT NOT NULL,
    -- Libraries up to this size (bytes) are synced when they appear; NULL: never.
    auto_sync_limit INTEGER,
    -- The server refused the app password (revoked): nothing runs until a new login.
    signed_out INTEGER NOT NULL DEFAULT 0,
    UNIQUE (instance, uuid)
);

-- A local folder kept identical to a whole library.
CREATE TABLE pair (
    id INTEGER PRIMARY KEY,
    account INTEGER NOT NULL REFERENCES account ON DELETE CASCADE,
    library TEXT NOT NULL,
    -- The library's root folder (the parent of the top-level entries).
    root TEXT NOT NULL,
    library_name TEXT NOT NULL,
    path TEXT NOT NULL,
    -- The local folder's file ID: a missing or replaced folder stops the pair.
    root_file_id TEXT,
    -- The last event applied; NULL: the pair needs a resync.
    position INTEGER,
    -- That event's ID, sent back with the position: a server restored from
    -- a backup reuses the numbers past its restore point.
    position_event TEXT,
    paused INTEGER NOT NULL DEFAULT 0,
    -- Why it stopped: its folder (missing, replaced, unreadable...), its
    -- library deleted, or its access removed. NULL: it runs.
    stop TEXT CHECK (stop IN ('folder', 'library_deleted', 'access_removed')),
    -- What the user is told about a stop for its folder.
    stop_detail TEXT,
    writable INTEGER NOT NULL DEFAULT 1,
    -- What it's doing, for status: written by the engine.
    activity TEXT NOT NULL DEFAULT 'waiting',
    -- The account's engine stopped on this error (tried again every 30 s),
    -- until its next activity.
    error TEXT,
    synced_at INTEGER,
    -- A sync pair, or a drive (its files' content fetched when opened).
    mode TEXT NOT NULL DEFAULT 'sync' CHECK (mode IN ('sync', 'drive')),
    -- Drives: the counter stamped on each change to its entries (macOS's
    -- anchors), moved by `theca keep` and `free` (a running engine looks).
    seq INTEGER NOT NULL DEFAULT 0,
    UNIQUE (account, library)
);

-- What was last in sync, per node: the base of the three-way reconciliation.
-- No path: computed through `parent`.
CREATE TABLE entry (
    pair INTEGER NOT NULL REFERENCES pair ON DELETE CASCADE,
    node TEXT NOT NULL,
    parent TEXT NOT NULL,
    -- The server's name (NFC).
    name TEXT NOT NULL,
    is_file INTEGER NOT NULL,
    -- The synced version; NULL for a folder.
    version TEXT,
    size INTEGER NOT NULL DEFAULT 0,
    -- Local modification time, nanoseconds since the epoch.
    mtime INTEGER NOT NULL DEFAULT 0,
    file_id TEXT,
    -- Drives on macOS and FUSE: kept on this computer; a node is kept when
    -- it or an ancestor is.
    kept INTEGER NOT NULL DEFAULT 0,
    -- Drives on Linux and Windows: freed (`theca free`), evicted by the
    -- running engine as soon as it isn't open, then cleared.
    free INTEGER NOT NULL DEFAULT 0,
    -- Drives: the last open seen, else the download (seconds since the epoch).
    used INTEGER,
    -- Drives on macOS: the pair's counter at this entry's last change.
    seq INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (pair, node)
);
CREATE INDEX entry_parent ON entry (pair, parent);

-- Drives on macOS: entries removed, with the pair's counter then, until the
-- system has listed the changes past it.
CREATE TABLE entry_gone (
    pair INTEGER NOT NULL REFERENCES pair ON DELETE CASCADE,
    node TEXT NOT NULL,
    seq INTEGER NOT NULL,
    PRIMARY KEY (pair, node)
);

-- Remote states received from the event feed (or the tree listing), not
-- reconciled yet. `gone`: purged, trashed, or out of reach.
CREATE TABLE remote (
    pair INTEGER NOT NULL REFERENCES pair ON DELETE CASCADE,
    node TEXT NOT NULL,
    gone INTEGER NOT NULL,
    parent TEXT,
    name TEXT,
    is_file INTEGER,
    version TEXT,
    size INTEGER,
    modified INTEGER,
    PRIMARY KEY (pair, node)
);

-- Libraries the user removed: never auto-synced again.
CREATE TABLE declined (
    account INTEGER NOT NULL REFERENCES account ON DELETE CASCADE,
    library TEXT NOT NULL,
    PRIMARY KEY (account, library)
);

-- Errors and notices, shown by the GUI and `status`.
CREATE TABLE notice (
    id INTEGER PRIMARY KEY,
    account INTEGER REFERENCES account ON DELETE CASCADE,
    pair INTEGER REFERENCES pair ON DELETE CASCADE,
    at INTEGER NOT NULL,
    -- error (needs attention) or info.
    level TEXT NOT NULL,
    path TEXT,
    message TEXT NOT NULL,
    -- What `undo` restores: node UUIDs, space-separated.
    undo TEXT
);

-- Settings for the whole computer (the drive limit...): a missing key is its
-- default.
CREATE TABLE setting (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

const VERSION: i32 = 4;

/// The oldest version migrated (`0.2.0`'s): before it, the schema was
/// rewritten in place, and a state is deleted by hand.
const OLDEST: i32 = 2;

/// The migration steps, from `OLDEST` + 1 up to `VERSION`: one per version,
/// never edited once released (`scripts/ci.sh`).
const STEPS: [fn(&rusqlite::Transaction<'_>) -> Result<()>; 2] =
    [migrations::v3::migrate, migrations::v4::migrate];

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("the state database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("the state database is version {0}, this client knows {VERSION}: update theca")]
    Newer(i32),
    #[error(
        "the state database {} was made by an older theca: delete it, then log in and add your folders again (their files stay, and are adopted)",
        .0.display()
    )]
    Older(PathBuf),
    #[error("the state database's migration failed (it is as it was): {0}")]
    Migration(String),
}

pub type Result<T> = std::result::Result<T, StateError>;

/// The state database. The connection sits behind a mutex, locked for one
/// query at a time and never across an `await`: the engine's tasks share it
/// on tokio's threads.
pub struct State {
    connection: Mutex<Connection>,
    /// Where each change the user sees is published (the GUI); none in a
    /// process nobody watches (the CLI).
    updates: Option<broadcast::Sender<Update>>,
    /// Its file (none in memory): no pair's folder may hold it.
    path: Option<PathBuf>,
}

/// A change the user sees, published as it's written: the GUI's window
/// follows them (docs: the sprint 09's GUI). Not an event of the feed.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Update {
    /// A pair's activity ("syncing", "up to date"...).
    Activity { pair: i64, activity: String },
    /// Pairs added, removed, paused, stopped, moved or renamed.
    Pairs,
    /// Accounts added, removed, signed out, or their settings.
    Accounts,
    /// Notices added or gone.
    Notices,
    /// Libraries the account sees and doesn't sync (too big to sync by
    /// themselves, or declined).
    Offered { account: i64, libraries: Vec<Offer> },
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Offer {
    pub library: Uuid,
    pub name: String,
    pub size: i64,
}

/// Where the state lives by default: the OS data directory.
pub fn default_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("rs", "theca", "theca")
        .map(|dirs| dirs.data_dir().join("state.sqlite"))
}

impl State {
    /// Opens (creating) the database at `path`, migrating one an older
    /// version wrote; only the user can read it (it may hold app passwords).
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| {
                rusqlite::Error::InvalidPath(PathBuf::from(format!("{}: {e}", dir.display())))
            })?;
        }
        let mut db = Connection::open(path)?;
        restrict(path);
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        // Refused before anything is written to it.
        let version: i32 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 | VERSION => {}
            newer if newer > VERSION => return Err(StateError::Newer(newer)),
            older if older < OLDEST => return Err(StateError::Older(path.to_owned())),
            _ => {}
        }
        db.pragma_update(None, "journal_mode", "wal")?;
        match version {
            0 => {
                db.execute_batch(SCHEMA)?;
                db.pragma_update(None, "user_version", VERSION)?;
            }
            VERSION => {}
            _ => migrate(&mut db, path)?,
        }
        db.pragma_update(None, "foreign_keys", true)?;
        Ok(Self {
            connection: Mutex::new(db),
            updates: None,
            path: Some(path.to_owned()),
        })
    }

    /// Its file, when it has one.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// An in-memory database (tests).
    pub fn memory() -> Result<Self> {
        let db = Connection::open_in_memory()?;
        db.pragma_update(None, "foreign_keys", true)?;
        db.execute_batch(SCHEMA)?;
        Ok(Self {
            connection: Mutex::new(db),
            updates: None,
            path: None,
        })
    }

    /// This state publishing its changes on `updates`.
    pub fn with_updates(mut self, updates: broadcast::Sender<Update>) -> Self {
        self.updates = Some(updates);
        self
    }

    pub fn publish(&self, update: Update) {
        if let Some(updates) = &self.updates {
            // Nobody listening (the window closed) is fine.
            let _ = updates.send(update);
        }
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding it left nothing half-written: SQLite rolled back.
        self.connection.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Brings an older state up to `VERSION`, every step in one transaction (a
/// crash leaves it as it was), after keeping a copy of it.
fn migrate(db: &mut Connection, path: &Path) -> Result<()> {
    // A step rebuilding a table drops the old one: its foreign keys must not
    // cascade (checked once the steps are done).
    db.pragma_update(None, "foreign_keys", false)?;
    // The write lock first: another process opening it meanwhile (the app, a
    // `theca status`) waits here, then finds it migrated.
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let from: i32 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    match from {
        VERSION => return Ok(()),
        newer if newer > VERSION => return Err(StateError::Newer(newer)),
        _ => {}
    }
    // Through a connection of its own, which reads the state as it was
    // before this transaction.
    keep_copy(&Connection::open(path)?, path, from)?;
    for version in from + 1..=VERSION {
        STEPS[(version - OLDEST - 1) as usize](&tx)?;
    }
    let broken: i64 = tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
        r.get(0)
    })?;
    if broken > 0 {
        return Err(StateError::Migration(format!(
            "{broken} rows lost their parent"
        )));
    }
    tx.pragma_update(None, "user_version", VERSION)?;
    tx.commit()?;
    tracing::info!(path = %path.display(), "the state migrated from version {from} to {VERSION}");
    Ok(())
}

/// A copy of the state before it's migrated (`state.sqlite.v2`), the latest
/// only: a migration's bug would otherwise lose the bases, of which the state
/// holds the only copy. Without the app passwords: the migration may move them
/// into the keychain, and a copy mustn't keep them in plain text, not even in
/// the pages SQLite frees (`secure_delete` overwrites them). Restoring it is by
/// hand, then a login.
fn keep_copy(db: &Connection, path: &Path, from: i32) -> Result<()> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let copy = |version: i32| path.with_file_name(format!("{name}.v{version}"));
    for older in OLDEST..=from {
        let _ = std::fs::remove_file(copy(older));
    }
    let to = copy(from);
    // Readable by the user alone before anything is written into it.
    std::fs::File::create(&to).map_err(|e| {
        rusqlite::Error::InvalidPath(PathBuf::from(format!("{}: {e}", to.display())))
    })?;
    restrict(&to);
    db.execute("VACUUM INTO ?1", [to.to_string_lossy()])?;
    let copied = Connection::open(&to)?;
    copied.pragma_update(None, "secure_delete", true)?;
    // Only where there is one: from version 3, an account whose password is
    // in the keychain has none, and its CHECK refuses both set.
    copied.execute(
        "UPDATE account SET password = '' WHERE password IS NOT NULL",
        [],
    )?;
    Ok(())
}

#[cfg(unix)]
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

// Windows: %APPDATA% is the user's own already.
#[cfg(not(unix))]
fn restrict(_: &Path) {}

/// An account, as stored.
#[derive(Debug, Clone)]
pub struct Account {
    pub id: i64,
    pub instance: String,
    pub uuid: Uuid,
    pub email: String,
    pub device: String,
    pub password: Password,
    pub default_folder: PathBuf,
    pub auto_sync_limit: Option<i64>,
    pub signed_out: bool,
}

/// Where an account's app password is.
#[derive(Debug, Clone, PartialEq)]
pub enum Password {
    /// In the OS keychain, under this entry (`keychain`).
    Keychain(String),
    /// In the state itself: no keychain, or the login asked for it.
    InState(String),
}

/// A sync pair, as stored.
#[derive(Debug, Clone)]
pub struct Pair {
    pub id: i64,
    pub account: i64,
    pub library: Uuid,
    pub root: Uuid,
    pub library_name: String,
    pub path: PathBuf,
    pub root_file_id: Option<String>,
    pub position: Option<i64>,
    pub position_event: Option<Uuid>,
    pub paused: bool,
    pub stop: Option<Stop>,
    pub writable: bool,
    /// What the engine is doing with it, and when it last finished a round.
    pub activity: String,
    /// The account's engine stopped on this error, until its next activity.
    pub error: Option<String>,
    pub synced_at: Option<DateTime<Utc>>,
    pub mode: Mode,
}

/// What a pair keeps on this computer: everything (a sync pair), or what is
/// opened, within the drive limit (a drive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Sync,
    Drive,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Sync => "sync",
            Mode::Drive => "drive",
        }
    }

    /// The column, which its CHECK limits to these two.
    fn from_column(mode: &str) -> Mode {
        if mode == "drive" {
            Mode::Drive
        } else {
            Mode::Sync
        }
    }
}

/// Why a pair stopped: stored as a kind, so that rewording what the user is
/// told never changes what the code does with it.
#[derive(Debug, Clone, PartialEq)]
pub enum Stop {
    /// Its local folder (missing, replaced, unreadable, in the trash...): the
    /// user locates it, or it resumes once the folder is back.
    Folder(String),
    /// It resumes by itself once the library comes back.
    LibraryDeleted,
    AccessRemoved,
}

impl Stop {
    /// What the user is told.
    pub fn reason(&self) -> &str {
        match self {
            Stop::Folder(why) => why,
            Stop::LibraryDeleted => "library deleted",
            Stop::AccessRemoved => "access removed",
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Stop::Folder(_) => "folder",
            Stop::LibraryDeleted => "library_deleted",
            Stop::AccessRemoved => "access_removed",
        }
    }

    fn from_columns(kind: Option<String>, detail: Option<String>) -> Option<Stop> {
        Some(match kind?.as_str() {
            "library_deleted" => Stop::LibraryDeleted,
            "access_removed" => Stop::AccessRemoved,
            _ => Stop::Folder(detail.unwrap_or_default()),
        })
    }
}

impl Pair {
    /// Why it syncs nothing until someone acts, or a retry succeeds: stopped,
    /// or its account's engine stopped on an error (started again within
    /// 30 s, `engine::stopped`).
    pub fn not_syncing(&self) -> Option<&str> {
        self.stop
            .as_ref()
            .map(Stop::reason)
            .or(self.error.as_deref())
    }

    /// Stopped because of its local folder (missing, in the trash,
    /// unreadable): the user locates it, or tries again.
    pub fn stopped_for_its_folder(&self) -> bool {
        matches!(self.stop, Some(Stop::Folder(_)))
    }
}

/// What was last in sync for one node.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub node: Uuid,
    pub parent: Uuid,
    pub name: String,
    pub is_file: bool,
    pub version: Option<Uuid>,
    pub size: i64,
    pub mtime: i64,
    pub file_id: Option<String>,
}

/// An entry's columns, in `entry_row`'s order.
const ENTRY: &str = "node, parent, name, is_file, version, size, mtime, file_id";

fn entry_row(r: &rusqlite::Row) -> rusqlite::Result<Entry> {
    Ok(Entry {
        node: uuid(r.get(0)?),
        parent: uuid(r.get(1)?),
        name: r.get(2)?,
        is_file: r.get(3)?,
        version: r.get::<_, Option<String>>(4)?.map(uuid),
        size: r.get(5)?,
        mtime: r.get(6)?,
        file_id: r.get(7)?,
    })
}

/// A remote state not reconciled yet; `None`: gone.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteState {
    pub parent: Uuid,
    pub name: String,
    pub is_file: bool,
    pub version: Option<Uuid>,
    pub size: i64,
    pub modified: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub id: i64,
    pub account: Option<i64>,
    pub pair: Option<i64>,
    pub at: DateTime<Utc>,
    pub level: String,
    pub path: Option<String>,
    pub message: String,
    pub undo: Option<String>,
}

fn uuid(text: String) -> Uuid {
    text.parse().unwrap_or_default()
}

impl State {
    pub fn accounts(&self) -> Result<Vec<Account>> {
        let db = self.db();
        let mut query = db.prepare(
            "SELECT id, instance, uuid, email, device, password, secret, default_folder,
                    auto_sync_limit, signed_out
             FROM account ORDER BY id",
        )?;
        let rows = query.query_map([], |r| {
            Ok(Account {
                id: r.get(0)?,
                instance: r.get(1)?,
                uuid: uuid(r.get(2)?),
                email: r.get(3)?,
                device: r.get(4)?,
                // The schema holds one or the other.
                password: match r.get::<_, Option<String>>(5)? {
                    Some(password) => Password::InState(password),
                    None => Password::Keychain(r.get(6)?),
                },
                default_folder: PathBuf::from(r.get::<_, String>(7)?),
                auto_sync_limit: r.get(8)?,
                signed_out: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn account(&self, id: i64) -> Result<Option<Account>> {
        Ok(self.accounts()?.into_iter().find(|a| a.id == id))
    }

    /// Adds an account, or replaces the app password of the same account on
    /// the same instance (signing in again). Its ID.
    pub fn save_account(&self, a: &Account) -> Result<i64> {
        let (password, secret) = match &a.password {
            Password::InState(password) => (Some(password), None),
            Password::Keychain(secret) => (None, Some(secret)),
        };
        self.db().execute(
            "INSERT INTO account (instance, uuid, email, device, password, secret, default_folder, auto_sync_limit)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (instance, uuid) DO UPDATE SET
                email = excluded.email, device = excluded.device, password = excluded.password,
                secret = excluded.secret, signed_out = 0",
            params![
                a.instance,
                a.uuid.to_string(),
                a.email,
                a.device,
                password,
                secret,
                a.default_folder.to_string_lossy(),
                a.auto_sync_limit
            ],
        )?;
        self.publish(Update::Accounts);
        Ok(self.db().query_row(
            "SELECT id FROM account WHERE instance = ?1 AND uuid = ?2",
            params![a.instance, a.uuid.to_string()],
            |r| r.get(0),
        )?)
    }

    pub fn set_signed_out(&self, account: i64) -> Result<()> {
        self.db()
            .execute("UPDATE account SET signed_out = 1 WHERE id = ?1", [account])?;
        self.publish(Update::Accounts);
        Ok(())
    }

    pub fn set_auto_sync_limit(&self, account: i64, limit: Option<i64>) -> Result<()> {
        self.db().execute(
            "UPDATE account SET auto_sync_limit = ?2 WHERE id = ?1",
            params![account, limit],
        )?;
        self.publish(Update::Accounts);
        Ok(())
    }

    pub fn set_default_folder(&self, account: i64, folder: &Path) -> Result<()> {
        self.db().execute(
            "UPDATE account SET default_folder = ?2 WHERE id = ?1",
            params![account, folder.to_string_lossy()],
        )?;
        self.publish(Update::Accounts);
        Ok(())
    }

    pub fn remove_account(&self, account: i64) -> Result<()> {
        self.db()
            .execute("DELETE FROM account WHERE id = ?1", [account])?;
        self.publish(Update::Accounts);
        Ok(())
    }

    pub fn pairs(&self) -> Result<Vec<Pair>> {
        let db = self.db();
        let mut query = db.prepare(
            "SELECT id, account, library, root, library_name, path, root_file_id, position,
                    paused, stop, writable, activity, synced_at, position_event, stop_detail, error,
                    mode
             FROM pair ORDER BY id",
        )?;
        let rows = query.query_map([], |r| {
            Ok(Pair {
                id: r.get(0)?,
                account: r.get(1)?,
                library: uuid(r.get(2)?),
                root: uuid(r.get(3)?),
                library_name: r.get(4)?,
                path: PathBuf::from(r.get::<_, String>(5)?),
                root_file_id: r.get(6)?,
                position: r.get(7)?,
                paused: r.get(8)?,
                stop: Stop::from_columns(r.get(9)?, r.get(14)?),
                writable: r.get(10)?,
                activity: r.get(11)?,
                error: r.get(15)?,
                synced_at: r
                    .get::<_, Option<i64>>(12)?
                    .and_then(DateTime::from_timestamp_millis),
                position_event: r.get::<_, Option<String>>(13)?.map(uuid),
                mode: Mode::from_column(&r.get::<_, String>(16)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn pair(&self, id: i64) -> Result<Option<Pair>> {
        Ok(self.pairs()?.into_iter().find(|p| p.id == id))
    }

    pub fn add_pair(&self, p: &Pair) -> Result<i64> {
        self.db().execute(
            "INSERT INTO pair (account, library, root, library_name, path, writable, mode)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                p.account,
                p.library.to_string(),
                p.root.to_string(),
                p.library_name,
                p.path.to_string_lossy(),
                p.writable,
                p.mode.as_str()
            ],
        )?;
        self.db().execute(
            "DELETE FROM declined WHERE account = ?1 AND library = ?2",
            params![p.account, p.library.to_string()],
        )?;
        self.publish(Update::Pairs);
        Ok(self.db().last_insert_rowid())
    }

    /// Removes a pair: its state goes, and the library is never auto-synced again.
    pub fn remove_pair(&self, pair: &Pair) -> Result<()> {
        self.db().execute(
            "INSERT OR IGNORE INTO declined (account, library) VALUES (?1, ?2)",
            params![pair.account, pair.library.to_string()],
        )?;
        self.db()
            .execute("DELETE FROM pair WHERE id = ?1", [pair.id])?;
        self.publish(Update::Pairs);
        Ok(())
    }

    pub fn declined(&self, account: i64) -> Result<Vec<Uuid>> {
        let db = self.db();
        let mut query = db.prepare("SELECT library FROM declined WHERE account = ?1")?;
        let rows = query.query_map([account], |r| Ok(uuid(r.get(0)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_paused(&self, pair: i64, paused: bool) -> Result<()> {
        self.db().execute(
            "UPDATE pair SET paused = ?2 WHERE id = ?1",
            params![pair, paused],
        )?;
        self.publish(Update::Pairs);
        Ok(())
    }

    pub fn set_stopped(&self, pair: i64, stop: Option<&Stop>) -> Result<()> {
        let detail = match stop {
            Some(Stop::Folder(why)) => Some(why.as_str()),
            _ => None,
        };
        self.db().execute(
            "UPDATE pair SET stop = ?2, stop_detail = ?3 WHERE id = ?1",
            params![pair, stop.map(Stop::kind), detail],
        )?;
        self.publish(Update::Pairs);
        Ok(())
    }

    pub fn set_pair_path(&self, pair: i64, path: &Path, file_id: Option<&str>) -> Result<()> {
        self.db().execute(
            "UPDATE pair SET path = ?2, root_file_id = ?3 WHERE id = ?1",
            params![pair, path.to_string_lossy(), file_id],
        )?;
        self.publish(Update::Pairs);
        Ok(())
    }

    /// What the engine does with the pair now (an error it stopped on is
    /// over).
    pub fn set_activity(&self, pair: i64, activity: &str, synced: bool) -> Result<()> {
        self.db().execute(
            "UPDATE pair SET activity = ?2, error = NULL,
                    synced_at = CASE WHEN ?3 THEN ?4 ELSE synced_at END
             WHERE id = ?1",
            params![pair, activity, synced, Utc::now().timestamp_millis()],
        )?;
        self.publish(Update::Activity {
            pair,
            activity: activity.to_owned(),
        });
        Ok(())
    }

    /// The account's engine stopped on `error`: said on the pair until its
    /// next activity.
    pub fn set_error(&self, pair: i64, error: &str) -> Result<()> {
        self.db().execute(
            "UPDATE pair SET error = ?2 WHERE id = ?1",
            params![pair, error],
        )?;
        self.publish(Update::Pairs);
        Ok(())
    }

    pub fn set_library(&self, pair: i64, name: &str, writable: bool) -> Result<()> {
        self.db().execute(
            "UPDATE pair SET library_name = ?2, writable = ?3 WHERE id = ?1",
            params![pair, name, writable],
        )?;
        self.publish(Update::Pairs);
        Ok(())
    }

    /// Asks for a resync: the position forgotten (a `reset`, a lost state).
    /// Forgets what was synced: the next round is a first sync into the
    /// folder (a merge), never a comparison with a base it doesn't match.
    pub fn forget_base(&self, pair: i64) -> Result<()> {
        // One transaction, the position first: a round's `synced` meanwhile
        // finds it NULL and writes nothing back.
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute(
            "UPDATE pair SET position = NULL, position_event = NULL WHERE id = ?1",
            [pair],
        )?;
        tx.execute("DELETE FROM entry WHERE pair = ?1", [pair])?;
        tx.execute("DELETE FROM remote WHERE pair = ?1", [pair])?;
        tx.commit()?;
        drop(db);
        self.publish(Update::Pairs);
        Ok(())
    }

    pub fn reset_position(&self, pair: i64) -> Result<()> {
        self.db().execute(
            "UPDATE pair SET position = NULL, position_event = NULL WHERE id = ?1",
            [pair],
        )?;
        Ok(())
    }

    pub fn entries(&self, pair: i64) -> Result<Vec<Entry>> {
        let db = self.db();
        let mut query = db.prepare(&format!("SELECT {ENTRY} FROM entry WHERE pair = ?1"))?;
        let rows = query.query_map([pair], entry_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn entry(&self, pair: i64, node: Uuid) -> Result<Option<Entry>> {
        Ok(self
            .db()
            .query_row(
                &format!("SELECT {ENTRY} FROM entry WHERE pair = ?1 AND node = ?2"),
                params![pair, node.to_string()],
                entry_row,
            )
            .optional()?)
    }

    /// The entry at a path below a pair's root, its names matched by key, as
    /// the scan matches them (a drive's file opened through its mount).
    pub fn entry_at(&self, pair: i64, root: Uuid, names: &[String]) -> Result<Option<Entry>> {
        let db = self.db();
        let mut query = db.prepare(&format!(
            "SELECT {ENTRY} FROM entry WHERE pair = ?1 AND parent = ?2"
        ))?;
        let mut found = None;
        let mut parent = root;
        for name in names {
            let key = crate::local::key(name);
            let children = query.query_map(params![pair, parent.to_string()], entry_row)?;
            let mut child = None;
            for e in children {
                let e = e?;
                if crate::local::key(&e.name) == key {
                    child = Some(e);
                    break;
                }
            }
            let Some(e) = child else {
                return Ok(None);
            };
            parent = e.node;
            found = Some(e);
        }
        Ok(found)
    }

    /// A drive's files' last use (opened, else downloaded), Unix seconds.
    pub fn used(&self, pair: i64) -> Result<HashMap<Uuid, i64>> {
        let db = self.db();
        let mut query =
            db.prepare("SELECT node, used FROM entry WHERE pair = ?1 AND used IS NOT NULL")?;
        let rows = query.query_map([pair], |r| Ok((uuid(r.get(0)?), r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A drive's node kept on this computer, or not (macOS and FUSE): a
    /// change to its entries, stamped with the pair's counter (what a
    /// running engine, in another process, looks for).
    pub fn set_kept(&self, pair: i64, node: Uuid, kept: bool) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute("UPDATE pair SET seq = seq + 1 WHERE id = ?1", [pair])?;
        tx.execute(
            "UPDATE entry SET kept = ?3, seq = (SELECT seq FROM pair WHERE id = ?1)
             WHERE pair = ?1 AND node = ?2",
            params![pair, node.to_string(), kept],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A drive's file or folder freed (`theca free`): no longer kept, and
    /// evicted as soon as possible by the running engine (the pair's counter
    /// tells it), or at once by the caller when none runs.
    pub fn set_free(&self, pair: i64, node: Uuid) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute("UPDATE pair SET seq = seq + 1 WHERE id = ?1", [pair])?;
        tx.execute(
            "UPDATE entry SET kept = 0, free = 1, seq = (SELECT seq FROM pair WHERE id = ?1)
             WHERE pair = ?1 AND node = ?2",
            params![pair, node.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A drive's nodes freed, waiting for their eviction.
    pub fn freed(&self, pair: i64) -> Result<Vec<Uuid>> {
        let db = self.db();
        let mut query = db.prepare("SELECT node FROM entry WHERE pair = ?1 AND free = 1")?;
        let rows = query.query_map([pair], |r| Ok(uuid(r.get(0)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A freed node's files all evicted (or edited since: they stay).
    pub fn clear_free(&self, pair: i64, node: Uuid) -> Result<()> {
        self.db().execute(
            "UPDATE entry SET free = 0 WHERE pair = ?1 AND node = ?2",
            params![pair, node.to_string()],
        )?;
        Ok(())
    }

    /// A folder's entries (`parent`: the pair's root for the top level).
    pub fn children(&self, pair: i64, parent: Uuid) -> Result<Vec<Entry>> {
        let db = self.db();
        let mut query = db.prepare(&format!(
            "SELECT {ENTRY} FROM entry WHERE pair = ?1 AND parent = ?2"
        ))?;
        let rows = query.query_map(params![pair, parent.to_string()], entry_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A macOS drive's entry as it is now, recorded, stamped with the pair's
    /// counter (its change listed past the anchors before it).
    pub fn stamp(&self, pair: i64, e: &Entry) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute("UPDATE pair SET seq = seq + 1 WHERE id = ?1", [pair])?;
        tx.execute(
            "INSERT INTO entry (pair, node, parent, name, is_file, version, size, mtime, file_id, seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, (SELECT seq FROM pair WHERE id = ?1))
             ON CONFLICT (pair, node) DO UPDATE SET
                 parent = excluded.parent, name = excluded.name, is_file = excluded.is_file,
                 version = excluded.version, size = excluded.size, mtime = excluded.mtime,
                 seq = excluded.seq",
            params![
                pair,
                e.node.to_string(),
                e.parent.to_string(),
                e.name,
                e.is_file,
                e.version.map(|v| v.to_string()),
                e.size,
                e.mtime,
                e.file_id
            ],
        )?;
        tx.execute(
            "DELETE FROM entry_gone WHERE pair = ?1 AND node = ?2",
            params![pair, e.node.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A macOS drive's node gone, and what is below it: each listed as gone
    /// past the anchors before it.
    pub fn stamp_gone(&self, pair: i64, node: Uuid) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute("UPDATE pair SET seq = seq + 1 WHERE id = ?1", [pair])?;
        tx.execute(
            "WITH RECURSIVE below(node) AS (
                 SELECT ?2
                 UNION SELECT e.node FROM entry e JOIN below b ON e.parent = b.node WHERE e.pair = ?1
             )
             INSERT INTO entry_gone (pair, node, seq)
                 SELECT ?1, e.node, (SELECT seq FROM pair WHERE id = ?1)
                 FROM entry e WHERE e.pair = ?1 AND e.node IN (SELECT node FROM below)
             ON CONFLICT (pair, node) DO UPDATE SET seq = excluded.seq",
            params![pair, node.to_string()],
        )?;
        tx.execute(
            "WITH RECURSIVE below(node) AS (
                 SELECT ?2
                 UNION SELECT e.node FROM entry e JOIN below b ON e.parent = b.node WHERE e.pair = ?1
             )
             DELETE FROM entry WHERE pair = ?1 AND node IN (SELECT node FROM below)",
            params![pair, node.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A macOS drive's changes past `anchor`: the entries changed, the nodes
    /// gone, and the counter now. Gone rows stay for the drive's life: a
    /// system asking from an older anchor (restored) still learns of them.
    pub fn changes_since(&self, pair: i64, anchor: i64) -> Result<(Vec<Entry>, Vec<Uuid>, i64)> {
        let db = self.db();
        let changed = {
            let mut query = db.prepare(&format!(
                "SELECT {ENTRY} FROM entry WHERE pair = ?1 AND seq > ?2"
            ))?;
            let rows = query.query_map(params![pair, anchor], entry_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let gone = {
            let mut query =
                db.prepare("SELECT node FROM entry_gone WHERE pair = ?1 AND seq > ?2")?;
            let rows = query.query_map(params![pair, anchor], |r| Ok(uuid(r.get(0)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let now = db.query_row("SELECT seq FROM pair WHERE id = ?1", [pair], |r| r.get(0))?;
        Ok((changed, gone, now))
    }

    /// Each pair's counter of changes to its entries.
    pub fn seqs(&self) -> Result<HashMap<i64, i64>> {
        let db = self.db();
        let mut query = db.prepare("SELECT id, seq FROM pair")?;
        let rows = query.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A drive's file last used (opened, else downloaded), in Unix seconds.
    pub fn set_used(&self, pair: i64, node: Uuid, at: i64) -> Result<()> {
        self.db().execute(
            "UPDATE entry SET used = ?3 WHERE pair = ?1 AND node = ?2",
            params![pair, node.to_string(), at],
        )?;
        Ok(())
    }

    /// A drive's nodes kept on this computer (macOS and FUSE; Windows keeps
    /// its pins on the files).
    pub fn kept(&self, pair: i64) -> Result<HashSet<Uuid>> {
        let db = self.db();
        let mut query = db.prepare("SELECT node FROM entry WHERE pair = ?1 AND kept = 1")?;
        let rows = query.query_map([pair], |r| Ok(uuid(r.get(0)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The drive limit set (`theca drive-limit`), in bytes; `None`: the
    /// default (`drive::limit`). A value that isn't one is the default too,
    /// logged.
    pub fn drive_limit(&self) -> Result<Option<u64>> {
        let value = self.setting("drive_limit")?;
        Ok(value.and_then(|v| {
            v.parse()
                .inspect_err(|_| tracing::warn!(value = v, "drive_limit isn't a size: the default"))
                .ok()
        }))
    }

    /// Back to the default.
    pub fn clear_drive_limit(&self) -> Result<()> {
        self.db()
            .execute("DELETE FROM setting WHERE key = 'drive_limit'", [])?;
        Ok(())
    }

    pub fn set_drive_limit(&self, bytes: u64) -> Result<()> {
        self.db().execute(
            "INSERT INTO setting (key, value) VALUES ('drive_limit', ?1)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [bytes.to_string()],
        )?;
        Ok(())
    }

    /// A setting for the whole computer, as stored: each has its accessor,
    /// the one place that parses it (a key nobody reads is ignored).
    fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .db()
            .query_row("SELECT value FROM setting WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// A node synced, in one transaction: its entry (dropped once the base
    /// is forgotten: `locate` while a round still runs would rebuild a base
    /// the next round, a merge, must not compare with; rounds run one at a
    /// time, so the next resync, which sets the position again, comes after
    /// it), its path's notices gone, the remote states it reconciles cleared.
    pub fn synced(&self, pair: i64, e: &Entry, path: Option<&str>, settled: &[Uuid]) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO entry (pair, node, parent, name, is_file, version, size, mtime, file_id)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9
             WHERE EXISTS (SELECT 1 FROM pair WHERE id = ?1 AND position IS NOT NULL)
             -- An update, not a replace: a drive's columns (kept, used,
             -- seq) aren't the base's, and stay.
             ON CONFLICT (pair, node) DO UPDATE SET
                 parent = excluded.parent, name = excluded.name, is_file = excluded.is_file,
                 version = excluded.version, size = excluded.size, mtime = excluded.mtime,
                 file_id = excluded.file_id",
            params![
                pair,
                e.node.to_string(),
                e.parent.to_string(),
                e.name,
                e.is_file,
                e.version.map(|v| v.to_string()),
                e.size,
                e.mtime,
                e.file_id
            ],
        )?;
        let cleared = match path {
            Some(path) => tx.execute(
                "DELETE FROM notice WHERE pair = ?1 AND path = ?2 AND undo IS NULL",
                params![pair, path],
            )?,
            None => 0,
        };
        for node in settled {
            tx.execute(
                "DELETE FROM remote WHERE pair = ?1 AND node = ?2",
                params![pair, node.to_string()],
            )?;
        }
        tx.commit()?;
        drop(db);
        // On every synced file: published only when something went.
        if cleared > 0 {
            self.publish(Update::Notices);
        }
        Ok(())
    }

    /// Forgets a node and everything below it.
    pub fn delete_entry(&self, pair: i64, node: Uuid) -> Result<()> {
        self.db().execute(
            "WITH RECURSIVE below(node) AS (
                 SELECT ?2
                 UNION ALL
                 SELECT e.node FROM entry e JOIN below b ON e.parent = b.node WHERE e.pair = ?1
             )
             DELETE FROM entry WHERE pair = ?1 AND node IN (SELECT node FROM below)",
            params![pair, node.to_string()],
        )?;
        Ok(())
    }

    /// A listing of the library (a resync), its whole remote truth: what was
    /// pending before goes, `states` and the position replace it, in one
    /// transaction (a position is never ahead of what was recorded).
    pub fn record_remote(
        &self,
        pair: i64,
        states: &[(Uuid, Option<RemoteState>)],
        position: Option<api::Position>,
    ) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute("DELETE FROM remote WHERE pair = ?1", [pair])?;
        for (node, state) in states {
            put_remote(&tx, pair, *node, state.as_ref())?;
        }
        if let Some(position) = position {
            set_position(&tx, pair, position)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Changes from the feed, added to what is pending, and its new position;
    /// nothing at all for a pair whose base was forgotten meanwhile (`locate`
    /// during the feed's await): its resync must still come.
    pub fn record_feed(
        &self,
        pair: i64,
        states: &[(Uuid, Option<RemoteState>)],
        position: Option<api::Position>,
    ) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let followed: bool = tx.query_row(
            "SELECT position IS NOT NULL FROM pair WHERE id = ?1",
            [pair],
            |r| r.get(0),
        )?;
        if !followed {
            return Ok(());
        }
        for (node, state) in states {
            put_remote(&tx, pair, *node, state.as_ref())?;
        }
        if let Some(position) = position {
            set_position(&tx, pair, position)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remote(&self, pair: i64) -> Result<Vec<(Uuid, Option<RemoteState>)>> {
        let db = self.db();
        let mut query = db.prepare(
            "SELECT node, gone, parent, name, is_file, version, size, modified FROM remote WHERE pair = ?1",
        )?;
        let rows = query.query_map([pair], |r| {
            let node = uuid(r.get(0)?);
            let gone: bool = r.get(1)?;
            if gone {
                return Ok((node, None));
            }
            Ok((
                node,
                Some(RemoteState {
                    parent: uuid(r.get::<_, Option<String>>(2)?.unwrap_or_default()),
                    name: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    is_file: r.get::<_, Option<bool>>(4)?.unwrap_or_default(),
                    version: r.get::<_, Option<String>>(5)?.map(uuid),
                    size: r.get::<_, Option<i64>>(6)?.unwrap_or_default(),
                    modified: r
                        .get::<_, Option<i64>>(7)?
                        .map(DateTime::from_timestamp_nanos),
                }),
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The remote states reconciled: forgotten.
    pub fn clear_remote(&self, pair: i64, nodes: &[Uuid]) -> Result<()> {
        let db = self.db();
        let mut delete = db.prepare_cached("DELETE FROM remote WHERE pair = ?1 AND node = ?2")?;
        for node in nodes {
            delete.execute(params![pair, node.to_string()])?;
        }
        Ok(())
    }

    pub fn notify(
        &self,
        account: Option<i64>,
        pair: Option<i64>,
        level: &str,
        path: Option<&str>,
        message: &str,
        undo: Option<&str>,
    ) -> Result<()> {
        // The same error again replaces the old one.
        self.db().execute(
            "DELETE FROM notice WHERE pair IS ?1 AND account IS ?2 AND path IS ?3 AND message = ?4",
            params![pair, account, path, message],
        )?;
        self.db().execute(
            "INSERT INTO notice (account, pair, at, level, path, message, undo)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                account,
                pair,
                Utc::now().timestamp_millis(),
                level,
                path,
                message,
                undo
            ],
        )?;
        self.publish(Update::Notices);
        Ok(())
    }

    pub fn notices(&self) -> Result<Vec<Notice>> {
        let db = self.db();
        let mut query = db.prepare(
            "SELECT id, pair, at, level, path, message, undo, account
             FROM notice ORDER BY at DESC, id DESC LIMIT 500",
        )?;
        let rows = query.query_map([], |r| {
            Ok(Notice {
                id: r.get(0)?,
                account: r.get(7)?,
                pair: r.get(1)?,
                at: DateTime::from_timestamp_millis(r.get(2)?).unwrap_or_default(),
                level: r.get(3)?,
                path: r.get(4)?,
                message: r.get(5)?,
                undo: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn dismiss(&self, notice: i64) -> Result<()> {
        self.db()
            .execute("DELETE FROM notice WHERE id = ?1", [notice])?;
        self.publish(Update::Notices);
        Ok(())
    }

    pub fn latest_undo(&self) -> Result<Option<Notice>> {
        Ok(self.notices()?.into_iter().find(|n| n.undo.is_some()))
    }

    /// The pairs whose base or pending remote states know `node`.
    pub fn pairs_of_node(&self, node: Uuid) -> Result<Vec<i64>> {
        let db = self.db();
        let mut query = db.prepare_cached(
            "SELECT pair FROM entry WHERE node = ?1 UNION SELECT pair FROM remote WHERE node = ?1",
        )?;
        let rows = query.query_map([node.to_string()], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether the pair's base or pending states know `node`.
    pub fn knows(&self, pair: i64, node: Uuid) -> Result<bool> {
        Ok(self.db().query_row(
            "SELECT EXISTS (SELECT 1 FROM entry WHERE pair = ?1 AND node = ?2)
                 OR EXISTS (SELECT 1 FROM remote WHERE pair = ?1 AND node = ?2)",
            params![pair, node.to_string()],
            |r| r.get(0),
        )?)
    }

    /// The account of a pair (`None`: removed meanwhile).
    pub fn pair_account(&self, pair: i64) -> Result<Option<i64>> {
        Ok(self
            .db()
            .query_row("SELECT account FROM pair WHERE id = ?1", [pair], |r| {
                r.get(0)
            })
            .optional()?)
    }
}

/// The pair's position: the last event applied, by number and ID.
fn set_position(db: &rusqlite::Connection, pair: i64, position: api::Position) -> Result<()> {
    db.execute(
        "UPDATE pair SET position = ?2, position_event = ?3 WHERE id = ?1",
        params![
            pair,
            position.seq(),
            position.event().map(|e| e.to_string())
        ],
    )?;
    Ok(())
}

fn put_remote(
    tx: &rusqlite::Transaction<'_>,
    pair: i64,
    node: Uuid,
    state: Option<&RemoteState>,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO remote (pair, node, gone, parent, name, is_file, version, size, modified)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            pair,
            node.to_string(),
            state.is_none(),
            state.map(|s| s.parent.to_string()),
            state.map(|s| s.name.clone()),
            state.map(|s| s.is_file),
            state.and_then(|s| s.version.map(|v| v.to_string())),
            state.map(|s| s.size),
            state.and_then(|s| s.modified.and_then(|m| m.timestamp_nanos_opt()))
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pair syncs nothing when stopped, or when its account's engine
    /// stopped on an error (said until a retry goes through).
    #[test]
    fn not_syncing_when_stopped_or_its_engine_failed() {
        let state = State::memory().unwrap();
        let mut p = state.pair(pair(&state)).unwrap().unwrap();
        assert_eq!(p.not_syncing(), None);
        p.error = Some("the state database: disk full".into());
        assert_eq!(p.not_syncing(), Some("the state database: disk full"));
        p.stop = Some(Stop::Folder("x is missing".into()));
        assert_eq!(p.not_syncing(), Some("x is missing"));
    }

    /// An error the engine stopped on is over at the pair's next activity.
    #[test]
    fn an_error_lasts_until_the_next_activity() {
        let state = State::memory().unwrap();
        let p = pair(&state);
        state.set_error(p, "boom").unwrap();
        assert_eq!(
            state.pair(p).unwrap().unwrap().error.as_deref(),
            Some("boom")
        );
        state.set_activity(p, "syncing", false).unwrap();
        assert_eq!(state.pair(p).unwrap().unwrap().error, None);
    }

    /// A stop is read back as it was set, its kind and what the user is told.
    #[test]
    fn a_stop_round_trips() {
        let state = State::memory().unwrap();
        let p = pair(&state);
        for stop in [
            Stop::Folder("the folder is missing".into()),
            Stop::LibraryDeleted,
            Stop::AccessRemoved,
        ] {
            state.set_stopped(p, Some(&stop)).unwrap();
            assert_eq!(state.pair(p).unwrap().unwrap().stop, Some(stop));
        }
        state.set_stopped(p, None).unwrap();
        assert_eq!(state.pair(p).unwrap().unwrap().stop, None);
    }

    /// A state from before the first migrated version (`0.1.0`'s) is refused,
    /// naming the file to delete: its schema was rewritten in place since.
    #[test]
    fn a_state_older_than_migrations_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", OLDEST - 1)
            .unwrap();
        let e = State::open(&path).err().expect("refused");
        assert!(matches!(e, StateError::Older(_)), "{e}");
        assert!(e.to_string().contains(&path.display().to_string()), "{e}");
    }

    /// The copy kept before migrating holds no app password, not even in the
    /// pages SQLite freed (a long one spills onto pages of its own).
    #[test]
    fn the_copy_before_migrating_keeps_no_password() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        let db = Connection::open(&path).unwrap();
        let password = "secret-".repeat(1000);
        db.execute_batch("CREATE TABLE account (id INTEGER PRIMARY KEY, password TEXT NOT NULL)")
            .unwrap();
        db.execute("INSERT INTO account (password) VALUES (?1)", [&password])
            .unwrap();
        keep_copy(&db, &path, OLDEST).unwrap();
        let copy = std::fs::read(dir.path().join(format!("state.sqlite.v{OLDEST}"))).unwrap();
        assert!(
            !copy
                .windows(7 * 10)
                .any(|w| w == &password.as_bytes()[..70])
        );
        let kept: String = Connection::open(dir.path().join(format!("state.sqlite.v{OLDEST}")))
            .unwrap()
            .query_row("SELECT password FROM account", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, "");
    }

    /// A state another process migrated meanwhile isn't migrated again, and
    /// its copy isn't replaced by the migrated state.
    #[test]
    fn a_state_migrated_meanwhile_is_left_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        State::open(&path).unwrap();
        let mut db = Connection::open(&path).unwrap();
        migrate(&mut db, &path).unwrap();
        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            !files
                .iter()
                .any(|f| f.to_string_lossy().contains(".sqlite.v")),
            "{files:?}"
        );
    }

    fn pair(state: &State) -> i64 {
        let account = state
            .save_account(&Account {
                id: 0,
                instance: "https://x".into(),
                uuid: Uuid::from_u128(1),
                email: "a@x".into(),
                device: "d".into(),
                password: Password::InState("p".into()),
                default_folder: "/tmp".into(),
                auto_sync_limit: None,
                signed_out: false,
            })
            .unwrap();
        state
            .add_pair(&Pair {
                id: 0,
                account,
                library: Uuid::from_u128(2),
                root: Uuid::from_u128(3),
                library_name: "Docs".into(),
                path: "/tmp/Docs".into(),
                root_file_id: None,
                position: None,
                position_event: None,
                paused: false,
                stop: None,
                writable: true,
                activity: String::new(),
                error: None,
                synced_at: None,
                mode: Mode::Sync,
            })
            .unwrap()
    }

    fn entry() -> Entry {
        Entry {
            node: Uuid::from_u128(4),
            parent: Uuid::from_u128(3),
            name: "a.txt".into(),
            is_file: true,
            version: None,
            size: 1,
            mtime: 0,
            file_id: None,
        }
    }

    /// A path finds its entry by key (case, normalization), folder by
    /// folder from the pair's root; a path nobody knows, nothing.
    #[test]
    fn an_entry_is_found_by_its_path() {
        let state = State::memory().unwrap();
        let pair = pair(&state);
        let root = Uuid::from_u128(3);
        let folder = Entry {
            node: Uuid::from_u128(5),
            parent: root,
            name: "Caf\u{e9}".into(),
            is_file: false,
            ..entry()
        };
        let file = Entry {
            parent: folder.node,
            ..entry()
        };
        state
            .record_remote(pair, &[], Some(theca_common::api::Position::Seq(1)))
            .unwrap();
        state.synced(pair, &folder, None, &[]).unwrap();
        state.synced(pair, &file, None, &[]).unwrap();
        let at = |names: &[&str]| {
            let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
            state.entry_at(pair, root, &names).unwrap().map(|e| e.node)
        };
        assert_eq!(at(&["cafe\u{301}", "A.TXT"]), Some(file.node));
        assert_eq!(at(&["Caf\u{e9}"]), Some(folder.node));
        assert_eq!(at(&["a.txt"]), None);
        assert_eq!(at(&["Caf\u{e9}", "b.txt"]), None);
        state.set_used(pair, file.node, 1_800_000_000).unwrap();
        let used: Option<i64> = state
            .db()
            .query_row(
                "SELECT used FROM entry WHERE node = ?1",
                [file.node.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(used, Some(1_800_000_000));
    }

    /// A macOS drive's changes since an anchor: what changed, what went (a
    /// folder with what is below it), past that anchor only, from an older
    /// anchor too.
    #[test]
    fn changes_since_an_anchor() {
        let state = State::memory().unwrap();
        let pair = pair(&state);
        let root = Uuid::from_u128(3);
        let folder = Entry {
            node: Uuid::from_u128(5),
            parent: root,
            name: "d".into(),
            is_file: false,
            ..entry()
        };
        let file = Entry {
            parent: folder.node,
            ..entry()
        };
        state.stamp(pair, &folder).unwrap();
        state.stamp(pair, &file).unwrap();
        let (changed, gone, two) = state.changes_since(pair, 0).unwrap();
        assert_eq!((changed.len(), gone.len(), two), (2, 0, 2));
        let renamed = Entry {
            name: "b.txt".into(),
            ..file.clone()
        };
        state.stamp(pair, &renamed).unwrap();
        let (changed, _, three) = state.changes_since(pair, two).unwrap();
        assert_eq!(
            changed.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["b.txt"]
        );
        state.stamp_gone(pair, folder.node).unwrap();
        let (changed, mut gone, four) = state.changes_since(pair, three).unwrap();
        gone.sort();
        assert_eq!(
            (changed.len(), gone, four),
            (0, vec![file.node, folder.node], 4)
        );
        assert!(state.entries(pair).unwrap().is_empty());
        assert!(state.changes_since(pair, four).unwrap().1.is_empty());
        assert_eq!(
            state.changes_since(pair, three).unwrap().1.len(),
            2,
            "asked again"
        );
    }

    /// A setting missing is its default, and so is a value that isn't one;
    /// a key nobody reads changes nothing.
    #[test]
    fn a_setting_falls_back_to_its_default() {
        let state = State::memory().unwrap();
        assert_eq!(state.drive_limit().unwrap(), None);
        let put = |key: &str, value: &str| {
            state
                .db()
                .execute(
                    "INSERT OR REPLACE INTO setting (key, value) VALUES (?1, ?2)",
                    [key, value],
                )
                .unwrap();
        };
        put("updates", "beta");
        assert_eq!(state.drive_limit().unwrap(), None);
        for invalid in ["20GB", "-1", ""] {
            put("drive_limit", invalid);
            assert_eq!(state.drive_limit().unwrap(), None, "{invalid:?}");
        }
        state.set_drive_limit(5_000_000_000).unwrap();
        state.set_drive_limit(20_000_000_000).unwrap();
        assert_eq!(state.drive_limit().unwrap(), Some(20_000_000_000));
    }

    /// A round still running when the base is forgotten (`locate`) doesn't
    /// write it back: the next round merges, it doesn't compare.
    #[test]
    fn a_forgotten_base_stays_forgotten() {
        let state = State::memory().unwrap();
        let pair = pair(&state);
        let e = entry();
        state
            .record_remote(pair, &[], Some(theca_common::api::Position::Seq(1)))
            .unwrap();
        state.synced(pair, &e, None, &[]).unwrap();
        assert_eq!(state.entries(pair).unwrap().len(), 1);
        state.forget_base(pair).unwrap();
        state.synced(pair, &e, None, &[]).unwrap();
        assert!(state.entries(pair).unwrap().is_empty());
    }

    fn state_of(name: &str) -> RemoteState {
        RemoteState {
            parent: Uuid::from_u128(3),
            name: name.into(),
            is_file: true,
            version: Some(Uuid::from_u128(5)),
            size: 1,
            modified: None,
        }
    }

    /// C13: the feed never gives a position back to a pair whose base was
    /// just forgotten: its resync would never come.
    #[test]
    fn the_feed_never_follows_a_forgotten_base() {
        let state = State::memory().unwrap();
        let pair = pair(&state);
        state
            .record_remote(pair, &[], Some(theca_common::api::Position::Seq(1)))
            .unwrap();
        state.forget_base(pair).unwrap();
        let x = Uuid::from_u128(8);
        state
            .record_feed(
                pair,
                &[(x, Some(state_of("x")))],
                Some(theca_common::api::Position::Seq(5)),
            )
            .unwrap();
        assert_eq!(state.pair(pair).unwrap().unwrap().position, None);
        assert!(state.remote(pair).unwrap().is_empty());
    }

    /// C15: a listing (a resync) is the whole remote truth: what the feed
    /// left pending before it no longer applies.
    #[test]
    fn a_listing_replaces_what_was_pending() {
        let state = State::memory().unwrap();
        let pair = pair(&state);
        let (x, y) = (Uuid::from_u128(8), Uuid::from_u128(9));
        state
            .record_remote(
                pair,
                &[(x, Some(state_of("x")))],
                Some(theca_common::api::Position::Seq(1)),
            )
            .unwrap();
        state
            .record_remote(
                pair,
                &[(y, Some(state_of("y")))],
                Some(theca_common::api::Position::Seq(2)),
            )
            .unwrap();
        let pending: Vec<Uuid> = state
            .remote(pair)
            .unwrap()
            .iter()
            .map(|(n, _)| *n)
            .collect();
        assert_eq!(pending, [y]);
    }

    /// B15: forgetting the base is all or nothing: a failure part way (the
    /// database busy) never leaves the base gone and the position kept (no
    /// resync would follow).
    #[test]
    fn forgetting_the_base_is_all_or_nothing() {
        let state = State::memory().unwrap();
        let pair = pair(&state);
        state
            .record_remote(pair, &[], Some(theca_common::api::Position::Seq(1)))
            .unwrap();
        state.synced(pair, &entry(), None, &[]).unwrap();
        let x = Uuid::from_u128(8);
        state
            .record_feed(pair, &[(x, Some(state_of("x")))], None)
            .unwrap();
        // Its last statement fails: the ones before it are undone too.
        state
            .db()
            .execute_batch(
                "CREATE TRIGGER fail BEFORE DELETE ON remote
                 BEGIN SELECT RAISE(ABORT, 'busy'); END",
            )
            .unwrap();
        assert!(state.forget_base(pair).is_err());
        assert_eq!(state.entries(pair).unwrap().len(), 1);
        assert_eq!(state.pair(pair).unwrap().unwrap().position, Some(1));
    }

    /// A state opened with a channel publishes what the user would see change,
    /// as it's written; one without publishes nothing (and doesn't fail).
    #[test]
    fn changes_are_published() {
        let (tx, mut rx) = broadcast::channel(16);
        let state = State::memory().unwrap().with_updates(tx);
        let pair = pair(&state);
        assert!(matches!(rx.try_recv(), Ok(Update::Accounts)));
        assert!(matches!(rx.try_recv(), Ok(Update::Pairs)));
        state.set_activity(pair, "syncing", false).unwrap();
        assert!(
            matches!(rx.try_recv(), Ok(Update::Activity { activity, .. }) if activity == "syncing")
        );
        state
            .synced(pair, &entry(), Some("nothing.txt"), &[])
            .unwrap();
        assert!(rx.try_recv().is_err(), "nothing cleared: nothing published");
        state
            .notify(None, Some(pair), "error", Some("a.txt"), "bad", None)
            .unwrap();
        assert!(matches!(rx.try_recv(), Ok(Update::Notices)));
        state.synced(pair, &entry(), Some("a.txt"), &[]).unwrap();
        assert!(matches!(rx.try_recv(), Ok(Update::Notices)));
        State::memory()
            .unwrap()
            .set_activity(pair, "x", false)
            .unwrap();
    }
}
