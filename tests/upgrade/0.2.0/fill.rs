//! 0.2.0's fill, adapted from 1.0's (tests/upgrade.rs): a client state written
//! through 0.2.0's own code, and the manifest 1.0 reads it back against, in
//! 1.0's terms (a stop's kind, the engine's error apart, where the password
//! is once migrated). Run once, by hand: THECA_FILL_OUT=<dir> cargo test
//! --test fill -- --ignored.

use std::path::PathBuf;

use rusqlite::Connection;
use serde_json::{Value, json};
use theca::state::{Account, Entry, Pair, RemoteState, State};
use theca_common::api::Position;
use uuid::Uuid;

fn id(n: u128) -> Uuid {
    Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0000 | n)
}

fn account(instance: &str, uuid: Uuid, email: &str, password: &str, limit: Option<i64>) -> Account {
    Account {
        id: 0,
        instance: instance.into(),
        uuid,
        email: email.into(),
        device: "laptop".into(),
        password: password.into(),
        default_folder: "/home/user/theca".into(),
        auto_sync_limit: limit,
        signed_out: false,
    }
}

fn pair(account: i64, library: Uuid, root: Uuid, name: &str, writable: bool) -> Pair {
    Pair {
        id: 0,
        account,
        library,
        root,
        library_name: name.into(),
        path: PathBuf::from(format!("/home/user/theca/{name}")),
        root_file_id: None,
        position: None,
        position_event: None,
        paused: false,
        stopped: None,
        writable,
        activity: "waiting".into(),
        synced_at: None,
    }
}

#[test]
#[ignore]
fn fill() {
    let out = PathBuf::from(std::env::var("THECA_FILL_OUT").expect("THECA_FILL_OUT"));
    std::fs::create_dir_all(&out).unwrap();
    let path = out.join("state.sqlite");
    let _ = std::fs::remove_file(&path);
    let state = State::open(&path).unwrap();

    let a = state
        .save_account(&account("https://cloud.example.com", id(1), "ada@example.com", "app-password-a", Some(500 * 1024 * 1024)))
        .unwrap();
    let b = state
        .save_account(&account("https://files.example.org", id(2), "bob@example.org", "app-password-b", None))
        .unwrap();
    state.set_signed_out(b).unwrap();

    // Synced, with a base and pending remote states.
    let docs = state.add_pair(&pair(a, id(10), id(11), "Documents", true)).unwrap();
    state
        .set_pair_path(docs, &PathBuf::from("/home/user/theca/Documents"), Some("16777232:4242"))
        .unwrap();
    let (folder, report, notes, gone) = (id(12), id(13), id(14), id(15));
    state
        .record_remote(docs, &[], Some(Position::Event { seq: 42, event: Some(id(16)) }))
        .unwrap();
    for e in [
        Entry { node: folder, parent: id(11), name: "Reports".into(), is_file: false, version: None, size: 0, mtime: 0, file_id: Some("4243".into()) },
        Entry { node: report, parent: folder, name: "Q3 report.pdf".into(), is_file: true, version: Some(id(17)), size: 183_402, mtime: 1_762_000_000_123_456_789, file_id: Some("4244".into()) },
        Entry { node: notes, parent: id(11), name: "Notes ΟΔΟΣ.txt".into(), is_file: true, version: Some(id(18)), size: 12, mtime: 1_762_000_100_000_000_000, file_id: Some("4245".into()) },
    ] {
        state.synced(docs, &e, None, &[]).unwrap();
    }
    state
        .record_feed(
            docs,
            &[
                (report, Some(RemoteState { parent: folder, name: "Q3 report.pdf".into(), is_file: true, version: Some(id(19)), size: 190_000, modified: chrono::DateTime::from_timestamp_nanos(1_762_100_000_000_000_000).into() })),
                (gone, None),
            ],
            Some(Position::Event { seq: 44, event: Some(id(20)) }),
        )
        .unwrap();
    state.set_activity(docs, "up to date", true).unwrap();

    // Paused, and stopped for its folder.
    let photos = state.add_pair(&pair(a, id(30), id(31), "Photos", true)).unwrap();
    state.set_paused(photos, true).unwrap();
    state.set_stopped(photos, Some("the folder is missing")).unwrap();
    state.set_activity(photos, "stopped: the folder is missing", false).unwrap();
    // Stopped for its library.
    let archive = state.add_pair(&pair(a, id(40), id(41), "Archive", true)).unwrap();
    state.set_stopped(archive, Some(theca::state::LIBRARY_DELETED)).unwrap();
    state.set_activity(archive, "stopped: library deleted", false).unwrap();
    // Its account's engine stopped on an error.
    let music = state.add_pair(&pair(a, id(50), id(51), "Music", true)).unwrap();
    state.set_activity(music, "error: the server answered 502 Bad Gateway", false).unwrap();
    // Read-only, its access removed, on the signed-out account.
    let shared = state.add_pair(&pair(b, id(60), id(61), "Shared", false)).unwrap();
    state.set_stopped(shared, Some(theca::state::ACCESS_REMOVED)).unwrap();
    state.set_activity(shared, "signed out", false).unwrap();
    // Declined: a pair removed.
    let old = state.add_pair(&pair(a, id(70), id(71), "Old", true)).unwrap();
    state.remove_pair(&state.pair(old).unwrap().unwrap()).unwrap();

    state
        .notify(Some(a), Some(docs), "info", Some("Reports/old.txt"), "moved to the trash", Some(&format!("{} {}", id(80), id(81))))
        .unwrap();
    state
        .notify(Some(a), Some(photos), "error", Some("/home/user/theca/Photos"), "stopped: the folder is missing (the local files stay)", None)
        .unwrap();
    state
        .notify(Some(b), None, "error", None, "This device was signed out of https://files.example.org: sign in again, or remove the account", None)
        .unwrap();
    drop(state);

    // The manifest, from what 0.2.0 wrote, in 1.0's terms.
    let db = Connection::open(&path).unwrap();
    let accounts: Vec<Value> = rows(&db, "SELECT id, instance, uuid, email, device, password, default_folder, auto_sync_limit, signed_out FROM account ORDER BY id", |r| {
        let account: i64 = r.get(0)?;
        Ok(json!({
            "instance": r.get::<_, String>(1)?,
            "uuid": r.get::<_, String>(2)?,
            "email": r.get::<_, String>(3)?,
            "device": r.get::<_, String>(4)?,
            "password": r.get::<_, String>(5)?,
            // Moved into the keychain by the migration.
            "keychain": true,
            "secret": null,
            "default_folder": r.get::<_, String>(6)?,
            "auto_sync_limit": r.get::<_, Option<i64>>(7)?,
            "signed_out": r.get::<_, bool>(8)?,
            "declined": rows(&db, &format!("SELECT library FROM declined WHERE account = {account} ORDER BY library"), |r| Ok(json!(r.get::<_, String>(0)?))),
        }))
    });
    let pairs: Vec<Value> = rows(&db, "SELECT p.id, a.uuid, library, root, library_name, path, root_file_id, position, position_event, paused, stopped, writable, activity, synced_at FROM pair p JOIN account a ON a.id = p.account ORDER BY p.id", |r| {
        let pair: i64 = r.get(0)?;
        let stopped: Option<String> = r.get(10)?;
        let (stop, detail) = match stopped.as_deref() {
            None => (None, None),
            Some("library deleted") => (Some("library_deleted"), None),
            Some("access removed") => (Some("access_removed"), None),
            Some(why) => (Some("folder"), Some(why.to_owned())),
        };
        let activity: String = r.get(12)?;
        let (activity, error) = match activity.strip_prefix("error: ") {
            Some(e) => ("waiting".to_owned(), Some(e.to_owned())),
            None => (activity, None),
        };
        Ok(json!({
            "account": r.get::<_, String>(1)?,
            "library": r.get::<_, String>(2)?,
            "root": r.get::<_, String>(3)?,
            "library_name": r.get::<_, String>(4)?,
            "path": r.get::<_, String>(5)?,
            "root_file_id": r.get::<_, Option<String>>(6)?,
            "position": r.get::<_, Option<i64>>(7)?,
            "position_event": r.get::<_, Option<String>>(8)?,
            "paused": r.get::<_, bool>(9)?,
            "stop": stop,
            "stop_detail": detail,
            "writable": r.get::<_, bool>(11)?,
            "activity": activity,
            "error": error,
            "synced_at": r.get::<_, Option<i64>>(13)?,
            "entries": rows(&db, &format!("SELECT node, parent, name, is_file, version, size, mtime, file_id FROM entry WHERE pair = {pair} ORDER BY node"), |r| Ok(json!({
                "node": r.get::<_, String>(0)?,
                "parent": r.get::<_, String>(1)?,
                "name": r.get::<_, String>(2)?,
                "is_file": r.get::<_, bool>(3)?,
                "version": r.get::<_, Option<String>>(4)?,
                "size": r.get::<_, i64>(5)?,
                "mtime": r.get::<_, i64>(6)?,
                "file_id": r.get::<_, Option<String>>(7)?,
            }))),
            "remote": rows(&db, &format!("SELECT node, gone, parent, name, is_file, version, size, modified FROM remote WHERE pair = {pair} ORDER BY node"), |r| {
                let gone: bool = r.get(1)?;
                Ok(if gone {
                    json!({ "node": r.get::<_, String>(0)?, "state": null })
                } else {
                    json!({ "node": r.get::<_, String>(0)?, "state": {
                        "parent": r.get::<_, String>(2)?,
                        "name": r.get::<_, String>(3)?,
                        "is_file": r.get::<_, bool>(4)?,
                        "version": r.get::<_, Option<String>>(5)?,
                        "size": r.get::<_, i64>(6)?,
                        "modified": r.get::<_, Option<i64>>(7)?,
                    }})
                })
            }),
        }))
    });
    let notices: Vec<Value> = rows(&db, "SELECT a.uuid, p.library, n.at, n.level, n.path, n.message, n.undo FROM notice n LEFT JOIN account a ON a.id = n.account LEFT JOIN pair p ON p.id = n.pair ORDER BY n.id", |r| {
        Ok(json!({
            "account": r.get::<_, Option<String>>(0)?,
            "library": r.get::<_, Option<String>>(1)?,
            "at": r.get::<_, i64>(2)?,
            "level": r.get::<_, String>(3)?,
            "path": r.get::<_, Option<String>>(4)?,
            "message": r.get::<_, String>(5)?,
            "undo": r.get::<_, Option<String>>(6)?,
        }))
    });
    let manifest = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "accounts": accounts,
        "pairs": pairs,
        "notices": notices,
    });
    std::fs::write(out.join("manifest.json"), serde_json::to_string_pretty(&manifest).unwrap() + "\n").unwrap();
}

fn rows(db: &Connection, sql: &str, f: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<Value>) -> Vec<Value> {
    let mut query = db.prepare(sql).unwrap();
    query.query_map([], f).unwrap().collect::<rusqlite::Result<_>>().unwrap()
}
