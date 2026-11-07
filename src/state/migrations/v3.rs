//! Version 3, the client's 1.0 (sprint 13), from 0.2.0's version 2: what the
//! code reacts to stored as codes (a pair's stop, its engine's error), never as
//! the English text shown; app passwords moved into the OS keychain where
//! there is one. Released: never edited.

use rusqlite::{Transaction, params};

use crate::keychain;
use crate::state::Result;

pub fn migrate(tx: &Transaction<'_>) -> Result<()> {
    // SQLite can't drop a column's NOT NULL, nor a column: both tables are
    // rebuilt, their rows (and IDs) copied.
    tx.execute_batch(
        r#"
CREATE TABLE account_v3 (
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
INSERT INTO account_v3 (id, instance, uuid, email, device, password, secret, default_folder,
                        auto_sync_limit, signed_out)
    SELECT id, instance, uuid, email, device, password, NULL, default_folder,
           auto_sync_limit, signed_out
    FROM account;
DROP TABLE account;
ALTER TABLE account_v3 RENAME TO account;

CREATE TABLE pair_v3 (
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
    UNIQUE (account, library)
);
-- Version 2 stored a stop as its text: "library deleted" and "access removed"
-- were the library's, any other text the folder's. An engine's error was the
-- activity "error: <why>".
INSERT INTO pair_v3 (id, account, library, root, library_name, path, root_file_id, position,
                     position_event, paused, stop, stop_detail, writable, activity, error,
                     synced_at)
    SELECT id, account, library, root, library_name, path, root_file_id, position,
           position_event, paused,
           CASE
               WHEN stopped IS NULL THEN NULL
               WHEN stopped = 'library deleted' THEN 'library_deleted'
               WHEN stopped = 'access removed' THEN 'access_removed'
               ELSE 'folder'
           END,
           CASE WHEN stopped IN ('library deleted', 'access removed') THEN NULL ELSE stopped END,
           writable,
           CASE WHEN substr(activity, 1, 7) = 'error: ' THEN 'waiting' ELSE activity END,
           CASE WHEN substr(activity, 1, 7) = 'error: ' THEN substr(activity, 8) END,
           synced_at
    FROM pair;
DROP TABLE pair;
ALTER TABLE pair_v3 RENAME TO pair;
"#,
    )?;
    // Last, after everything SQL: an entry written can't be rolled back. A
    // crash before the commit leaves entries nobody names, and the next open
    // writes new ones.
    let passwords: Vec<(i64, String)> = tx
        .prepare("SELECT id, password FROM account")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (account, password) in passwords {
        let secret = keychain::new_secret();
        match keychain::store(&secret, &password) {
            Ok(()) => {
                tx.execute(
                    "UPDATE account SET password = NULL, secret = ?2 WHERE id = ?1",
                    params![account, secret],
                )?;
            }
            // No keychain (a headless server): kept here, said as a login
            // says it.
            Err(e) => {
                tracing::warn!(account, "the app password stays in the state: {e}");
                tx.execute(
                    "INSERT INTO notice (account, at, level, message) VALUES (?1, ?2, 'info', ?3)",
                    params![
                        account,
                        chrono::Utc::now().timestamp_millis(),
                        "the app password is kept in this app's state file: no keychain could be reached"
                    ],
                )?;
            }
        }
    }
    Ok(())
}
