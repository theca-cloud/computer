//! Version 6, notices that end (sprint "A drive that browses fast, in a window
//! that reads well"), from 1.4.0's version 5: a notice says whether it is an
//! account's feed error, which the feed clears itself once it answers again.
//! Every notice there is stays as it is (`feed` = 0: said until dismissed).
//! The table is rebuilt, as a column added by ALTER TABLE is stored after the
//! last one with spacing a fresh state's schema can't spell the same.

use rusqlite::Transaction;

use crate::state::Result;

pub fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        r#"
CREATE TABLE notice_v6 (
    id INTEGER PRIMARY KEY,
    account INTEGER REFERENCES account ON DELETE CASCADE,
    pair INTEGER REFERENCES pair ON DELETE CASCADE,
    at INTEGER NOT NULL,
    -- error (needs attention) or info.
    level TEXT NOT NULL,
    path TEXT,
    message TEXT NOT NULL,
    -- What `undo` restores: node UUIDs, space-separated.
    undo TEXT,
    -- An account's feed error (or its engine's, tried again): cleared by the
    -- feed's first message after it.
    feed INTEGER NOT NULL DEFAULT 0
);
INSERT INTO notice_v6 (id, account, pair, at, level, path, message, undo)
    SELECT id, account, pair, at, level, path, message, undo FROM notice;
DROP TABLE notice;
ALTER TABLE notice_v6 RENAME TO notice;
"#,
    )?;
    Ok(())
}
