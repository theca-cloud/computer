//! Version 5, the one drive entry (sprint "One Theca entry for drives"), from
//! 1.3.x's version 4: every pair gets a UUIDv7 of its own, what leaves the
//! state (macOS's item identifiers: a pair's integer ID is reused after a
//! removal, and macOS keeps identifiers in its own database). The table is
//! rebuilt, as a column added by ALTER TABLE lands after the constraints, which
//! a fresh state's schema can't spell the same.

use rusqlite::Transaction;
use uuid::Uuid;

use crate::state::Result;

pub fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        r#"
CREATE TABLE pair_v5 (
    id INTEGER PRIMARY KEY,
    -- What leaves the state (macOS's item identifiers): a UUIDv7.
    uuid TEXT NOT NULL UNIQUE,
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
"#,
    )?;
    let ids: Vec<i64> = tx
        .prepare("SELECT id FROM pair ORDER BY id")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in ids {
        tx.execute(
            "INSERT INTO pair_v5 (id, uuid, account, library, root, library_name, path,
                                  root_file_id, position, position_event, paused, stop,
                                  stop_detail, writable, activity, error, synced_at, mode, seq)
             SELECT id, ?2, account, library, root, library_name, path, root_file_id, position,
                    position_event, paused, stop, stop_detail, writable, activity, error,
                    synced_at, mode, seq
             FROM pair WHERE id = ?1",
            rusqlite::params![id, Uuid::now_v7().to_string()],
        )?;
    }
    tx.execute_batch(
        r#"
DROP TABLE pair;
ALTER TABLE pair_v5 RENAME TO pair;
"#,
    )?;
    Ok(())
}
