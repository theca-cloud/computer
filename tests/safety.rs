//! What could lose a user's files (sprint 11): each test failed before its
//! fix (the real server, the real engines).

mod common;

use common::*;
use theca::local;
use theca::state::Entry;
use uuid::Uuid;

/// A1: the server's database restored from a backup (the feed's `reset`) is a
/// resync with no base: a file created since the backup was never on the
/// restored server, so it goes up again instead of being deleted.
#[tokio::test(flavor = "multi_thread")]
async fn a_reset_forgets_the_base() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.write("x.txt", "one");
    w.converge(&[&a]).await;
    a.stop().await;

    // What the base holds for a file the restored server never had, and a
    // position above the library's counter.
    a.write("ghost.txt", "created after the backup");
    let meta = std::fs::metadata(a.path("ghost.txt")).unwrap();
    let pair = a.pair();
    let ghost = Entry {
        node: Uuid::now_v7(),
        parent: pair.root,
        name: "ghost.txt".into(),
        is_file: true,
        version: Some(Uuid::now_v7()),
        size: meta.len() as i64,
        mtime: local::mtime(&meta),
        file_id: local::file_id(&a.path("ghost.txt")),
    };
    a.state()
        .synced(pair.id, &ghost, Some("ghost.txt"), &[])
        .unwrap();
    a.state()
        .record_remote(pair.id, &[], Some(1_000_000))
        .unwrap();

    a.restart();
    w.converge(&[&a]).await;
    assert_eq!(
        std::fs::read_to_string(a.path("ghost.txt")).unwrap(),
        "created after the backup"
    );
}

/// Whether a folder's disk tells `A` from `a` (ext4; not macOS's or Windows'
/// default): on the others, the tests needing it pass without running.
fn case_sensitive(dir: &std::path::Path) -> bool {
    let probe = dir.join("case-probe");
    std::fs::write(&probe, "").unwrap();
    let sensitive = !dir.join("CASE-PROBE").exists();
    std::fs::remove_file(probe).unwrap();
    if !sensitive {
        eprintln!("skipped: {} ignores case", dir.display());
    }
    sensitive
}

/// A3: a case-only rename on the server never replaces a local file that
/// differs from the synced one only by case (two files on ext4).
#[tokio::test(flavor = "multi_thread")]
async fn a_case_only_rename_never_replaces_a_case_variant() {
    let w = world().await;
    let a = w.device("a").await;
    if !case_sensitive(a.folder.parent().unwrap()) {
        return;
    }
    a.write("Report.txt", "synced");
    w.converge(&[&a]).await;
    a.write("report.txt", "mine");
    eventually("the variant reported", || async {
        a.notices().iter().any(|n| n.starts_with("report.txt"))
    })
    .await;

    let e = a
        .state()
        .entries(a.pair().id)
        .unwrap()
        .into_iter()
        .find(|e| e.name == "Report.txt")
        .unwrap();
    let rename = theca_common::api::UpdateNode {
        id: e.node,
        name: Some(theca_common::name::Name::new("report.txt").unwrap()),
        parent: Some(e.parent),
        replace: false,
    };
    w.admin
        .update_nodes(&[rename])
        .await
        .unwrap()
        .pop()
        .unwrap()
        .unwrap();
    eventually("the rename seen", || async {
        a.notices().iter().any(|n| n.contains("not moved here"))
            || a.state()
                .entries(a.pair().id)
                .unwrap()
                .iter()
                .any(|e| e.name == "report.txt")
    })
    .await;
    let contents: Vec<String> = ["Report.txt", "report.txt"]
        .iter()
        .filter_map(|n| std::fs::read_to_string(a.path(n)).ok())
        .collect();
    assert!(
        contents.iter().any(|c| c == "mine"),
        "the local variant is gone: {contents:?}"
    );
}

/// B1: a folder the scan can't read (permissions, a disk error) is not a
/// deletion: its files stay on the server.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn an_unreadable_folder_is_not_deleted() {
    use std::os::unix::fs::PermissionsExt;
    let w = world().await;
    let a = w.device("a").await;
    a.write("d/x.txt", "kept");
    w.converge(&[&a]).await;
    std::fs::set_permissions(a.path("d"), std::fs::Permissions::from_mode(0o000)).unwrap();
    // A round after it: another change, uploaded.
    a.write("y.txt", "a round");
    eventually("y.txt uploaded", || async {
        w.remote(&w.library).await.contains_key("y.txt")
    })
    .await;
    eventually("the folder reported", || async {
        a.notices().iter().any(|n| n.starts_with("d: not synced"))
    })
    .await;
    // Deletions wait for the folder to be quiet for 2 s (the chmod was an
    // event): past it, with a round to act.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    a.write("z.txt", "another round");
    eventually("z.txt uploaded", || async {
        w.remote(&w.library).await.contains_key("z.txt")
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let remote = w.remote(&w.library).await;
    std::fs::set_permissions(a.path("d"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        remote.contains_key("d/x.txt"),
        "trashed: {:?}",
        remote.keys()
    );
}

/// B6: a pair's folder is never inside another pair's, nor around one.
#[tokio::test(flavor = "multi_thread")]
async fn a_pair_is_never_inside_or_around_another() {
    let w = world().await;
    let a = w.device("a").await;
    let notes = w
        .admin
        .create_library(w.organization, "Notes")
        .await
        .unwrap();
    for path in [
        a.folder.join("inner"),
        a.folder.parent().unwrap().to_owned(),
    ] {
        let added = theca::ops::add(
            &a.state(),
            &a.account,
            &notes.id.to_string(),
            Some(path.clone()),
            true,
        )
        .await;
        assert!(
            matches!(added, Err(theca::ops::OpError::Refused(_))),
            "{}: {added:?}",
            path.display()
        );
    }
}

/// B7: two accounts of one device, one library shared with both: auto-sync
/// never puts the second pair in the first one's folder.
#[tokio::test(flavor = "multi_thread")]
async fn two_accounts_never_share_a_folder() {
    let w = world().await;
    let a = w.device("a").await;
    w.converge(&[&a]).await;
    let (member, user) = w.member("m@test.theca").await;
    w.set_member(&w.library, user, true).await;
    let created = member
        .create_app_password(&theca_common::api::CreateAppPassword {
            name: "a".into(),
            organization: None,
            library: None,
            read_only: false,
        })
        .await
        .unwrap();
    let state = a.state();
    let b = theca::ops::login(
        &state,
        &w.server.url,
        "a",
        Some(a.folder.parent().unwrap().to_owned()),
        Some(created.password),
        |_| panic!("a token needs no browser"),
    )
    .await
    .unwrap();
    state.set_auto_sync_limit(b.id, Some(1 << 30)).unwrap();
    a.handle.wake();
    eventually("the second account's pair", || async {
        state.pairs().unwrap().iter().any(|p| p.account == b.id)
    })
    .await;
    let pairs = state.pairs().unwrap();
    assert_ne!(pairs[0].path, pairs[1].path, "one folder for two pairs");
}

/// B10: a local rename the server refuses (the name is taken there) doesn't
/// bring the old name back, nor merge the renamed file over the other one.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_rename_brings_nothing_back() {
    let w = world().await;
    let mut a = w.device("a").await;
    let b = w.device("b").await;
    a.write("a.txt", "one");
    w.converge(&[&a, &b]).await;
    a.stop().await;
    b.write("a.txt", "edited on b");
    b.write("b.txt", "b's own");
    w.converge(&[&b]).await;
    std::fs::rename(a.path("a.txt"), a.path("b.txt")).unwrap();
    a.restart();
    eventually("the rename refused", || async {
        a.notices()
            .iter()
            .any(|n| n.starts_with("b.txt: a file or folder with this name"))
    })
    .await;
    // A few rounds more: still nothing brought back.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(!a.path("a.txt").exists(), "the old name came back");
    let remote = w.remote(&w.library).await;
    assert_eq!(remote["b.txt"].as_deref(), Some(&b"b's own"[..]));
}

/// B8: a file only touched (same bytes, a new time) while it changed on the
/// server stays the server's: the touch doesn't put the stale copy back.
#[tokio::test(flavor = "multi_thread")]
async fn a_touch_never_beats_a_remote_edit() {
    let w = world().await;
    let mut a = w.device("a").await;
    let b = w.device("b").await;
    a.write("x.txt", "one");
    w.converge(&[&a, &b]).await;
    a.stop().await;
    b.write("x.txt", "two, from b");
    w.converge(&[&b]).await;
    let touched = std::fs::File::options()
        .write(true)
        .open(a.path("x.txt"))
        .unwrap();
    touched
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
        .unwrap();
    drop(touched);
    a.restart();
    w.converge(&[&a, &b]).await;
    assert_eq!(
        std::fs::read_to_string(a.path("x.txt")).unwrap(),
        "two, from b"
    );
}

/// C9: a folder deleted on the server goes locally even when the OS left a
/// file in it (`.DS_Store`): nothing anybody synced.
#[tokio::test(flavor = "multi_thread")]
async fn a_folder_holding_what_the_os_leaves_is_deleted() {
    let w = world().await;
    let a = w.device("a").await;
    a.write("d/y.txt", "y");
    w.converge(&[&a]).await;
    a.write("d/.DS_Store", "finder");
    let d = a
        .state()
        .entries(a.pair().id)
        .unwrap()
        .into_iter()
        .find(|e| e.name == "d")
        .unwrap()
        .node;
    w.admin.trash_nodes(&[d], false).await.unwrap();
    eventually("d gone locally", || async { !a.path("d").exists() }).await;
}

/// C10: a file from the server where something local already has its name
/// (a synced file renamed onto it meanwhile) is listed, not fetched again
/// every few seconds.
#[tokio::test(flavor = "multi_thread")]
async fn a_download_onto_a_local_file_is_listed() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.write("a.txt", "synced");
    w.converge(&[&a]).await;
    a.stop().await;
    w.upload(&w.library, "b.txt", b"the server's").await;
    std::fs::rename(a.path("a.txt"), a.path("b.txt")).unwrap();
    a.restart();
    eventually("listed", || async {
        a.notices()
            .iter()
            .any(|n| n.starts_with("b.txt: not downloaded"))
    })
    .await;
    assert_eq!(std::fs::read_to_string(a.path("b.txt")).unwrap(), "synced");
}

/// C7: moved into a new folder and edited on the server between two rounds:
/// the file comes down in its new place with its new content.
#[tokio::test(flavor = "multi_thread")]
async fn a_remote_move_and_edit_both_come_down() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.write("d/x.txt", "one");
    w.converge(&[&a]).await;
    a.stop().await;
    let x = a
        .state()
        .entries(a.pair().id)
        .unwrap()
        .into_iter()
        .find(|e| e.name == "x.txt")
        .unwrap();
    let n = w
        .admin
        .create_folders(w.library.root, &["n".into()])
        .await
        .unwrap()
        .remove(0);
    let to_n = theca_common::api::UpdateNode {
        id: x.node,
        name: None,
        parent: Some(n.id),
        replace: false,
    };
    w.admin
        .update_nodes(&[to_n])
        .await
        .unwrap()
        .pop()
        .unwrap()
        .unwrap();
    theca_client::upload(
        &w.admin,
        &theca_client::MemorySource(b"two".to_vec()),
        theca_client::Target::Version {
            library: w.library.id,
            file: x.node,
            base: x.version,
        },
        &theca_client::UploadOptions::default(),
        |_| {},
        &theca_client::Cancel::default(),
    )
    .await
    .unwrap();
    a.restart();
    w.converge(&[&a]).await;
    assert_eq!(std::fs::read_to_string(a.path("n/x.txt")).unwrap(), "two");
}

/// C6: names swapped on the server (`a` ↔ `b`) come down swapped, instead of
/// each move waiting on the other forever.
#[tokio::test(flavor = "multi_thread")]
async fn a_remote_swap_comes_down() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.write("a.txt", "A");
    a.write("b.txt", "B");
    w.converge(&[&a]).await;
    a.stop().await;
    let entries = a.state().entries(a.pair().id).unwrap();
    let node = |n: &str| entries.iter().find(|e| e.name == n).unwrap().node;
    let rename = |id, to: &str| theca_common::api::UpdateNode {
        id,
        name: Some(theca_common::name::Name::new(to).unwrap()),
        parent: None,
        replace: false,
    };
    for (id, to) in [
        (node("a.txt"), "t.txt"),
        (node("b.txt"), "a.txt"),
        (node("a.txt"), "b.txt"),
    ] {
        w.admin
            .update_nodes(&[rename(id, to)])
            .await
            .unwrap()
            .pop()
            .unwrap()
            .unwrap();
    }
    a.restart();
    w.converge(&[&a]).await;
    assert_eq!(std::fs::read_to_string(a.path("b.txt")).unwrap(), "A");
}

/// C12: a first sync of hundreds of files under macOS's default limit of 256
/// open files (an app started from the Finder): every file goes up. Run again
/// in a shell lowering the limit, which holds for the whole process.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn many_files_go_up_with_few_open_files() {
    if std::env::var_os("THECA_FEW_FILES").is_none() {
        let mut sh = std::process::Command::new("sh");
        sh.arg("-c")
            .arg("ulimit -n 256 && exec \"$0\" --exact many_files_go_up_with_few_open_files --nocapture")
            .arg(std::env::current_exe().unwrap())
            .env("THECA_FEW_FILES", "1");
        let status = tokio::task::spawn_blocking(move || sh.status())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success());
        return;
    }
    let w = world().await;
    let mut a = w.device("a").await;
    w.converge(&[&a]).await;
    a.stop().await;
    // All there before a round: one batch.
    for i in 0..400 {
        a.write(&format!("f{i}.txt"), &format!("file {i}"));
    }
    a.restart();
    // Never "too many open files" on the way (retried, it would get there).
    let mut seen = Vec::new();
    while w.remote(&w.library).await.len() < 400 {
        seen.extend(a.notices().into_iter().filter(|n| n.contains("open files")));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(seen.is_empty(), "{:?}", &seen[..1]);
}

/// C14: a folder copied in just before its library was trashed comes down
/// with its content once the library is restored.
#[tokio::test(flavor = "multi_thread")]
async fn a_folder_arriving_in_a_trashed_library_comes_down_on_restore() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.write("src/y.txt", "y");
    w.converge(&[&a]).await;
    a.stop().await;
    let src = a
        .state()
        .entries(a.pair().id)
        .unwrap()
        .into_iter()
        .find(|e| e.name == "src")
        .unwrap()
        .node;
    let copy = theca_common::api::CopyNode {
        id: src,
        parent: w.library.root,
        name: Some(theca_common::name::Name::new("copy").unwrap()),
        replace: false,
    };
    w.admin
        .copy_nodes(&[copy])
        .await
        .unwrap()
        .pop()
        .unwrap()
        .unwrap();
    w.admin.delete_library(w.library.id, false).await.unwrap();
    a.restart();
    eventually("stopped", || async {
        a.pair().stopped.as_deref() == Some("library deleted")
    })
    .await;
    w.admin.restore_library(w.library.id).await.unwrap();
    w.converge(&[&a]).await;
    assert!(a.path("copy/y.txt").exists());
}

/// C18: `theca add <name>` with that name in two organizations asks for the
/// library's ID instead of taking one of them.
#[tokio::test(flavor = "multi_thread")]
async fn an_ambiguous_library_name_is_refused() {
    let w = world().await;
    let a = w.device("a").await;
    let other: serde_json::Value = w
        .admin
        .call(
            reqwest::Method::POST,
            "/api/v1/organizations",
            Some(&serde_json::json!({ "name": "Other" })),
        )
        .await
        .unwrap();
    let other: uuid::Uuid = other["id"].as_str().unwrap().parse().unwrap();
    let ours = w
        .admin
        .create_library(w.organization, "Notes")
        .await
        .unwrap();
    w.admin.create_library(other, "Notes").await.unwrap();
    let folder = a.folder.parent().unwrap().join("Notes");
    let added = theca::ops::add(&a.state(), &a.account, "Notes", Some(folder.clone()), false).await;
    assert!(
        matches!(&added, Err(theca::ops::OpError::Refused(why)) if why.contains("several")),
        "{added:?}"
    );
    theca::ops::add(
        &a.state(),
        &a.account,
        &ours.id.to_string(),
        Some(folder),
        false,
    )
    .await
    .unwrap();
}

/// C4: an upload failing on the network (the server gone) is listed while it
/// is retried, never retried in silence.
#[tokio::test(flavor = "multi_thread")]
async fn a_failing_upload_is_listed() {
    let mut w = world().await;
    let a = w.device("a").await;
    // Following the feed (no round runs before it caught up).
    a.write("first.txt", "1");
    w.converge(&[&a]).await;
    w.server.kill();
    a.write("x.txt", "x");
    eventually("listed", || async {
        a.notices().iter().any(|n| n.starts_with("x.txt"))
    })
    .await;
}

// The guards that already worked, each with a test that fails without it.

/// Bytes nobody can compress: every chunk of them is uploaded and fetched.
fn noise(len: usize) -> Vec<u8> {
    let mut x = 0x9e37_79b9_7f4a_7c15_u64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 56) as u8
        })
        .collect()
}

/// The visible names in a folder (a download's part file is hidden).
fn visible(folder: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(folder)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect()
}

/// A download cut mid-stream leaves the synced file as it was, and the
/// state too; with the network back, the new version comes down whole.
#[tokio::test(flavor = "multi_thread")]
async fn a_cut_download_leaves_the_file_as_it_was() {
    let (w, cut) = world_cut().await;
    let mut a = w.device("a").await;
    let one = noise(4 * 1024 * 1024);
    std::fs::write(a.path("big.bin"), &one).unwrap();
    settle(&a.path("big.bin"));
    w.converge(&[&a]).await;
    a.stop().await;
    let before = a.state().entries(a.pair().id).unwrap();
    let e = before.iter().find(|e| e.name == "big.bin").unwrap().clone();
    let two: Vec<u8> = one.iter().rev().copied().collect();
    theca_client::upload(
        &w.admin,
        &theca_client::MemorySource(two.clone()),
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
    cut.store(true, std::sync::atomic::Ordering::SeqCst);
    a.restart();
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;
    a.stop().await;
    assert!(
        std::fs::read(a.path("big.bin")).unwrap() == one,
        "the synced content is gone"
    );
    assert_eq!(visible(&a.folder), ["big.bin"]);
    assert_eq!(a.state().entries(a.pair().id).unwrap(), before);
    cut.store(false, std::sync::atomic::Ordering::SeqCst);
    a.restart();
    w.converge(&[&a]).await;
    assert!(std::fs::read(a.path("big.bin")).unwrap() == two);
}

/// An upload cut mid-stream makes no version; with the network back, it goes
/// up whole.
#[tokio::test(flavor = "multi_thread")]
async fn a_cut_upload_makes_no_version() {
    let (w, cut) = world_cut().await;
    let mut a = w.device("a").await;
    a.write("first.txt", "1");
    w.converge(&[&a]).await;
    cut.store(true, std::sync::atomic::Ordering::SeqCst);
    let big = noise(4 * 1024 * 1024);
    std::fs::write(a.path("big.bin"), &big).unwrap();
    settle(&a.path("big.bin"));
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;
    a.stop().await;
    cut.store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(!w.remote(&w.library).await.contains_key("big.bin"));
    a.restart();
    w.converge(&[&a]).await;
}

/// The pair's folder replaced by an empty one (a disk unmounted, its empty
/// mount point left): never read as everything deleted.
#[tokio::test(flavor = "multi_thread")]
async fn a_replaced_folder_is_never_read_as_deleted() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.write("x.txt", "x");
    w.converge(&[&a]).await;
    a.stop().await;
    std::fs::rename(&a.folder, a.folder.with_file_name("unmounted")).unwrap();
    std::fs::create_dir(&a.folder).unwrap();
    a.restart();
    eventually("stopped", || async { a.pair().stopped.is_some() }).await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(w.remote(&w.library).await.contains_key("x.txt"));
}

/// Two engines never run on one state: the second `theca run` refuses.
#[tokio::test(flavor = "multi_thread")]
async fn two_engines_never_share_a_state() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.stop().await;
    let theca = || {
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_theca"));
        c.arg("--state").arg(&a.state_path).arg("run");
        c
    };
    let mut first = theca().spawn().unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let mut second = theca()
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Refused at once; still running after a few seconds, it wasn't.
    let mut ended = None;
    for _ in 0..50 {
        ended = second.try_wait().unwrap();
        if ended.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    for child in [&mut first, &mut second] {
        let _ = child.kill();
        let _ = child.wait();
    }
    assert!(ended.is_some_and(|s| !s.success()), "the second engine ran");
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut second.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(stderr.contains("already running"), "{stderr}");
}

/// Whether a folder's disk keeps NFC and NFD spellings of a name apart
/// (ext4; not APFS, nor NTFS).
fn normalization_sensitive(dir: &std::path::Path) -> bool {
    let probe = dir.join("caf\u{e9}-probe");
    std::fs::write(&probe, "").unwrap();
    let sensitive = !dir.join("cafe\u{301}-probe").exists();
    std::fs::remove_file(probe).unwrap();
    if !sensitive {
        eprintln!("skipped: {} ignores normalization", dir.display());
    }
    sensitive
}

/// NFC and NFD spellings side by side (ext4): the synced one stays itself,
/// the other is listed, never uploaded over it.
#[tokio::test(flavor = "multi_thread")]
async fn nfc_and_nfd_side_by_side() {
    let w = world().await;
    let a = w.device("a").await;
    if !normalization_sensitive(a.folder.parent().unwrap()) {
        return;
    }
    a.write("caf\u{e9}.txt", "synced");
    w.converge(&[&a]).await;
    a.write("cafe\u{301}.txt", "other");
    eventually("listed", || async {
        a.notices().iter().any(|n| n.starts_with("cafe\u{301}.txt"))
    })
    .await;
    let remote = w.remote(&w.library).await;
    assert_eq!(remote["caf\u{e9}.txt"].as_deref(), Some(&b"synced"[..]));
}

/// A file another program holds locked (Windows) is never replaced: its
/// download waits, listed, and comes once it's released.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn a_locked_file_waits() {
    use std::os::windows::fs::OpenOptionsExt;
    let w = world().await;
    let a = w.device("a").await;
    let b = w.device("b").await;
    a.write("x.txt", "one");
    w.converge(&[&a, &b]).await;
    let locked = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(b.path("x.txt"))
        .unwrap();
    a.write("x.txt", "two, longer");
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    drop(locked);
    w.converge(&[&a, &b]).await;
    assert_eq!(
        std::fs::read_to_string(b.path("x.txt")).unwrap(),
        "two, longer"
    );
}

/// C9: a folder deleted on the server that still holds something the scan
/// doesn't sync (a symbolic link) stays, and says why.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_folder_left_not_empty_is_listed() {
    let w = world().await;
    let a = w.device("a").await;
    a.write("d/y.txt", "y");
    w.converge(&[&a]).await;
    std::os::unix::fs::symlink("/tmp", a.path("d/link")).unwrap();
    let d = a
        .state()
        .entries(a.pair().id)
        .unwrap()
        .into_iter()
        .find(|e| e.name == "d")
        .unwrap()
        .node;
    w.admin.trash_nodes(&[d], false).await.unwrap();
    eventually("listed", || async {
        a.notices().iter().any(|n| n.starts_with("d: not deleted"))
    })
    .await;
    assert!(!a.path("d/y.txt").exists());
}

/// B6: a pair's folder moved into another pair's is not followed there (one
/// library's files would land in the other): the pair stops.
#[tokio::test(flavor = "multi_thread")]
async fn a_folder_moved_into_another_pair_stops() {
    let w = world().await;
    let a = w.device("a").await;
    a.write("x.txt", "x");
    w.converge(&[&a]).await;
    let notes = w
        .admin
        .create_library(w.organization, "Notes")
        .await
        .unwrap();
    let other = a.folder.parent().unwrap().join("Notes");
    theca::ops::add(
        &a.state(),
        &a.account,
        &notes.id.to_string(),
        Some(other.clone()),
        false,
    )
    .await
    .unwrap();
    a.handle.wake();
    std::fs::rename(&a.folder, other.join("Docs")).unwrap();
    eventually("stopped", || async { a.pair().stopped.is_some() }).await;
    assert!(
        !a.pair().path.starts_with(&other),
        "followed into another pair"
    );
}
