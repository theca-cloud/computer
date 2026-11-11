//! Version 4, the drive (sprint 16) and choosing libraries (sprint 18), from
//! 1.0.0's and 1.1.0's version 3: a pair is a sync pair or a drive; a
//! drive's entries say whether they are kept, when last used, and when they
//! last changed (macOS's anchors); settings for the whole computer; what an
//! account does with a library new to it, and the libraries it knows
//! (`seen`, replacing `declined`). Every existing pair stays a sync pair; an
//! account auto-syncing goes on (`sync`), one that didn't lists (`silent`).
//! Released with 1.2.0: never edited after.

use rusqlite::Transaction;

use crate::state::Result;

pub fn migrate(tx: &Transaction<'_>) -> Result<()> {
    // Columns added by ALTER TABLE land after a table's constraints, which a
    // fresh state's schema can't spell the same: both tables are rebuilt, their
    // rows (and IDs) copied.
    tx.execute_batch(
        r#"
CREATE TABLE pair_v4 (
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
INSERT INTO pair_v4 (id, account, library, root, library_name, path, root_file_id, position,
                     position_event, paused, stop, stop_detail, writable, activity, error,
                     synced_at)
    SELECT id, account, library, root, library_name, path, root_file_id, position,
           position_event, paused, stop, stop_detail, writable, activity, error, synced_at
    FROM pair;
DROP TABLE pair;
ALTER TABLE pair_v4 RENAME TO pair;

CREATE TABLE entry_v4 (
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
INSERT INTO entry_v4 (pair, node, parent, name, is_file, version, size, mtime, file_id)
    SELECT pair, node, parent, name, is_file, version, size, mtime, file_id FROM entry;
DROP TABLE entry;
ALTER TABLE entry_v4 RENAME TO entry;
CREATE INDEX entry_parent ON entry (pair, parent);

-- Drives on macOS: entries removed, with the pair's counter then, until the
-- system has listed the changes past it.
CREATE TABLE entry_gone (
    pair INTEGER NOT NULL REFERENCES pair ON DELETE CASCADE,
    node TEXT NOT NULL,
    seq INTEGER NOT NULL,
    PRIMARY KEY (pair, node)
);

-- Settings for the whole computer (the drive limit...): a missing key is its
-- default.
CREATE TABLE setting (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE account_v4 (
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
    -- What a library new to the account does (one not in `seen`): synced (up
    -- to auto_sync_limit), a drive, listed and notified, or listed only.
    new_libraries TEXT NOT NULL DEFAULT 'notify'
        CHECK (new_libraries IN ('sync', 'drive', 'notify', 'silent')),
    -- New libraries up to this size (bytes) sync (new_libraries 'sync'); 0: no
    -- limit.
    auto_sync_limit INTEGER NOT NULL DEFAULT 524288000,
    -- The server refused the app password (revoked): nothing runs until a new login.
    signed_out INTEGER NOT NULL DEFAULT 0,
    UNIQUE (instance, uuid)
);
-- A limit set auto-synced: `sync` with it; none listed them: `silent`.
INSERT INTO account_v4 (id, instance, uuid, email, device, password, secret, default_folder,
                        new_libraries, auto_sync_limit, signed_out)
    SELECT id, instance, uuid, email, device, password, secret, default_folder,
           CASE WHEN auto_sync_limit IS NULL THEN 'silent' ELSE 'sync' END,
           coalesce(auto_sync_limit, 524288000), signed_out
    FROM account;
DROP TABLE account;
ALTER TABLE account_v4 RENAME TO account;

-- The libraries an account knows: one not here is new, and gets the
-- account's new_libraries once (then it's here). One removed stays: never
-- added by itself again.
CREATE TABLE seen (
    account INTEGER NOT NULL REFERENCES account ON DELETE CASCADE,
    library TEXT NOT NULL,
    PRIMARY KEY (account, library)
);
INSERT INTO seen (account, library)
    SELECT account, library FROM pair
    UNION SELECT account, library FROM declined;
DROP TABLE declined;
"#,
    )?;
    Ok(())
}
