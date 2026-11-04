//! Two devices and the server, converging after every step (the real server,
//! the real engines).

mod common;

use common::*;

#[tokio::test(flavor = "multi_thread")]
async fn two_devices_converge() {
    let w = world().await;
    let a = w.device("a").await;
    let b = w.device("b").await;
    w.converge(&[&a, &b]).await;

    // Created on a: on the server and on b.
    a.write("notes.txt", "one");
    a.write("d/e/deep.txt", "deep");
    w.converge(&[&a, &b]).await;

    // Modified on b.
    b.write("notes.txt", "two, longer");
    w.converge(&[&a, &b]).await;

    // Renamed and moved on a.
    std::fs::rename(a.path("notes.txt"), a.path("d/renamed.txt")).unwrap();
    w.converge(&[&a, &b]).await;

    // Deleted on b: in the trash, gone on a.
    std::fs::remove_dir_all(b.path("d/e")).unwrap();
    w.converge(&[&a, &b]).await;
}

/// Changed on both sides while one device was offline: the late upload forks
/// the version tree and becomes current; the other content is a version.
#[tokio::test(flavor = "multi_thread")]
async fn both_changed_forks_the_version_tree() {
    let w = world().await;
    let a = w.device("a").await;
    let mut b = w.device("b").await;
    a.write("notes.txt", "one");
    w.converge(&[&a, &b]).await;
    b.stop().await;
    a.write("notes.txt", "from a");
    w.converge(&[&a]).await;
    b.write("notes.txt", "from b, offline");
    b.restart();
    w.converge(&[&a, &b]).await;
    assert_eq!(
        std::fs::read_to_string(a.path("notes.txt")).unwrap(),
        "from b, offline"
    );
    let file = a
        .state()
        .entries(a.pair().id)
        .unwrap()
        .into_iter()
        .find(|e| e.name == "notes.txt")
        .unwrap();
    let versions = w.admin.versions(file.node).await.unwrap();
    assert_eq!(versions.len(), 3, "one, a's, and b's on one");
    assert!(
        b.notices()
            .iter()
            .any(|n| n.contains("also changed elsewhere")),
        "{:?}",
        b.notices()
    );
}

/// Deleted on one side, modified on the other: the modification wins, both
/// ways; a folder deleted while a file arrived in it comes back with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_modification_wins_over_a_delete() {
    let w = world().await;
    let a = w.device("a").await;
    let mut b = w.device("b").await;
    a.write("kept.txt", "one");
    a.write("back.txt", "one");
    a.write("d/old.txt", "old");
    w.converge(&[&a, &b]).await;

    b.stop().await;
    // Deleted on a, modified on b (offline).
    std::fs::remove_file(a.path("kept.txt")).unwrap();
    // Modified on a, deleted on b.
    a.write("back.txt", "modified on a");
    // A folder deleted on b while a file arrives in it on a.
    a.write("d/new.txt", "new");
    w.converge(&[&a]).await;
    b.write("kept.txt", "modified on b");
    std::fs::remove_file(b.path("back.txt")).unwrap();
    std::fs::remove_dir_all(b.path("d")).unwrap();
    b.restart();
    w.converge(&[&a, &b]).await;
    let remote = w.remote(&w.library).await;
    let names: Vec<&str> = remote.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        ["back.txt", "d", "d/new.txt", "kept.txt"],
        "d/old.txt trashed"
    );
    assert_eq!(remote["kept.txt"].as_deref(), Some(&b"modified on b"[..]));
    assert_eq!(remote["back.txt"].as_deref(), Some(&b"modified on a"[..]));
}

/// Deletes go to the trash, and Undo brings them back.
#[tokio::test(flavor = "multi_thread")]
async fn deletes_can_be_undone() {
    let w = world().await;
    let a = w.device("a").await;
    for i in 0..5 {
        a.write(&format!("f/{i}.txt"), "x");
    }
    w.converge(&[&a]).await;
    std::fs::remove_dir_all(a.path("f")).unwrap();
    w.converge(&[&a]).await;
    let trash = w.admin.library_trash(w.library.id).await.unwrap();
    assert_eq!(trash.len(), 1, "the folder, its files with it");
    let notice = a.state().latest_undo().unwrap().expect("an undo notice");
    assert!(
        notice.message.contains("moved to the trash"),
        "{}",
        notice.message
    );
    theca::ops::undo(&a.state(), notice.id).await.unwrap();
    w.converge(&[&a]).await;
    assert_eq!(common::local(&a.folder).len(), 6);
}

/// Names the server refuses stay local and are reported; normalization never
/// makes two names; a case-only rename is a rename.
#[tokio::test(flavor = "multi_thread")]
async fn names() {
    let w = world().await;
    let a = w.device("a").await;
    let b = w.device("b").await;
    // NFD on disk (as macOS may hold it): NFC on the server and on b.
    a.write("cafe\u{301}.txt", "nfd");
    a.write("Report.txt", "r");
    w.converge(&[&a, &b]).await;
    assert!(w.remote(&w.library).await.contains_key("café.txt"));
    std::fs::rename(a.path("Report.txt"), a.path("tmp.txt")).unwrap();
    std::fs::rename(a.path("tmp.txt"), a.path("REPORT.txt")).unwrap();
    w.converge(&[&a, &b]).await;

    // Refused names: reported, not uploaded, nothing else blocked. Windows
    // can't hold them (`:` names a stream of `bad`, the dot is dropped).
    if cfg!(windows) {
        return;
    }
    a.write("bad:name.txt", "x");
    a.write("trailing.", "x");
    a.write("fine.txt", "x");
    eventually("fine.txt uploaded", || async {
        w.remote(&w.library).await.contains_key("fine.txt")
    })
    .await;
    eventually("the refusals reported", || async {
        let n = a.notices();
        n.iter().any(|n| n.starts_with("bad:name.txt: not synced"))
            && n.iter().any(|n| n.starts_with("trailing.: not synced"))
    })
    .await;
    let remote = w.remote(&w.library).await;
    assert!(!remote.contains_key("bad:name.txt") && !remote.contains_key("trailing."));
}

/// A read-only library: changes come down, local ones are reported, and a
/// file changed on both sides keeps the local copy aside.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_only_library_only_comes_down() {
    let w = world().await;
    let writer = w.device("writer").await;
    let password = w
        .admin
        .create_app_password(&theca_common::api::CreateAppPassword {
            name: "reader".into(),
            organization: None,
            library: None,
            read_only: true,
        })
        .await
        .unwrap()
        .password;
    let mut reader = w.device_with("reader", &w.library, &password, true).await;
    writer.write("doc.txt", "one");
    w.converge(&[&writer, &reader]).await;

    reader.write("mine.txt", "local only");
    eventually("reported", || async {
        reader.notices().iter().any(|n| n.contains("read-only"))
    })
    .await;
    assert!(!w.remote(&w.library).await.contains_key("mine.txt"));

    reader.stop().await;
    reader.write("doc.txt", "reader's edit");
    writer.write("doc.txt", "writer's edit");
    w.converge(&[&writer]).await;
    reader.restart();
    eventually("the server's version down, the local one aside", || async {
        std::fs::read_to_string(reader.path("doc.txt"))
            .ok()
            .as_deref()
            == Some("writer's edit")
            && std::fs::read_to_string(reader.path("doc (local).txt"))
                .ok()
                .as_deref()
                == Some("reader's edit")
    })
    .await;
}

/// A lost state: the same folder added again (a merge) finds everything
/// identical: nothing new on the server.
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_state_resyncs_without_new_versions() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.write("x.txt", "x");
    a.write("d/y.txt", "y");
    w.converge(&[&a]).await;
    a.stop().await;
    let file = a
        .state()
        .entries(a.pair().id)
        .unwrap()
        .into_iter()
        .find(|e| e.name == "x.txt")
        .unwrap();
    // The state database lost: the pair's state forgotten.
    let state = a.state();
    let pair = a.pair();
    state.remove_pair(&pair).unwrap();
    let account = a.account.clone();
    let err = theca::ops::add(&state, &account, "Docs", Some(a.folder.clone()), false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("confirm the merge"), "{err}");
    theca::ops::add(&state, &account, "Docs", Some(a.folder.clone()), true)
        .await
        .unwrap();
    a.restart();
    w.converge(&[&a]).await;
    eventually("in sync again", || async {
        a.pair().activity == "up to date"
    })
    .await;
    assert_eq!(
        w.admin.versions(file.node).await.unwrap().len(),
        1,
        "identical: no new version"
    );
    // And it keeps syncing.
    a.write("z.txt", "z");
    w.converge(&[&a]).await;
}

/// The pair's folder renamed while the client runs: followed. Removed: the
/// pair stops, and nothing is deleted on the server, even when its files
/// went first.
#[tokio::test(flavor = "multi_thread")]
async fn the_folder_followed_or_stopped() {
    let w = world().await;
    let a = w.device("a").await;
    a.write("x.txt", "x");
    w.converge(&[&a]).await;
    let moved = a.folder.with_file_name("Docs renamed");
    std::fs::rename(&a.folder, &moved).unwrap();
    std::fs::write(moved.join("after.txt"), "after").unwrap();
    common::settle(&moved.join("after.txt"));
    let real = moved.canonicalize().unwrap();
    eventually("followed", || async {
        a.pair().path.canonicalize().ok().as_ref() == Some(&real)
    })
    .await;
    eventually("after.txt uploaded", || async {
        w.remote(&w.library).await.contains_key("after.txt")
    })
    .await;

    // As `rm -rf`: the files, then the folder; a round runs in between.
    std::fs::remove_file(moved.join("x.txt")).unwrap();
    std::fs::remove_file(moved.join("after.txt")).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    std::fs::remove_dir(&moved).unwrap();
    eventually("stopped", || async { a.pair().stopped.is_some() }).await;
    assert_eq!(
        w.remote(&w.library).await.len(),
        2,
        "nothing deleted on the server"
    );
}

/// A library trashed on the server stops its pair (the files stay); restored,
/// it resumes. A revoked app password signs the device out.
#[tokio::test(flavor = "multi_thread")]
async fn library_trashed_and_device_signed_out() {
    let w = world().await;
    let a = w.device("a").await;
    a.write("x.txt", "x");
    w.converge(&[&a]).await;
    w.admin.delete_library(w.library.id, false).await.unwrap();
    eventually("stopped", || async {
        a.pair().stopped.as_deref() == Some("library deleted")
    })
    .await;
    assert!(a.path("x.txt").exists());
    w.admin.restore_library(w.library.id).await.unwrap();
    eventually("resumed", || async { a.pair().stopped.is_none() }).await;
    a.write("y.txt", "y");
    w.converge(&[&a]).await;

    // This device's own (a shared server holds others).
    w.admin
        .revoke_app_password(a.app_password.unwrap())
        .await
        .unwrap();
    eventually("signed out", || async {
        a.state().accounts().unwrap()[0].signed_out
    })
    .await;
}

/// A new library under the account's limit is synced into the default
/// folder by itself; a bigger one is only offered. One removed is declined:
/// never synced again by itself, and no folder made for it.
#[tokio::test(flavor = "multi_thread")]
async fn small_new_libraries_sync_by_themselves() {
    let w = world().await;
    // Full before the device sees it (an empty library is small).
    let big = w.admin.create_library(w.organization, "Big").await.unwrap();
    w.upload(&big, "big.bin", &[7; 2048]).await;
    let a = w.device("a").await;
    a.state()
        .set_auto_sync_limit(a.account.id, Some(1024))
        .unwrap();
    w.converge(&[&a]).await;
    let small = w
        .admin
        .create_library(w.organization, "Small")
        .await
        .unwrap();
    w.upload(&small, "hello.txt", b"hello").await;
    // Its folder created (the default folder holds it), its content synced.
    let folder = a.account.default_folder.join("Small");
    eventually("Small synced", || async {
        std::fs::read(folder.join("hello.txt")).ok().as_deref() == Some(&b"hello"[..])
    })
    .await;
    let pair = a
        .state()
        .pairs()
        .unwrap()
        .into_iter()
        .find(|p| p.library == small.id)
        .unwrap();
    assert_eq!(pair.path, folder);
    assert!(
        pair.stopped.is_none() && pair.root_file_id.is_some(),
        "{pair:?}"
    );
    // Big, over the limit, looked at in the same pass: offered only.
    assert!(a.offered().contains(&big.id), "{:?}", a.offered());
    assert!(
        !a.state()
            .pairs()
            .unwrap()
            .iter()
            .any(|p| p.library == big.id)
    );
    assert!(!a.account.default_folder.join("Big").exists());

    // Small removed, its files with it: declined. Another new library makes
    // the engine look again: that one is synced, Small isn't, nor its folder.
    theca::ops::remove(&a.state(), &pair, true).unwrap();
    assert!(!folder.exists());
    let later = w
        .admin
        .create_library(w.organization, "Later")
        .await
        .unwrap();
    // The offers are published at the end of a pass: Small among them, the
    // pass is over.
    eventually("Later synced, Small offered", || async {
        a.state()
            .pairs()
            .unwrap()
            .iter()
            .any(|p| p.library == later.id)
            && a.offered().contains(&small.id)
    })
    .await;
    assert!(
        !a.state()
            .pairs()
            .unwrap()
            .iter()
            .any(|p| p.library == small.id)
    );
    assert!(!folder.exists(), "no folder for a declined library");
}

/// A library named like one already synced, in another organization, goes
/// into `<library> (<organization>)`.
#[tokio::test(flavor = "multi_thread")]
async fn clashing_names_get_their_organization() {
    let w = world().await;
    let a = w.device("a").await;
    a.state()
        .set_auto_sync_limit(a.account.id, Some(1024))
        .unwrap();
    w.converge(&[&a]).await;
    let beta: serde_json::Value = w
        .admin
        .call(
            reqwest::Method::POST,
            "/api/v1/organizations",
            Some(&serde_json::json!({ "name": "Beta" })),
        )
        .await
        .unwrap();
    let beta: uuid::Uuid = beta["id"].as_str().unwrap().parse().unwrap();
    let docs = w.admin.create_library(beta, "Docs").await.unwrap();
    eventually("Beta's Docs synced", || async {
        a.state()
            .pairs()
            .unwrap()
            .iter()
            .any(|p| p.library == docs.id)
    })
    .await;
    let pair = a
        .state()
        .pairs()
        .unwrap()
        .into_iter()
        .find(|p| p.library == docs.id)
        .unwrap();
    assert_eq!(pair.path, a.account.default_folder.join("Docs (Beta)"));
}

/// The folder moved while the app was off: the pair stops; Try again stops it
/// again while it's missing; Locate on the same folder resumes as it was;
/// Locate on another folder needs a confirmation, then merges it (nothing
/// trashed on the server).
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_folder_located() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.write("x.txt", "x");
    a.write("d/y.txt", "y");
    w.converge(&[&a]).await;
    a.stop().await;
    let moved = a.folder.with_file_name("Docs moved");
    std::fs::rename(&a.folder, &moved).unwrap();
    a.restart();
    eventually("stopped", || async { a.pair().stopped_for_its_folder() }).await;
    let state = a.state();
    theca::ops::set_paused(&state, &a.pair(), false).unwrap();
    eventually("stopped again", || async {
        a.pair().stopped_for_its_folder()
    })
    .await;

    // The same folder, moved: resumes from its base.
    let located = theca::ops::locate(&state, &a.pair(), &moved, false).unwrap();
    assert_eq!(located, theca::ops::Located::Moved);
    std::fs::write(moved.join("after.txt"), "after").unwrap();
    common::settle(&moved.join("after.txt"));
    eventually("after.txt uploaded", || async {
        w.remote(&w.library).await.contains_key("after.txt")
    })
    .await;

    // Another folder, empty: refused, then merged: everything comes down.
    let other = a.folder.with_file_name("Elsewhere");
    std::fs::create_dir(&other).unwrap();
    let located = theca::ops::locate(&state, &a.pair(), &other, false).unwrap();
    assert_eq!(located, theca::ops::Located::Different);
    theca::ops::locate(&state, &a.pair(), &other, true).unwrap();
    eventually("merged", || async {
        common::local(&other) == w.remote(&w.library).await
    })
    .await;
    assert!(
        w.admin
            .library_trash(w.library.id)
            .await
            .unwrap()
            .is_empty(),
        "nothing trashed"
    );
}

/// A member (not the organization's owner, who sees every library) taken
/// off the library: the pair stops, the files stay; given access back, it
/// resumes and catches up.
#[tokio::test(flavor = "multi_thread")]
async fn access_removed_and_given_back() {
    let w = world().await;
    // A shared server: no second account (see `member`).
    let Some((member, user)) = w.member("member@test.theca").await else {
        return;
    };
    w.set_member(&w.library, user, true).await;
    let password = member
        .create_app_password(&theca_common::api::CreateAppPassword {
            name: "m".into(),
            organization: None,
            library: None,
            read_only: false,
        })
        .await
        .unwrap()
        .password;
    let m = w.device_with("m", &w.library, &password, true).await;
    m.write("x.txt", "x");
    w.converge(&[&m]).await;

    w.set_member(&w.library, user, false).await;
    eventually("stopped", || async {
        m.pair().stopped.as_deref() == Some(theca::state::ACCESS_REMOVED)
    })
    .await;
    assert!(m.path("x.txt").exists(), "the files stay");
    // Changed meanwhile: comes down once access is back.
    w.upload(&w.library, "while away.txt", b"away").await;

    w.set_member(&w.library, user, true).await;
    eventually("resumed", || async { m.pair().stopped.is_none() }).await;
    w.converge(&[&m]).await;
    m.write("y.txt", "y");
    w.converge(&[&m]).await;
}

/// A library renamed: the pair's name follows, its folder keeps its name
/// (decisions.md, Sync: a rename never renames the local folder).
#[tokio::test(flavor = "multi_thread")]
async fn a_library_renamed_keeps_its_folder() {
    let w = world().await;
    let a = w.device("a").await;
    a.write("x.txt", "x");
    w.converge(&[&a]).await;
    w.admin
        .rename_library(w.library.id, "Papers")
        .await
        .unwrap();
    eventually("the name followed", || async {
        a.pair().library_name == "Papers"
    })
    .await;
    assert_eq!(a.pair().path, a.folder);
    assert!(a.folder.ends_with("Docs") && a.folder.is_dir());
    a.write("y.txt", "y");
    w.upload(&w.library, "z.txt", b"z").await;
    w.converge(&[&a]).await;
}

/// A file and a folder moved through the API to another library of the
/// organization: they leave the first library's folder and arrive in the
/// other's, nothing trashed.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_across_libraries() {
    let w = world().await;
    let other = w
        .admin
        .create_library(w.organization, "Other")
        .await
        .unwrap();
    let a = w.device("a").await;
    let b = w.device_for("b", &other, true).await;
    a.write("x.txt", "x");
    a.write("d/y.txt", "y");
    a.write("stays.txt", "s");
    w.converge(&[&a]).await;
    w.converge_on(&other, &[&b]).await;
    let nodes: Vec<_> = a
        .state()
        .entries(a.pair().id)
        .unwrap()
        .into_iter()
        .filter(|e| e.name == "x.txt" || e.name == "d")
        .map(|e| theca_common::api::UpdateNode {
            id: e.node,
            name: None,
            parent: Some(other.root),
            replace: false,
        })
        .collect();
    assert_eq!(nodes.len(), 2);
    for outcome in w.admin.update_nodes(&nodes).await.unwrap() {
        outcome.unwrap();
    }
    w.converge(&[&a]).await;
    w.converge_on(&other, &[&b]).await;
    assert_eq!(
        common::local(&a.folder).keys().collect::<Vec<_>>(),
        ["stays.txt"]
    );
    assert_eq!(
        common::local(&b.folder).keys().collect::<Vec<_>>(),
        ["d", "d/y.txt", "x.txt"]
    );
    assert!(
        w.admin
            .library_trash(w.library.id)
            .await
            .unwrap()
            .is_empty(),
        "moved, not trashed"
    );
}

/// Removing a pair deleting its files deletes only those identical to what
/// was synced (a modified one and one never uploaded stay); removing an
/// account keeping them leaves every file, and revokes its app password.
/// Nothing is deleted on the server either way.
#[tokio::test(flavor = "multi_thread")]
async fn removing_a_pair_or_an_account() {
    let w = world().await;
    let mut a = w.device("a").await;
    let mut b = w.device("b").await;
    a.write("same.txt", "same");
    a.write("d/same.txt", "same");
    a.write("edited.txt", "one");
    w.converge(&[&a, &b]).await;
    // Stopped: these stay local changes.
    a.stop().await;
    b.stop().await;
    a.write("edited.txt", "two");
    a.write("new.txt", "never uploaded");

    let kept = theca::ops::remove(&a.state(), &a.pair(), true).unwrap();
    assert_eq!(kept, 2);
    assert_eq!(
        common::local(&a.folder).keys().collect::<Vec<_>>(),
        ["edited.txt", "new.txt"]
    );
    assert!(a.state().pairs().unwrap().is_empty());

    let before = common::local(&b.folder);
    let kept = theca::ops::remove_account(&b.state(), &b.account, false)
        .await
        .unwrap();
    assert_eq!(kept, 0, "nothing deleted, nothing counted");
    assert_eq!(common::local(&b.folder), before);
    assert!(b.state().accounts().unwrap().is_empty());
    assert!(b.state().pairs().unwrap().is_empty());
    let passwords = w.admin.app_passwords().await.unwrap();
    assert!(!passwords.iter().any(|p| Some(p.id) == b.app_password));
    assert!(passwords.iter().any(|p| Some(p.id) == a.app_password));

    assert_eq!(w.remote(&w.library).await.len(), 4, "nothing deleted");
}
