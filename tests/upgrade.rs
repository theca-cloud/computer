//! The client state only moves forward (wiki: Sync client, The client
//! state): every
//! release's state, kept in `tests/upgrade/<version>/` with a manifest of what
//! it holds, is opened by this build, migrated, and read back; a fresh state's
//! schema is what the migrations build; a newer state is refused, untouched.
//! `fill` writes a release's state (`scripts/release.sh` runs it).

mod common;

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde_json::{Value, json};
use theca::keychain;
use theca::state::{
    Account, Entry, Mode, NewLibraries, Pair, Password, RemoteState, State, StateError, Stop,
};
use theca_common::api::Position;
use uuid::Uuid;

/// The kept release states, oldest first.
fn releases() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/upgrade");
    let mut releases: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("state.sqlite").exists())
        .collect();
    let version = |p: &PathBuf| {
        semver::Version::parse(&p.file_name().unwrap().to_string_lossy()).expect("a version")
    };
    releases.sort_by_key(version);
    releases
}

/// A release's state, copied into a temporary folder (the kept one is never
/// written), its keychain entries put back (they don't travel with the file).
fn copy_of(release: &Path) -> (tempfile::TempDir, PathBuf, Value) {
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(release.join("manifest.json")).unwrap()).unwrap();
    for a in manifest["accounts"].as_array().unwrap() {
        if let (true, Some(secret)) = (a["keychain"] == true, a["secret"].as_str()) {
            keychain::store(secret, a["password"].as_str().unwrap()).unwrap();
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    std::fs::copy(release.join("state.sqlite"), &path).unwrap();
    (dir, path, manifest)
}

fn version(path: &Path) -> i32 {
    Connection::open(path)
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

/// Every release's state is opened, migrated, and holds what its manifest
/// lists, in the schema a fresh state has (`SCHEMA` never changes without a
/// step building the same); neither it nor the copy kept before migrating
/// holds an app password the keychain now has. One test: the releases'
/// keychain entries are shared.
#[test]
fn every_release_state_reads_back() {
    common::keychain();
    let releases = releases();
    assert!(!releases.is_empty());
    let (_fresh, fresh) = fresh();
    let current = version(&fresh);
    let expected = schema(&fresh);
    for release in releases {
        let (dir, path, manifest) = copy_of(&release);
        let was = version(&path);
        let state = State::open(&path).unwrap_or_else(|e| panic!("{}: {e}", release.display()));
        assert_eq!(schema(&path), expected, "{}", release.display());
        let mut read = manifest_of(&state, &manifest["version"]);
        // An entry the migration made has a name of its own.
        let pairs_of = |account: &Value| -> Vec<Value> {
            manifest["pairs"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|p| p["account"] == account["uuid"])
                .map(|p| p["library"].clone())
                .collect()
        };
        for (a, expected) in read["accounts"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .zip(manifest["accounts"].as_array().unwrap())
        {
            if expected["secret"].is_null() {
                a["secret"] = Value::Null;
            }
            // Before version 4: a limit auto-synced (`sync` with it), none
            // listed (`silent`, 500 MB); `seen` holds the declined libraries
            // and the pairs'. Then read back in the release's terms.
            if expected.get("new_libraries").is_none() {
                let limit = &expected["auto_sync_limit"];
                let (new, bytes) = match limit.as_i64() {
                    Some(l) => ("sync", l),
                    None => ("silent", 500 * 1024 * 1024),
                };
                assert_eq!(
                    (a["new_libraries"].as_str(), a["auto_sync_limit"].as_i64()),
                    (Some(new), Some(bytes)),
                    "{}",
                    release.display()
                );
                let mut seen: Vec<Value> = expected["declined"].as_array().unwrap().clone();
                seen.extend(pairs_of(expected));
                seen.sort_by_key(|v| v.to_string());
                seen.dedup();
                assert_eq!(a["seen"], Value::Array(seen), "{}", release.display());
                let a = a.as_object_mut().unwrap();
                a.remove("new_libraries");
                a.remove("seen");
                a.insert("auto_sync_limit".into(), limit.clone());
                a.insert("declined".into(), expected["declined"].clone());
            }
        }
        // A release before the drive (version 4) has sync pairs only.
        for (p, expected) in read["pairs"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .zip(manifest["pairs"].as_array().unwrap())
        {
            if expected.get("mode").is_none() {
                assert_eq!(p["mode"], "sync", "{}", release.display());
                p.as_object_mut().unwrap().remove("mode");
            }
        }
        assert_eq!(read, manifest, "{}", release.display());
        if was < current {
            let copy = dir.path().join(format!("state.sqlite.v{was}"));
            let copy = Connection::open(copy).unwrap();
            let kept: Vec<String> = copy
                .prepare("SELECT password FROM account WHERE password IS NOT NULL")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert!(
                !kept.is_empty() && kept.iter().all(String::is_empty),
                "{kept:?}"
            );
            drop(copy);
            let bytes = std::fs::read(dir.path().join(format!("state.sqlite.v{was}"))).unwrap();
            for a in manifest["accounts"].as_array().unwrap() {
                let password = a["password"].as_str().unwrap().as_bytes();
                assert!(
                    !bytes.windows(password.len()).any(|w| w == password),
                    "{} left in the copy",
                    a["password"]
                );
            }
        }
        let accounts = state.accounts().unwrap();
        drop(state);
        // Its file once closed (the WAL written back): no page keeps a
        // password the keychain now holds.
        let bytes = std::fs::read(&path).unwrap();
        for a in &accounts {
            if let Password::Keychain(secret) = &a.password {
                let password = keychain::read(secret).unwrap();
                assert!(
                    !bytes
                        .windows(password.len())
                        .any(|w| w == password.as_bytes()),
                    "{password} left in {}",
                    release.display()
                );
                keychain::delete(secret).unwrap();
            }
        }
    }
}

/// A state a newer version wrote is refused, and left as it was (not even
/// turned into a WAL one).
#[test]
fn a_newer_state_is_refused() {
    let (_dir, path) = fresh();
    let db = Connection::open(&path).unwrap();
    db.pragma_update(None, "user_version", version(&path) + 1)
        .unwrap();
    db.pragma_update(None, "journal_mode", "delete").unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    let e = State::open(&path).err().expect("refused");
    assert!(matches!(e, StateError::Newer(_)), "{e}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

fn fresh() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    State::open(&path).unwrap();
    (dir, path)
}

/// The schema as SQLite keeps it, per table and index, without what doesn't
/// change it: comments, spacing, quotes (a rebuilt table is renamed into
/// place, and SQLite quotes its name then).
fn schema(path: &Path) -> Vec<(String, String)> {
    let db = Connection::open(path).unwrap();
    let mut query = db
        .prepare("SELECT name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name")
        .unwrap();
    query
        .query_map([], |r| Ok((r.get(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .map(|row| {
            let (name, sql) = row.unwrap();
            let code: Vec<&str> = sql
                .lines()
                .map(|l| l.split_once("--").map_or(l, |(code, _)| code))
                .collect();
            let sql = code.join(" ").replace('"', "");
            (name, sql.split_whitespace().collect::<Vec<_>>().join(" "))
        })
        .collect()
}

/// What a state holds, in the manifest's terms, read through the state's own
/// code: where each account's password is, and what it is.
fn manifest_of(state: &State, version: &Value) -> Value {
    let accounts = state.accounts().unwrap();
    let pairs = state.pairs().unwrap();
    let uuid_of = |account: i64| accounts.iter().find(|a| a.id == account).map(|a| a.uuid);
    let account_json: Vec<Value> = accounts
        .iter()
        .map(|a| {
            let mut seen: Vec<_> = state.seen(a.id).unwrap().into_iter().collect();
            seen.sort();
            json!({
                "instance": a.instance,
                "uuid": a.uuid,
                "email": a.email,
                "device": a.device,
                "password": keychain::password(a).unwrap(),
                "keychain": matches!(a.password, Password::Keychain(_)),
                "secret": match &a.password {
                    Password::Keychain(secret) => Some(secret),
                    Password::InState(_) => None,
                },
                "default_folder": a.default_folder,
                "new_libraries": a.new_libraries,
                "auto_sync_limit": a.auto_sync_limit,
                "signed_out": a.signed_out,
                "seen": seen,
            })
        })
        .collect();
    let pair_json: Vec<Value> = pairs
        .iter()
        .map(|p| {
            let mut entries = state.entries(p.id).unwrap();
            entries.sort_by_key(|e| e.node.to_string());
            let mut remote = state.remote(p.id).unwrap();
            remote.sort_by_key(|(node, _)| node.to_string());
            let (stop, detail) = match &p.stop {
                None => (None, None),
                Some(Stop::Folder(why)) => (Some("folder"), Some(why.clone())),
                Some(Stop::LibraryDeleted) => (Some("library_deleted"), None),
                Some(Stop::AccessRemoved) => (Some("access_removed"), None),
            };
            json!({
                "account": uuid_of(p.account),
                "library": p.library,
                "root": p.root,
                "library_name": p.library_name,
                "path": p.path,
                "root_file_id": p.root_file_id,
                "position": p.position,
                "position_event": p.position_event,
                "paused": p.paused,
                "stop": stop,
                "stop_detail": detail,
                "writable": p.writable,
                "activity": p.activity,
                "error": p.error,
                "synced_at": p.synced_at.map(|t| t.timestamp_millis()),
                "mode": match p.mode {
                    Mode::Sync => "sync",
                    Mode::Drive => "drive",
                },
                "entries": entries.iter().map(|e| json!({
                    "node": e.node,
                    "parent": e.parent,
                    "name": e.name,
                    "is_file": e.is_file,
                    "version": e.version,
                    "size": e.size,
                    "mtime": e.mtime,
                    "file_id": e.file_id,
                })).collect::<Vec<_>>(),
                "remote": remote.iter().map(|(node, s)| json!({
                    "node": node,
                    "state": s.as_ref().map(|s| json!({
                        "parent": s.parent,
                        "name": s.name,
                        "is_file": s.is_file,
                        "version": s.version,
                        "size": s.size,
                        "modified": s.modified.and_then(|m| m.timestamp_nanos_opt()),
                    })),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut notices = state.notices().unwrap();
    notices.sort_by_key(|n| n.id);
    let notice_json: Vec<Value> = notices
        .iter()
        .map(|n| {
            json!({
                "account": n.account.and_then(uuid_of),
                "library": n.pair.and_then(|id| pairs.iter().find(|p| p.id == id)).map(|p| p.library),
                "at": n.at.timestamp_millis(),
                "level": n.level,
                "path": n.path,
                "message": n.message,
                "undo": n.undo,
            })
        })
        .collect();
    json!({
        "version": version,
        "accounts": account_json,
        "pairs": pair_json,
        "notices": notice_json,
    })
}

fn id(n: u128) -> Uuid {
    Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0000 | n)
}

/// A release's state, through this build's code, and its manifest:
/// `THECA_FILL_OUT=tests/upgrade/<version> cargo test --test upgrade fill --
/// --ignored` (`scripts/release.sh` does). The same content as 0.2.0's fill
/// (`tests/upgrade/0.2.0/fill.rs`), plus what each version adds.
#[test]
#[ignore]
fn fill() {
    common::keychain();
    let out = PathBuf::from(std::env::var("THECA_FILL_OUT").expect("THECA_FILL_OUT"));
    let version = out.file_name().unwrap().to_string_lossy().into_owned();
    semver::Version::parse(&version).expect("the folder is named after the version");
    std::fs::create_dir_all(&out).unwrap();
    let path = out.join("state.sqlite");
    let _ = std::fs::remove_file(&path);
    let state = State::open(&path).unwrap();

    let account = |instance: &str, uuid, email: &str, password, new_libraries| Account {
        id: 0,
        instance: instance.into(),
        uuid,
        email: email.into(),
        device: "laptop".into(),
        password,
        default_folder: "/home/user/theca".into(),
        new_libraries,
        auto_sync_limit: 500 * 1024 * 1024,
        signed_out: false,
    };
    // One in the keychain, one kept in the state.
    let secret = keychain::new_secret();
    keychain::store(&secret, "app-password-a").unwrap();
    let a = state
        .save_account(&account(
            "https://cloud.example.com",
            id(1),
            "ada@example.com",
            Password::Keychain(secret.clone()),
            NewLibraries::Sync,
        ))
        .unwrap();
    let b = state
        .save_account(&account(
            "https://files.example.org",
            id(2),
            "bob@example.org",
            Password::InState("app-password-b".into()),
            NewLibraries::Silent,
        ))
        .unwrap();
    state.set_signed_out(b).unwrap();

    let pair = |account, library, root, name: &str, writable| Pair {
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
        stop: None,
        writable,
        activity: "waiting".into(),
        error: None,
        synced_at: None,
        mode: Mode::Sync,
    };
    // Synced, with a base and pending remote states.
    let docs = state
        .add_pair(&pair(a, id(10), id(11), "Documents", true))
        .unwrap();
    state
        .set_pair_path(
            docs,
            Path::new("/home/user/theca/Documents"),
            Some("16777232:4242"),
        )
        .unwrap();
    let (folder, report, notes, gone) = (id(12), id(13), id(14), id(15));
    let position = |seq, event| {
        Some(Position::Event {
            seq,
            event: Some(event),
        })
    };
    state
        .record_remote(docs, &[], position(42, id(16)))
        .unwrap();
    let entry =
        |node, parent, name: &str, version: Option<Uuid>, size, mtime, file_id: &str| Entry {
            node,
            parent,
            name: name.into(),
            is_file: version.is_some(),
            version,
            size,
            mtime,
            file_id: Some(file_id.into()),
        };
    for e in [
        entry(folder, id(11), "Reports", None, 0, 0, "4243"),
        entry(
            report,
            folder,
            "Q3 report.pdf",
            Some(id(17)),
            183_402,
            1_762_000_000_123_456_789,
            "4244",
        ),
        entry(
            notes,
            id(11),
            "Notes ΟΔΟΣ.txt",
            Some(id(18)),
            12,
            1_762_000_100_000_000_000,
            "4245",
        ),
    ] {
        state.synced(docs, &e, None, &[]).unwrap();
    }
    let changed = RemoteState {
        parent: folder,
        name: "Q3 report.pdf".into(),
        is_file: true,
        version: Some(id(19)),
        size: 190_000,
        modified: Some(chrono::DateTime::from_timestamp_nanos(
            1_762_100_000_000_000_000,
        )),
    };
    state
        .record_feed(
            docs,
            &[(report, Some(changed)), (gone, None)],
            position(44, id(20)),
        )
        .unwrap();
    state.set_activity(docs, "up to date", true).unwrap();
    // Paused, and stopped for its folder.
    let photos = state
        .add_pair(&pair(a, id(30), id(31), "Photos", true))
        .unwrap();
    state.set_paused(photos, true).unwrap();
    let missing = Stop::Folder("the folder is missing".into());
    state.set_stopped(photos, Some(&missing)).unwrap();
    state
        .set_activity(photos, "stopped: the folder is missing", false)
        .unwrap();
    // Stopped for its library.
    let archive = state
        .add_pair(&pair(a, id(40), id(41), "Archive", true))
        .unwrap();
    state
        .set_stopped(archive, Some(&Stop::LibraryDeleted))
        .unwrap();
    state
        .set_activity(archive, "stopped: library deleted", false)
        .unwrap();
    // Its account's engine stopped on an error.
    let music = state
        .add_pair(&pair(a, id(50), id(51), "Music", true))
        .unwrap();
    state
        .set_error(music, "the server answered 502 Bad Gateway")
        .unwrap();
    // A drive.
    state
        .add_pair(&Pair {
            mode: Mode::Drive,
            ..pair(a, id(90), id(91), "Projects", true)
        })
        .unwrap();
    // Read-only, its access removed, on the signed-out account.
    let shared = state
        .add_pair(&pair(b, id(60), id(61), "Shared", false))
        .unwrap();
    state
        .set_stopped(shared, Some(&Stop::AccessRemoved))
        .unwrap();
    state.set_activity(shared, "signed out", false).unwrap();
    // Declined: a pair removed.
    let old = state
        .add_pair(&pair(a, id(70), id(71), "Old", true))
        .unwrap();
    state
        .remove_pair(&state.pair(old).unwrap().unwrap())
        .unwrap();

    let undo = format!("{} {}", id(80), id(81));
    state
        .notify(
            Some(a),
            Some(docs),
            "info",
            Some("Reports/old.txt"),
            "moved to the trash",
            Some(&undo),
        )
        .unwrap();
    state
        .notify(
            Some(a),
            Some(photos),
            "error",
            Some("/home/user/theca/Photos"),
            "stopped: the folder is missing (the local files stay)",
            None,
        )
        .unwrap();
    state
        .notify(Some(b), None, "error", None, "This device was signed out of https://files.example.org: sign in again, or remove the account", None)
        .unwrap();

    let manifest = manifest_of(&state, &json!(version));
    drop(state);
    // Into one file: what a WAL holds isn't committed with it.
    Connection::open(&path)
        .unwrap()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    std::fs::write(
        out.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap() + "\n",
    )
    .unwrap();
    keychain::delete(&secret).unwrap();
}
