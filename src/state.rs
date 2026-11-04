//! The client's state: one SQLite file holding everything it keeps (accounts,
//! sync pairs, what was last in sync, notices). No config file: settings
//! change through the CLI or the GUI (docs/decisions.md, Sync).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use tokio::sync::broadcast;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

/// The schema, rewritten in place until the client's 1.0; then forward-only,
/// one step per `user_version`.
const SCHEMA: &str = r#"
-- An account on an instance, with this device's app password.
CREATE TABLE account (
    id INTEGER PRIMARY KEY,
    instance TEXT NOT NULL,
    uuid TEXT NOT NULL,
    email TEXT NOT NULL,
    device TEXT NOT NULL,
    password TEXT NOT NULL,
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
    paused INTEGER NOT NULL DEFAULT 0,
    -- Why it stopped (missing folder, library deleted, access removed...).
    stopped TEXT,
    writable INTEGER NOT NULL DEFAULT 1,
    -- What it's doing, for status: written by the engine.
    activity TEXT NOT NULL DEFAULT 'waiting',
    synced_at INTEGER,
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
    PRIMARY KEY (pair, node)
);
CREATE INDEX entry_parent ON entry (pair, parent);

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
"#;

const VERSION: i32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("the state database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("the state database is version {0}, this client knows {VERSION}: update theca")]
    Newer(i32),
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
    /// Opens (creating) the database at `path`; only the user can read it (it
    /// holds app passwords).
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| {
                rusqlite::Error::InvalidPath(PathBuf::from(format!("{}: {e}", dir.display())))
            })?;
        }
        let db = Connection::open(path)?;
        restrict(path);
        db.pragma_update(None, "journal_mode", "wal")?;
        db.pragma_update(None, "foreign_keys", true)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i32 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => {
                db.execute_batch(SCHEMA)?;
                db.pragma_update(None, "user_version", VERSION)?;
            }
            VERSION => {}
            newer => return Err(StateError::Newer(newer)),
        }
        Ok(Self {
            connection: Mutex::new(db),
            updates: None,
        })
    }

    /// An in-memory database (tests).
    pub fn memory() -> Result<Self> {
        let db = Connection::open_in_memory()?;
        db.pragma_update(None, "foreign_keys", true)?;
        db.execute_batch(SCHEMA)?;
        Ok(Self {
            connection: Mutex::new(db),
            updates: None,
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
    pub password: String,
    pub default_folder: PathBuf,
    pub auto_sync_limit: Option<i64>,
    pub signed_out: bool,
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
    pub paused: bool,
    pub stopped: Option<String>,
    pub writable: bool,
    /// What the engine is doing with it, and when it last finished a round.
    pub activity: String,
    pub synced_at: Option<DateTime<Utc>>,
}

/// Why a pair stops for its library (it resumes by itself when the library
/// comes back); every other stop is about its folder.
pub const LIBRARY_DELETED: &str = "library deleted";
pub const ACCESS_REMOVED: &str = "access removed";

impl Pair {
    /// Stopped because of its local folder (missing, in the trash,
    /// unreadable): the user locates it, or tries again.
    pub fn stopped_for_its_folder(&self) -> bool {
        self.stopped
            .as_deref()
            .is_some_and(|why| why != LIBRARY_DELETED && why != ACCESS_REMOVED)
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
            "SELECT id, instance, uuid, email, device, password, default_folder, auto_sync_limit, signed_out
             FROM account ORDER BY id",
        )?;
        let rows = query.query_map([], |r| {
            Ok(Account {
                id: r.get(0)?,
                instance: r.get(1)?,
                uuid: uuid(r.get(2)?),
                email: r.get(3)?,
                device: r.get(4)?,
                password: r.get(5)?,
                default_folder: PathBuf::from(r.get::<_, String>(6)?),
                auto_sync_limit: r.get(7)?,
                signed_out: r.get(8)?,
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
        self.db().execute(
            "INSERT INTO account (instance, uuid, email, device, password, default_folder, auto_sync_limit)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (instance, uuid) DO UPDATE SET
                email = excluded.email, device = excluded.device, password = excluded.password,
                signed_out = 0",
            params![
                a.instance,
                a.uuid.to_string(),
                a.email,
                a.device,
                a.password,
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
                    paused, stopped, writable, activity, synced_at
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
                stopped: r.get(9)?,
                writable: r.get(10)?,
                activity: r.get(11)?,
                synced_at: r
                    .get::<_, Option<i64>>(12)?
                    .and_then(DateTime::from_timestamp_millis),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn pair(&self, id: i64) -> Result<Option<Pair>> {
        Ok(self.pairs()?.into_iter().find(|p| p.id == id))
    }

    pub fn add_pair(&self, p: &Pair) -> Result<i64> {
        self.db().execute(
            "INSERT INTO pair (account, library, root, library_name, path, writable)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                p.account,
                p.library.to_string(),
                p.root.to_string(),
                p.library_name,
                p.path.to_string_lossy(),
                p.writable
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

    pub fn set_stopped(&self, pair: i64, why: Option<&str>) -> Result<()> {
        self.db().execute(
            "UPDATE pair SET stopped = ?2 WHERE id = ?1",
            params![pair, why],
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

    pub fn set_activity(&self, pair: i64, activity: &str, synced: bool) -> Result<()> {
        self.db().execute(
            "UPDATE pair SET activity = ?2,
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
        let db = self.db();
        db.execute("DELETE FROM entry WHERE pair = ?1", [pair])?;
        db.execute("DELETE FROM remote WHERE pair = ?1", [pair])?;
        db.execute("UPDATE pair SET position = NULL WHERE id = ?1", [pair])?;
        drop(db);
        self.publish(Update::Pairs);
        Ok(())
    }

    pub fn reset_position(&self, pair: i64) -> Result<()> {
        self.db()
            .execute("UPDATE pair SET position = NULL WHERE id = ?1", [pair])?;
        Ok(())
    }

    pub fn entries(&self, pair: i64) -> Result<Vec<Entry>> {
        let db = self.db();
        let mut query = db.prepare(
            "SELECT node, parent, name, is_file, version, size, mtime, file_id
             FROM entry WHERE pair = ?1",
        )?;
        let rows = query.query_map([pair], |r| {
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
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
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
            "INSERT OR REPLACE INTO entry (pair, node, parent, name, is_file, version, size, mtime, file_id)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9
             WHERE EXISTS (SELECT 1 FROM pair WHERE id = ?1 AND position IS NOT NULL)",
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

    /// Records remote states and the new position, in one transaction: a
    /// position is never ahead of what was recorded.
    pub fn record_remote(
        &self,
        pair: i64,
        states: &[(Uuid, Option<RemoteState>)],
        position: Option<i64>,
    ) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        for (node, state) in states {
            put_remote(&tx, pair, *node, state.as_ref())?;
        }
        if let Some(position) = position {
            tx.execute(
                "UPDATE pair SET position = ?2 WHERE id = ?1",
                params![pair, position],
            )?;
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
            "SELECT id, pair, at, level, path, message, undo FROM notice ORDER BY at DESC, id DESC LIMIT 500",
        )?;
        let rows = query.query_map([], |r| {
            Ok(Notice {
                id: r.get(0)?,
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

    fn pair(state: &State) -> i64 {
        let account = state
            .save_account(&Account {
                id: 0,
                instance: "https://x".into(),
                uuid: Uuid::from_u128(1),
                email: "a@x".into(),
                device: "d".into(),
                password: "p".into(),
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
                paused: false,
                stopped: None,
                writable: true,
                activity: String::new(),
                synced_at: None,
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

    /// A round still running when the base is forgotten (`locate`) doesn't
    /// write it back: the next round merges, it doesn't compare.
    #[test]
    fn a_forgotten_base_stays_forgotten() {
        let state = State::memory().unwrap();
        let pair = pair(&state);
        let e = entry();
        state.record_remote(pair, &[], Some(1)).unwrap();
        state.synced(pair, &e, None, &[]).unwrap();
        assert_eq!(state.entries(pair).unwrap().len(), 1);
        state.forget_base(pair).unwrap();
        state.synced(pair, &e, None, &[]).unwrap();
        assert!(state.entries(pair).unwrap().is_empty());
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
