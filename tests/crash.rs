//! The client killed at a named point (`THECA_CRASH_AT`, the `crash-points`
//! feature), then started again: nothing lost, nothing trashed by mistake.

#![cfg(feature = "crash-points")]

mod common;

use std::path::Path;
use std::time::Duration;

use common::*;

/// `theca <args>` on a device's state, killed at `point`.
async fn crash(state: &Path, point: &str, args: &[&str]) {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_theca"));
    command
        .arg("--state")
        .arg(state)
        .args(args)
        .env("THECA_CRASH_AT", point);
    let status = tokio::task::spawn_blocking(move || command.status())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.success(), "never reached {point}");
}

/// B5: removing a pair with its files, killed once they're deleted: the
/// next start doesn't take them for the user's deletions.
#[tokio::test(flavor = "multi_thread")]
async fn removal_killed_after_the_deletions() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.write("x.txt", "synced");
    w.converge(&[&a]).await;
    a.stop().await;
    // Never synced, so kept: the folder stays.
    a.write("local only.txt", "kept");
    crash(
        &a.state_path,
        "removal-deleted",
        &["remove", "Docs", "--delete-files"],
    )
    .await;
    assert!(!a.path("x.txt").exists(), "deleted before the crash");
    a.restart();
    // Past the quiet a deletion waits for, and a round.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let remote = w.remote(&w.library).await;
    assert!(remote.contains_key("x.txt"), "trashed: {:?}", remote.keys());
}

/// A new version of a synced file, uploaded as another client would.
async fn edit(w: &World, a: &Device, name: &str, content: &[u8]) {
    let e = a
        .state()
        .entries(a.pair().id)
        .unwrap()
        .into_iter()
        .find(|e| e.name == name)
        .unwrap();
    theca_client::upload(
        &w.admin,
        &theca_client::MemorySource(content.to_vec()),
        theca_client::Target::Version {
            library: w.library.id,
            file: e.node,
            base: e.version,
        },
        &theca_client::UploadOptions::default(),
        |_| {},
        &theca_client::Cancel::default(),
    )
    .await
    .unwrap();
}

/// B9: a batch of downloads killed at the second file: the first is
/// recorded, so it isn't taken for a local edit when the client starts again.
#[tokio::test(flavor = "multi_thread")]
async fn downloads_are_recorded_as_they_land() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.write("one.txt", "1");
    a.write("two.txt", "2");
    w.converge(&[&a]).await;
    a.stop().await;
    let before = a.state().entries(a.pair().id).unwrap();
    edit(&w, &a, "one.txt", b"one, edited").await;
    edit(&w, &a, "two.txt", b"two, edited").await;
    crash(&a.state_path, "downloaded-unrecorded#2", &["run"]).await;
    let after = a.state().entries(a.pair().id).unwrap();
    let recorded = after.iter().filter(|e| !before.contains(e)).count();
    assert_eq!(recorded, 1, "{after:?}");
}

/// B8: killed after a download landed but before it was recorded: on the
/// next start the file looks edited locally, and goes up again; the server
/// answers that it has this content already, and nothing forks.
#[tokio::test(flavor = "multi_thread")]
async fn a_download_killed_before_its_record_forks_nothing() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.write("x.txt", "one");
    w.converge(&[&a]).await;
    a.stop().await;
    edit(&w, &a, "x.txt", b"two, from elsewhere").await;
    crash(&a.state_path, "downloaded-unrecorded", &["run"]).await;
    a.restart();
    w.converge(&[&a]).await;
    let node = a
        .state()
        .entries(a.pair().id)
        .unwrap()
        .into_iter()
        .find(|e| e.name == "x.txt")
        .unwrap()
        .node;
    eventually("x.txt recorded", || async {
        a.state()
            .entries(a.pair().id)
            .unwrap()
            .iter()
            .any(|e| e.node == node && e.size == 19)
    })
    .await;
    assert_eq!(
        w.admin.versions(node).await.unwrap().len(),
        2,
        "one, two: no fork"
    );
    assert!(
        !a.notices()
            .iter()
            .any(|n| n.contains("also changed elsewhere")),
        "{:?}",
        a.notices()
    );
}

/// Killed after an upload was committed, before it was recorded: on the next
/// start it goes up again, and the server's "unchanged" forks nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_killed_before_its_record_forks_nothing() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.write("x.txt", "one");
    w.converge(&[&a]).await;
    a.stop().await;
    a.write("x.txt", "two, edited here");
    crash(&a.state_path, "uploaded-unrecorded", &["run"]).await;
    a.restart();
    w.converge(&[&a]).await;
    let node = a
        .state()
        .entries(a.pair().id)
        .unwrap()
        .into_iter()
        .find(|e| e.name == "x.txt")
        .unwrap()
        .node;
    assert_eq!(
        w.admin.versions(node).await.unwrap().len(),
        2,
        "one, two: no fork"
    );
    assert!(
        !a.notices()
            .iter()
            .any(|n| n.contains("also changed elsewhere")),
        "{:?}",
        a.notices()
    );
}

/// Killed after a local delete went to the server's trash, before it was
/// forgotten: on the next start nothing comes back, nothing errs.
#[tokio::test(flavor = "multi_thread")]
async fn a_trash_killed_before_its_record_is_done_once() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.write("x.txt", "x");
    a.write("y.txt", "y");
    w.converge(&[&a]).await;
    a.stop().await;
    std::fs::remove_file(a.path("x.txt")).unwrap();
    crash(&a.state_path, "trashed-unforgotten", &["run"]).await;
    a.restart();
    w.converge(&[&a]).await;
    assert!(!a.path("x.txt").exists());
    assert_eq!(w.admin.library_trash(w.library.id).await.unwrap().len(), 1);
    assert!(
        !a.notices().iter().any(|n| n.contains("error")),
        "{:?}",
        a.notices()
    );
}

/// Killed after a remote rename was done locally, before it was recorded: on
/// the next start the file is where the server has it, once, unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_move_killed_before_its_record_converges() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.write("x.txt", "x");
    w.converge(&[&a]).await;
    a.stop().await;
    let e = a
        .state()
        .entries(a.pair().id)
        .unwrap()
        .into_iter()
        .find(|e| e.name == "x.txt")
        .unwrap();
    let rename = theca_common::api::UpdateNode {
        id: e.node,
        name: Some(theca_common::name::Name::new("y.txt").unwrap()),
        parent: None,
        replace: false,
    };
    w.admin
        .update_nodes(&[rename])
        .await
        .unwrap()
        .pop()
        .unwrap()
        .unwrap();
    crash(&a.state_path, "moved-unrecorded", &["run"]).await;
    a.restart();
    w.converge(&[&a]).await;
    assert_eq!(std::fs::read_to_string(a.path("y.txt")).unwrap(), "x");
    assert_eq!(w.admin.versions(e.node).await.unwrap().len(), 1);
}

/// Killed after a remote delete was done locally, before it was recorded:
/// on the next start nothing comes back, nothing goes up again.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_delete_killed_before_its_record_converges() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.write("x.txt", "x");
    a.write("y.txt", "y");
    w.converge(&[&a]).await;
    a.stop().await;
    let x = a
        .state()
        .entries(a.pair().id)
        .unwrap()
        .into_iter()
        .find(|e| e.name == "x.txt")
        .unwrap()
        .node;
    w.admin.trash_nodes(&[x], false).await.unwrap();
    crash(&a.state_path, "deleted-unrecorded", &["run"]).await;
    a.restart();
    w.converge(&[&a]).await;
    assert!(!a.path("x.txt").exists());
    assert_eq!(w.admin.library_trash(w.library.id).await.unwrap().len(), 1);
}
