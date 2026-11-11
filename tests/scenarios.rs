//! Two devices and the server, converging after every step (the real server,
//! the real engines).

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use theca::state::NewLibraries;
use theca::state::Stop;

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
    // Noticed as its round ends, after the server already shows the merge.
    eventually("the merge reported", || async {
        b.notices()
            .iter()
            .any(|n| n.contains("also changed elsewhere"))
    })
    .await;
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
    // Written once the round ends, after the server already shows the trash.
    eventually("an undo notice", || async {
        a.state().latest_undo().unwrap().is_some()
    })
    .await;
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
    let mut reader = w
        .device_with("reader", &w.library, &password, true, true)
        .await;
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
    let err = theca::ops::add(
        &state,
        &account,
        "Docs",
        Some(a.folder.clone()),
        false,
        theca::state::Mode::Sync,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("confirm the merge"), "{err}");
    theca::ops::add(
        &state,
        &account,
        "Docs",
        Some(a.folder.clone()),
        true,
        theca::state::Mode::Sync,
    )
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
    eventually("stopped", || async { a.pair().stop.is_some() }).await;
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
        a.pair().stop == Some(Stop::LibraryDeleted)
    })
    .await;
    assert!(a.path("x.txt").exists());
    w.admin.restore_library(w.library.id).await.unwrap();
    eventually("resumed", || async { a.pair().stop.is_none() }).await;
    a.write("y.txt", "y");
    w.converge(&[&a]).await;

    w.admin
        .revoke_app_password(a.app_password.unwrap())
        .await
        .unwrap();
    eventually("signed out", || async {
        a.state().accounts().unwrap()[0].signed_out
    })
    .await;
}

/// `sync`: a library new to the account under its limit is synced into the
/// default folder by itself; a bigger one is only listed. One removed stays
/// seen: never synced again by itself, and no folder made for it.
#[tokio::test(flavor = "multi_thread")]
async fn small_new_libraries_sync_by_themselves() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.stop().await;
    a.state()
        .set_new_libraries(a.account.id, NewLibraries::Sync)
        .unwrap();
    a.state().set_auto_sync_limit(a.account.id, 1024).unwrap();
    // New to it, and full before its engine sees it (an empty library is
    // small).
    let big = w.admin.create_library(w.organization, "Big").await.unwrap();
    w.upload(&big, "big.bin", &[7; 2048]).await;
    a.restart();
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
        pair.stop.is_none() && pair.root_file_id.is_some(),
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
        .set_new_libraries(a.account.id, NewLibraries::Sync)
        .unwrap();
    a.state().set_auto_sync_limit(a.account.id, 1024).unwrap();
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
    let (member, user) = w.member("member@test.theca").await;
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
    let m = w.device_with("m", &w.library, &password, true, true).await;
    m.write("x.txt", "x");
    w.converge(&[&m]).await;

    w.set_member(&w.library, user, false).await;
    eventually("stopped", || async {
        m.pair().stop == Some(Stop::AccessRemoved)
    })
    .await;
    assert!(m.path("x.txt").exists(), "the files stay");
    // Changed meanwhile: comes down once access is back.
    w.upload(&w.library, "while away.txt", b"away").await;

    w.set_member(&w.library, user, true).await;
    eventually("resumed", || async { m.pair().stop.is_none() }).await;
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
    // Their entries, written just after the server took them.
    eventually("x.txt and d synced", || async {
        a.state()
            .entries(a.pair().id)
            .unwrap()
            .iter()
            .filter(|e| e.name == "x.txt" || e.name == "d")
            .count()
            == 2
    })
    .await;
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

/// A file's node in a device's base.
fn node(device: &Device, name: &str) -> uuid::Uuid {
    device
        .state()
        .entries(device.pair().id)
        .unwrap()
        .into_iter()
        .find(|e| e.name == name)
        .unwrap()
        .node
}

/// Waits until `marker` is on the server: the rounds that saw it ran, and
/// what they left is what they decided.
async fn marked(w: &World, marker: &str) {
    eventually(marker, || async {
        w.remote(&w.library).await.contains_key(marker)
    })
    .await;
}

/// An ignore file synced from one device stops uploads on both: what it names
/// never goes up, a stored file it names is read-only (a local edit or
/// deletion stays local, the server's deletion comes down); removing the
/// pattern resumes everything.
#[tokio::test(flavor = "multi_thread")]
async fn an_ignore_file_stops_uploads_on_every_device() {
    let w = world().await;
    let a = w.device("a").await;
    let b = w.device("b").await;
    a.write("keep.log", "stored before");
    a.write("old.log", "stored before");
    w.converge(&[&a, &b]).await;
    a.write(".thecaignore", "*.log\nbuild/\n");
    w.converge(&[&a, &b]).await;

    a.write("a.log", "never up");
    a.write("build/x.o", "never up");
    a.write("keep.log", "edited on a");
    std::fs::remove_file(b.path("old.log")).unwrap();
    b.write("b.log", "never up");
    a.write("marker-a.txt", "a's round ran");
    b.write("marker-b.txt", "b's round ran");
    marked(&w, "marker-a.txt").await;
    marked(&w, "marker-b.txt").await;
    let remote = w.remote(&w.library).await;
    let names: Vec<&str> = remote.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        [
            ".thecaignore",
            "keep.log",
            "marker-a.txt",
            "marker-b.txt",
            "old.log"
        ]
    );
    assert_eq!(remote["keep.log"].as_deref(), Some(&b"stored before"[..]));
    assert!(
        w.admin
            .library_trash(w.library.id)
            .await
            .unwrap()
            .is_empty(),
        "nothing trashed"
    );
    assert!(
        !a.notices()
            .iter()
            .chain(&b.notices())
            .any(|n| n.contains("read-only")),
        "silent: {:?}",
        a.notices()
    );

    // A new version on the server comes down: onto a's unchanged old.log,
    // and back onto b, which had deleted it.
    let old = node(&a, "old.log");
    let base = w.admin.versions(old).await.unwrap()[0].id;
    theca_client::upload(
        &w.admin,
        &theca_client::MemorySource(b"from the server".to_vec()),
        theca_client::Target::Version {
            library: w.library.id,
            file: old,
            base: Some(base),
        },
        &theca_client::UploadOptions::default(),
        |_| {},
        &theca_client::Cancel::default(),
    )
    .await
    .unwrap();
    for device in [&a, &b] {
        eventually("the server's version down", || async {
            std::fs::read_to_string(device.path("old.log"))
                .ok()
                .as_deref()
                == Some("from the server")
        })
        .await;
    }

    // Changed on the server and edited on a: b gets it; a keeps its edit,
    // and no copy aside (its name could escape the pattern).
    let keep = node(&a, "keep.log");
    let base = w.admin.versions(keep).await.unwrap()[0].id;
    theca_client::upload(
        &w.admin,
        &theca_client::MemorySource(b"keep, from the server".to_vec()),
        theca_client::Target::Version {
            library: w.library.id,
            file: keep,
            base: Some(base),
        },
        &theca_client::UploadOptions::default(),
        |_| {},
        &theca_client::Cancel::default(),
    )
    .await
    .unwrap();
    eventually("b has the server's keep.log", || async {
        std::fs::read_to_string(b.path("keep.log")).ok().as_deref() == Some("keep, from the server")
    })
    .await;
    a.write("marker-a2.txt", "a's round ran");
    marked(&w, "marker-a2.txt").await;
    assert_eq!(
        std::fs::read_to_string(a.path("keep.log")).unwrap(),
        "edited on a"
    );
    assert!(!a.path("keep (local).log").exists());

    // Deleted on the server: deleted where unchanged (old.log), kept where
    // edited (a's keep.log).
    let (keep, old) = (node(&a, "keep.log"), node(&a, "old.log"));
    w.admin.trash_nodes(&[keep, old], false).await.unwrap();
    eventually("old.log deleted on a", || async {
        !a.path("old.log").exists()
    })
    .await;
    eventually("keep.log deleted on b", || async {
        !b.path("keep.log").exists()
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(a.path("keep.log")).unwrap(),
        "edited on a"
    );

    // The patterns removed: everything goes up.
    a.write(".thecaignore", "# nothing\n");
    eventually("everything up", || async {
        let remote = w.remote(&w.library).await;
        ["a.log", "b.log", "build/x.o", "keep.log"]
            .iter()
            .all(|p| remote.contains_key(*p))
    })
    .await;
    w.converge(&[&a, &b]).await;
}

/// A folder paired with a library whose ignore file names some of its
/// content: nothing of it goes up, not even before the ignore file is down.
#[tokio::test(flavor = "multi_thread")]
async fn a_first_sync_waits_for_the_library_s_ignore_file() {
    let w = world().await;
    w.upload(&w.library, ".thecaignore", b"node_modules/\n*.tmp\n")
        .await;
    let mut a = w.device_for("a", &w.library, false).await;
    a.write("node_modules/x/index.js", "never up");
    a.write("scratch.tmp", "never up");
    a.write("src.txt", "up");
    a.restart();
    marked(&w, "src.txt").await;
    eventually("the ignore file down", || async {
        a.path(".thecaignore").exists()
    })
    .await;
    let remote = w.remote(&w.library).await;
    let names: Vec<&str> = remote.keys().map(String::as_str).collect();
    assert_eq!(names, [".thecaignore", "src.txt"]);
}

/// A tree moved into the pair from the same disk (one rename) arrives with
/// its nested ignore file: the first scan sees it, nothing it names goes up.
#[tokio::test(flavor = "multi_thread")]
async fn a_tree_moved_in_uploads_nothing_its_ignore_file_names() {
    let w = world().await;
    let a = w.device("a").await;
    w.converge(&[&a]).await;
    let outside = a.folder.parent().unwrap().join("outside");
    for (path, content) in [
        ("proj/sub/.thecaignore", "*.tmp\ncache/\n"),
        ("proj/sub/x.tmp", "never up"),
        ("proj/sub/cache/y", "never up"),
        ("proj/sub/kept.txt", "up"),
    ] {
        let full = outside.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, content).unwrap();
        settle(&full);
    }
    std::fs::rename(outside.join("proj"), a.path("proj")).unwrap();
    // Both up: they upload side by side, either first.
    marked(&w, "proj/sub/kept.txt").await;
    marked(&w, "proj/sub/.thecaignore").await;
    let remote = w.remote(&w.library).await;
    let names: Vec<&str> = remote.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        [
            "proj",
            "proj/sub",
            "proj/sub/.thecaignore",
            "proj/sub/kept.txt"
        ]
    );
}

/// A synced file moved into an ignored folder that was never stored (the
/// scan doesn't enter it) isn't a deletion: it stays where it was on the
/// server.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_moved_into_an_ignored_folder_stays_on_the_server() {
    let w = world().await;
    let a = w.device("a").await;
    a.write(".thecaignore", "build/\n");
    a.write("d/a.txt", "synced");
    w.converge(&[&a]).await;
    std::fs::create_dir_all(a.path("build")).unwrap();
    std::fs::rename(a.path("d/a.txt"), a.path("build/a.txt")).unwrap();
    a.write("marker.txt", "a's round ran");
    marked(&w, "marker.txt").await;
    // Past the quiet time deletions wait for, and a round after it.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    a.write("marker2.txt", "a's next round ran");
    marked(&w, "marker2.txt").await;
    assert!(w.remote(&w.library).await.contains_key("d/a.txt"));
    assert!(
        w.admin
            .library_trash(w.library.id)
            .await
            .unwrap()
            .is_empty()
    );
}

/// A folder deleted on the server, holding ignored files never stored: what
/// was synced is deleted, the ignored files stay with their folder (never
/// on the server), and the folder isn't uploaded again for them.
#[tokio::test(flavor = "multi_thread")]
async fn a_folder_deleted_on_the_server_keeps_its_ignored_files() {
    let w = world().await;
    let a = w.device("a").await;
    a.write(".thecaignore", "*.log\n");
    a.write("d/k.txt", "synced");
    w.converge(&[&a]).await;
    a.write("d/x.log", "only here");
    let d = node(&a, "d");
    w.admin.trash_nodes(&[d], false).await.unwrap();
    eventually("k.txt deleted", || async { !a.path("d/k.txt").exists() }).await;
    eventually("said", || async {
        a.notices()
            .iter()
            .any(|n| n.contains("holds what isn't synced"))
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(a.path("d/x.log")).unwrap(),
        "only here"
    );
    a.write("marker.txt", "a's round ran");
    marked(&w, "marker.txt").await;
    assert!(!w.remote(&w.library).await.contains_key("d"));
}

/// An ignore file's line that isn't UTF-8 is said on the pair; the rest
/// applies.
#[tokio::test(flavor = "multi_thread")]
async fn an_ignore_file_s_bad_line_is_said() {
    let w = world().await;
    let a = w.device("a").await;
    std::fs::write(a.path(".thecaignore"), b"\xff\n*.log\n").unwrap();
    settle(&a.path(".thecaignore"));
    a.write("a.log", "never up");
    eventually("said", || async {
        a.notices()
            .iter()
            .any(|n| n.starts_with(".thecaignore: ignore file: line 1 skipped"))
    })
    .await;
    a.write("marker.txt", "a's round ran");
    marked(&w, "marker.txt").await;
    assert!(!w.remote(&w.library).await.contains_key("a.log"));
}

/// The libraries announced as new (`Update::NewLibrary`: the window's OS
/// notification), by name, as the engine publishes them.
fn announced(d: &Device) -> Arc<Mutex<Vec<String>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut updates = d.handle.updates.subscribe();
    let names = seen.clone();
    tokio::spawn(async move {
        while let Ok(update) = updates.recv().await {
            if let theca::state::Update::NewLibrary { name, .. } = update {
                names.lock().unwrap().push(name);
            }
        }
    });
    seen
}

/// `notify`: a library new to the account is listed and announced once,
/// not again after a restart; one there at login never is. `silent`: listed
/// only. `sync`: 0 is no limit; above the limit, announced.
#[tokio::test(flavor = "multi_thread")]
async fn new_libraries_follow_the_account() {
    let w = world().await;
    // There before the login: never new.
    let before = w
        .admin
        .create_library(w.organization, "Before")
        .await
        .unwrap();
    let mut a = w.device("a").await;
    let state = a.state();
    state
        .set_new_libraries(a.account.id, NewLibraries::Notify)
        .unwrap();
    let said = announced(&a);
    a.handle.wake();
    let notified = w
        .admin
        .create_library(w.organization, "Notified")
        .await
        .unwrap();
    // The list is the engine's at once, the announcement a channel's
    // message: both waited for, then a moment for one too many.
    eventually("Notified listed and announced", || async {
        a.offered().contains(&notified.id) && !said.lock().unwrap().is_empty()
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    assert!(a.offered().contains(&before.id), "Before listed too");
    let announced_now = said.lock().unwrap().clone();
    assert_eq!(announced_now, ["Notified"], "once, Before never");
    // A restart, still `notify`: listed again, never announced again.
    a.stop().await;
    a.restart();
    let said = announced(&a);
    eventually("Notified listed again", || async {
        a.offered().contains(&notified.id)
    })
    .await;
    // Time for an announcement, if one were coming.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert!(
        said.lock().unwrap().is_empty(),
        "{:?}",
        said.lock().unwrap()
    );
    state
        .set_new_libraries(a.account.id, NewLibraries::Silent)
        .unwrap();
    let quiet = w
        .admin
        .create_library(w.organization, "Quiet")
        .await
        .unwrap();
    eventually("Quiet listed", || async {
        a.offered().contains(&quiet.id) && a.offered().contains(&notified.id)
    })
    .await;
    assert!(
        said.lock().unwrap().is_empty(),
        "{:?}",
        said.lock().unwrap()
    );

    // `sync`, no limit: a big library syncs; with one, a bigger one is
    // announced, listed.
    state
        .set_new_libraries(a.account.id, NewLibraries::Sync)
        .unwrap();
    state.set_auto_sync_limit(a.account.id, 0).unwrap();
    a.stop().await;
    let big = w.admin.create_library(w.organization, "Big").await.unwrap();
    w.upload(&big, "big.bin", &[7; 4096]).await;
    a.restart();
    eventually("Big synced", || async {
        state.pairs().unwrap().iter().any(|p| p.library == big.id)
    })
    .await;
    state.set_auto_sync_limit(a.account.id, 1024).unwrap();
    a.stop().await;
    let bigger = w
        .admin
        .create_library(w.organization, "Bigger")
        .await
        .unwrap();
    w.upload(&bigger, "big.bin", &[7; 4096]).await;
    a.restart();
    let said = announced(&a);
    eventually("Bigger listed and announced", || async {
        a.offered().contains(&bigger.id) && said.lock().unwrap().contains(&"Bigger".to_owned())
    })
    .await;
    assert!(
        !state
            .pairs()
            .unwrap()
            .iter()
            .any(|p| p.library == bigger.id)
    );
    for l in [&before, &notified, &quiet] {
        assert!(
            !state.pairs().unwrap().iter().any(|p| p.library == l.id),
            "{}",
            l.name
        );
    }
    a.stop().await;
}

/// `drive`: a library new to the account becomes a drive; where one can't
/// be (no FUSE: the CI image), an error notice names it and why, and nothing
/// is synced instead.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn a_new_library_becomes_a_drive() {
    let w = world().await;
    let mut a = w.device("a").await;
    let state = a.state();
    state
        .set_new_libraries(a.account.id, NewLibraries::Drive)
        .unwrap();
    a.handle.wake();
    let photos = w
        .admin
        .create_library(w.organization, "Photos")
        .await
        .unwrap();
    match theca::drive::unavailable() {
        None => {
            eventually("Photos a drive", || async {
                state
                    .pairs()
                    .unwrap()
                    .iter()
                    .any(|p| p.library == photos.id && p.mode == theca::state::Mode::Drive)
            })
            .await;
        }
        Some(why) => {
            eventually("an error notice", || async {
                a.notices()
                    .iter()
                    .any(|n| n.contains("Photos not added as a drive") && n.contains(&why))
            })
            .await;
            assert!(a.offered().contains(&photos.id));
            assert!(
                !state
                    .pairs()
                    .unwrap()
                    .iter()
                    .any(|p| p.library == photos.id)
            );
        }
    }
    // Stopped before its folder goes: a mount's own engine can't serve its
    // removal.
    a.stop().await;
}
