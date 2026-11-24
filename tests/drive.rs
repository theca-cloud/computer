//! Drives (sprint 16) on Linux (FUSE) and Windows (Cloud Files), against
//! the real server: a drive beside a sync device on one library. A drive's
//! rounds work where `Device::backing` says (Linux: its backing folder;
//! Windows: its own folder); opening a placeholder fetches it.
#![cfg(any(target_os = "linux", windows))]

mod common;

use std::collections::BTreeSet;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::sync::Arc;

use common::*;
#[cfg(target_os = "linux")]
use theca::drive::fetch;
use theca::drive::is_placeholder;
#[cfg(target_os = "linux")]
use theca::state::Pair;

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Drives are folders of the state's drives folder, named when added
/// (theca/computer#94): the library's, then with its organization when
/// another drive has the name. Two accounts of one state with the same
/// library; the first one removed and added again takes the plain name back,
/// the second keeps its own.
#[tokio::test(flavor = "multi_thread")]
async fn drives_are_named_in_the_drives_folder() {
    use theca::state::{Mode, State};
    let w = world().await;
    let (dir, state_path, first) = w.logged_in("names").await;
    let state = State::open(&state_path).unwrap();
    let drives = dir.path().join("theca-drives");
    // A second account on the same state, a member of the library.
    let second = w
        .second_account(&state, dir.path(), "second@test.theca", &w.library)
        .await;
    let library = w.library.name.clone();
    let id = w.library.id.to_string();
    let add = async |account: &theca::state::Account| {
        theca::ops::add(&state, account, &id, None, false, Mode::Drive)
            .await
            .unwrap()
    };
    let a = add(&first).await;
    let b = add(&second).await;
    let name = |p: &theca::state::Pair| {
        assert_eq!(
            p.path.parent(),
            Some(drives.as_path()),
            "in the drives folder"
        );
        p.path.file_name().unwrap().to_string_lossy().into_owned()
    };
    assert_eq!(name(&a), library);
    let other = name(&b);
    assert!(
        other != library && other.ends_with(&format!(" - {library}")),
        "{other}"
    );

    // The first removed and added again: the plain name is free again, and
    // the second's is not renamed.
    theca::ops::remove(&state, &a, false).unwrap();
    let again = add(&first).await;
    assert_eq!(name(&again), library);
    let b_now = state.pair(b.id).unwrap().unwrap();
    assert_eq!(name(&b_now), other);
}

/// Two drives of two accounts of one state (the same library in both) are
/// two folders of one sync root, the drives folder (theca/computer#94,
/// Windows): each fetches what is opened of its own; a drive's folder isn't
/// deleted nor renamed, nor a file moved across two drives; a file made at
/// the top stays a file of nobody's; removing one drive leaves the other and
/// the root, the last takes the sync root and the folder. (Run by GitHub's
/// Windows runner only.)
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn drives_are_folders_of_one_sync_root() {
    use theca::state::Mode;
    let w = world().await;
    let notes = w
        .admin
        .create_library(w.organization, "Notes")
        .await
        .unwrap();
    w.upload(&notes, "n.txt", b"notes").await;
    let mut a = w.device_for("a", &w.library, false).await;
    let state = a.state();
    let first = state.accounts().unwrap().remove(0);
    let dir = state.path().unwrap().parent().unwrap().to_owned();
    // Windows takes no sync root in the temp folder (`UNABLE_TO_MASK_PATH`):
    // the drives folder in the user's profile, as a user's is.
    let profile = tempfile::Builder::new()
        .prefix("theca-drive-")
        .tempdir_in(std::env::var("USERPROFILE").unwrap())
        .unwrap();
    own_drives(&state, profile.path());
    let second = w
        .second_account(&state, &dir, "two@test.theca", &notes)
        .await;
    let id = notes.id.to_string();
    let one = theca::ops::add(&state, &first, &id, None, false, Mode::Drive)
        .await
        .unwrap();
    let two = theca::ops::add(&state, &second, &id, None, false, Mode::Drive)
        .await
        .unwrap();
    let drives = one.path.parent().unwrap().to_owned();
    let name = |p: &theca::state::Pair| p.path.file_name().unwrap().to_string_lossy().into_owned();
    let (n1, n2) = (name(&one), name(&two));
    assert_ne!(n1, n2);
    assert!(
        theca::drive::cloud::registered(&state),
        "one sync root, on the drives folder"
    );

    a.restart();
    // Each drive's own file, fetched when opened.
    for p in [&one, &two] {
        eventually("its own file", || async {
            listed(&p.path).await.contains("n.txt")
        })
        .await;
        let file = p.path.join("n.txt");
        let content = in_drive("a file opened", move || std::fs::read_to_string(file)).await;
        assert_eq!(content.unwrap(), "notes");
    }
    assert_eq!(top(&drives).await, BTreeSet::from([n1.clone(), n2.clone()]));

    // A drive's folder isn't the user's to delete nor to rename, and nothing
    // moves from a drive to another.
    let at = |name: &str| drives.join(name);
    // As Explorer does (`move`, `rmdir`): Rust's `rename` and `remove_dir_all`
    // go by POSIX semantics, which Windows doesn't tell the sync root about.
    let classic = |command: &str, args: &[&Path]| {
        let out = std::process::Command::new("cmd")
            .args(["/C", command])
            .args(args)
            .output()
            .unwrap();
        out.status.success()
    };
    assert!(
        !classic("move", &[&at(&n1), &at("renamed")]),
        "a drive was renamed"
    );
    assert!(one.path.exists() && !at("renamed").exists());
    assert!(
        !classic(
            "move",
            &[&one.path.join("n.txt"), &two.path.join("moved.txt")]
        ),
        "a file moved across two drives"
    );
    assert!(one.path.join("n.txt").exists() && !two.path.join("moved.txt").exists());
    // Inside a drive, what a drive does.
    assert_eq!(
        errno("a folder in a drive", {
            let p = one.path.join("sub");
            move || std::fs::create_dir(p)
        })
        .await,
        None
    );
    // A file made at the top can't be refused: it stays, belongs to no drive,
    // and nothing uploads it.
    let loose = at("loose.txt");
    std::fs::write(&loose, "nobody's").unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(loose.exists());
    assert!(!w.remote(&notes).await.contains_key("loose.txt"));

    // Last: `rmdir /S` first deletes what the drive holds.
    assert!(
        !classic("rmdir", &[Path::new("/S"), Path::new("/Q"), &at(&n1)]),
        "a drive's folder was deleted"
    );
    assert!(one.path.exists());

    // One drive removed: its folder goes, the other stays open, the root stays.
    theca::ops::remove(&state, &one, false).unwrap();
    eventually("the first drive withdrawn", || async {
        !one.path.exists() && top(&drives).await.contains(&n2)
    })
    .await;
    assert!(theca::drive::cloud::registered(&state));
    assert!(!listed(&two.path).await.is_empty());
    // The last takes the sync root and the folder.
    std::fs::remove_file(&loose).unwrap();
    theca::ops::remove(&state, &two, false).unwrap();
    // Once the engines have closed the connection: one that stopped on a
    // removal under its feet (theca/computer#113) does so after its retry
    // 30 s later.
    eventually_within(90, "the sync root and the folder taken", || async {
        !theca::drive::cloud::registered(&state) && !drives.exists()
    })
    .await;
    a.stop().await;
}

/// What the drives folder lists, through the mount.
async fn top(drives: &Path) -> BTreeSet<String> {
    let drives = drives.to_owned();
    in_drive("the drives folder listed", move || {
        std::fs::read_dir(&drives)
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    })
    .await
}

/// The error number an operation in the drives folder failed with (none: it
/// worked).
async fn errno(
    what: &'static str,
    call: impl FnOnce() -> std::io::Result<()> + Send + 'static,
) -> Option<i32> {
    in_drive(what, move || {
        call().err().map(|e| e.raw_os_error().unwrap_or(-1))
    })
    .await
}

/// Two drives of two accounts of one state (the same library in both) are
/// two folders of one mount of the drives folder (theca/computer#94): each
/// opens its own files; the top level refuses what isn't Theca's to make,
/// and a move across two drives; removing one leaves the other, and the last
/// takes the mount and the folder with it.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn drives_are_folders_of_one_mount() {
    use theca::state::Mode;
    if !fuse() {
        return;
    }
    // The top folder is read-only: the kernel refuses what changes it, before
    // the mount is asked.
    const EACCES: i32 = 13;
    const EXDEV: i32 = 18;
    let w = world().await;
    let notes = w
        .admin
        .create_library(w.organization, "Notes")
        .await
        .unwrap();
    w.upload(&notes, "n.txt", b"notes").await;
    let mut a = w.device_in_a_child("a", false).await;
    let state = a.state();
    let first = state.accounts().unwrap().remove(0);
    let dir = state.path().unwrap().parent().unwrap().to_owned();
    let second = w
        .second_account(&state, &dir, "two@test.theca", &notes)
        .await;
    let id = notes.id.to_string();
    let one = theca::ops::add(&state, &first, &id, None, false, Mode::Drive)
        .await
        .unwrap();
    let two = theca::ops::add(&state, &second, &id, None, false, Mode::Drive)
        .await
        .unwrap();
    let drives = one.path.parent().unwrap().to_owned();
    assert!(
        !drives.exists(),
        "the drives folder exists while mounted only"
    );
    let name = |p: &theca::state::Pair| p.path.file_name().unwrap().to_string_lossy().into_owned();
    let (n1, n2) = (name(&one), name(&two));
    assert_ne!(n1, n2);

    a.restart();
    eventually("both drives listed", || async {
        top(&drives).await == BTreeSet::from([n1.clone(), n2.clone()])
    })
    .await;
    for p in [&one, &two] {
        eventually("its own files", || async {
            listed(&p.path).await.contains("n.txt")
        })
        .await;
        // Read back through the mount (the same node, in two drives: two inodes).
        let file = p.path.join("n.txt");
        let content = in_drive("a file opened", move || std::fs::read_to_string(file)).await;
        assert_eq!(content.unwrap(), "notes");
    }
    // One mount, on the drives folder: no drive has one of its own.
    assert!(theca::drive::fuse::mounted(&drives));
    assert!(!theca::drive::fuse::mounted(&one.path) && !theca::drive::fuse::mounted(&two.path));

    // The top level is Theca's.
    let at = |name: &str| drives.join(name);
    assert_eq!(
        errno("a file at the top", {
            let p = at("x.txt");
            move || std::fs::File::create(p).map(|_| ())
        })
        .await,
        Some(EACCES)
    );
    assert_eq!(
        errno("a folder at the top", {
            let p = at("x");
            move || std::fs::create_dir(p)
        })
        .await,
        Some(EACCES)
    );
    assert_eq!(
        errno("a drive renamed", {
            let (from, to) = (at(&n1), at("renamed"));
            move || std::fs::rename(from, to)
        })
        .await,
        Some(EACCES)
    );
    assert_eq!(
        errno("a drive deleted", {
            let p = at(&n1);
            move || std::fs::remove_dir(p)
        })
        .await,
        Some(EACCES)
    );
    assert_eq!(
        errno("a move across two drives", {
            let (from, to) = (one.path.join("n.txt"), two.path.join("moved.txt"));
            move || std::fs::rename(from, to)
        })
        .await,
        Some(EXDEV)
    );
    // Inside a drive, anything a drive does.
    assert_eq!(
        errno("a folder in a drive", {
            let p = one.path.join("sub");
            move || std::fs::create_dir(p)
        })
        .await,
        None
    );
    assert_eq!(
        errno("a file renamed in a drive", {
            let (from, to) = (one.path.join("n.txt"), one.path.join("sub/n.txt"));
            move || std::fs::rename(from, to)
        })
        .await,
        None
    );

    // One drive removed: it leaves the list, the other stays open.
    theca::ops::remove(&state, &one, false).unwrap();
    eventually("the first drive withdrawn", || async {
        top(&drives).await == BTreeSet::from([n2.clone()])
    })
    .await;
    // (Its own library's file, where the other drive's rename put it.)
    let still = listed(&two.path).await;
    assert!(
        still.contains("n.txt") || still.contains("sub/n.txt"),
        "{still:?}"
    );
    // The last one takes the mount, and the folder, with it.
    theca::ops::remove(&state, &two, false).unwrap();
    eventually("nothing mounted, no drives folder", || async {
        !theca::drive::fuse::mounted(&drives) && !drives.exists()
    })
    .await;
    a.stop().await;
}

/// A drive made by 1.3.x (a folder of its own outside the drives folder, its
/// own mount) moves into the drives folder at the first start of this
/// version (theca/computer#94): the same drive, its files and an edit not
/// uploaded kept, its old folder gone, said once.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn a_drive_made_by_1_3_moves_into_the_drives_folder() {
    use theca::state::Mode;
    if !fuse() {
        return;
    }
    let w = world().await;
    let notes = w
        .admin
        .create_library(w.organization, "Notes")
        .await
        .unwrap();
    w.upload(&notes, "n.txt", b"notes").await;
    let mut a = w.device_in_a_child("a", false).await;
    let state = a.state();
    let account = state.accounts().unwrap().remove(0);
    let dir = state.path().unwrap().parent().unwrap().to_owned();
    let made = theca::ops::add(
        &state,
        &account,
        &notes.id.to_string(),
        None,
        false,
        Mode::Drive,
    )
    .await
    .unwrap();
    let drives = made.path.parent().unwrap().to_owned();
    // Put back as 1.3.x had it: a folder of its own, outside the drives folder.
    let old = dir.join("elsewhere").join("Notes");
    std::fs::create_dir_all(&old).unwrap();
    let root_file_id = state.pair(made.id).unwrap().unwrap().root_file_id;
    state
        .set_pair_path(made.id, &old, root_file_id.as_deref())
        .unwrap();
    // An edit not uploaded, in the drive's files.
    let backing = theca::drive::backing(&state, made.id).unwrap();
    std::fs::write(backing.join("edit.txt"), "not uploaded").unwrap();

    a.restart();
    eventually("the drive in the drives folder", || async {
        state.pair(made.id).unwrap().unwrap().path == drives.join("Notes")
    })
    .await;
    assert!(!old.exists(), "the old folder is left behind");
    let moved = state.pair(made.id).unwrap().unwrap();
    assert_eq!(moved.library, notes.id, "the same drive");
    let folder = drives.join("Notes");
    eventually("its files in the one mount", || async {
        let listed = listed(&folder).await;
        listed.contains("n.txt") && listed.contains("edit.txt")
    })
    .await;
    eventually("the edit not uploaded went up", || async {
        w.remote(&notes).await.get("edit.txt") == Some(&Some(b"not uploaded".to_vec()))
    })
    .await;
    let said = |a: &Device| {
        a.notices()
            .iter()
            .filter(|n| n.contains("Your drives moved"))
            .count()
    };
    assert_eq!(said(&a), 1, "said once");
    // A second start has nothing to move, and says nothing more.
    a.stop().await;
    a.restart();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(said(&a), 1);
    a.stop().await;
}

/// A drive paused stays listed, as a stopped sync pair's folder stays: what
/// is downloaded opens, a placeholder doesn't (and nothing is fetched for
/// it), a file saved there is uploaded once it resumes.
#[tokio::test(flavor = "multi_thread")]
async fn a_paused_drive_stays_listed() {
    if !fuse() {
        return;
    }
    let w = world().await;
    let s = w.device("sync").await;
    s.write("a.txt", "hello");
    s.write("b.txt", "world");
    w.converge(&[&s]).await;
    let mut d = w.drive("drive").await;
    let (folder, backing) = (d.folder.clone(), d.backing());
    eventually("listed", || async { listed(&folder).await.len() == 2 }).await;
    let opened = |name: &str| {
        let path = folder.join(name);
        in_drive("a file opened", move || std::fs::read_to_string(path))
    };
    assert_eq!(opened("a.txt").await.unwrap(), "hello");

    let (state, id) = (d.state(), d.pair().id);
    state.set_paused(id, true).unwrap();
    // The engine looks every second: once it has, it no longer serves the
    // drive, and a placeholder fails to open (opened before, it would be
    // fetched).
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let refused = opened("b.txt").await.expect_err("a placeholder opened");
    // (FUSE answers EIO; Windows says its own cloud error.)
    #[cfg(target_os = "linux")]
    assert_eq!(refused.raw_os_error(), Some(5), "EIO: {refused}");
    #[cfg(windows)]
    let _ = refused;
    assert!(
        is_placeholder(&backing.join("b.txt")),
        "nothing was fetched"
    );
    assert_eq!(opened("a.txt").await.unwrap(), "hello", "downloaded opens");
    assert!(listed(&folder).await.contains("b.txt"), "still listed");
    let saved = folder.join("saved.txt");
    in_drive("a file saved", move || std::fs::write(saved, "meanwhile"))
        .await
        .unwrap();

    state.set_paused(id, false).unwrap();
    eventually("uploaded on resume", || async {
        w.remote(&w.library).await.get("saved.txt") == Some(&Some(b"meanwhile".to_vec()))
    })
    .await;
    eventually("b.txt opens again", || async {
        opened("b.txt").await.is_ok_and(|c| c == "world")
    })
    .await;
    d.stop().await;
}

/// Everything listed as placeholders; a file fetched when asked, recorded;
/// changed on the server, evicted again; kept (its folder), downloaded; an
/// edit in the drive uploaded.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn a_drive_shows_everything_and_fetches_what_is_opened() {
    if !fuse() {
        return;
    }
    let w = world().await;
    let s = w.device("sync").await;
    s.write("a.txt", "hello");
    s.write("d/b.txt", "inside");
    w.converge(&[&s]).await;
    let d = w.drive("drive").await;
    let backing = d.backing();
    let (a, b) = (backing.join("a.txt"), backing.join("d/b.txt"));
    eventually("everything as placeholders", || async {
        is_placeholder(&a)
            && is_placeholder(&b)
            && std::fs::metadata(&a).is_ok_and(|m| m.len() == 5)
    })
    .await;

    let state = Arc::new(d.state());
    let client = theca::ops::client(&d.account).unwrap();
    let (fetcher, serve) = fetch::service(
        state.clone(),
        client,
        Default::default(),
        Default::default(),
    );
    tokio::spawn(serve);
    let pair = Pair {
        path: backing.clone(),
        ..d.pair()
    };
    let fetched = {
        let (fetcher, pair) = (fetcher.clone(), pair.clone());
        tokio::task::spawn_blocking(move || fetcher.fetch(&pair, Path::new("a.txt")))
    };
    fetched.await.unwrap().unwrap();
    assert_eq!(read(&a), "hello");
    assert!(!is_placeholder(&a));

    s.write("a.txt", "hello again");
    eventually("evicted for the server's change", || async {
        is_placeholder(&a) && std::fs::metadata(&a).is_ok_and(|m| m.len() == 11)
    })
    .await;

    // As `theca keep` does it, from another process than the engine's: the
    // pair's counter tells it.
    theca::ops::keep(&state, &d.folder.join("d"), true).unwrap();
    eventually("a kept folder's file downloaded", || async {
        read(&b) == "inside"
    })
    .await;

    std::fs::write(&b, "edited in the drive").unwrap();
    settle(&b);
    eventually("the drive's edit synced", || async {
        read(&s.path("d/b.txt")) == "edited in the drive"
    })
    .await;
}

/// What the drive's folder lists, through the mount.
async fn listed(folder: &Path) -> BTreeSet<String> {
    let folder = folder.to_owned();
    in_drive("the drive listed", move || {
        theca::local::scan(&folder)
            .map(|s| s.items.into_iter().map(|i| i.path.join("/")).collect())
            .unwrap_or_default()
    })
    .await
}

/// A placeholder renamed over another, opened before a round records it:
/// it opens with its own content, never the one it replaced; and the
/// server ends as the rename says.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn a_placeholder_renamed_over_another_opens_as_itself() {
    if !fuse() {
        return;
    }
    let w = world().await;
    let s = w.device("sync").await;
    s.write("a.txt", "aaa");
    s.write("b.txt", "bbb");
    w.converge(&[&s]).await;
    let d = w.drive("drive").await;
    let folder = d.folder.clone();
    eventually("listed", || async { listed(&folder).await.len() == 2 }).await;
    let (a, b) = (folder.join("a.txt"), folder.join("b.txt"));
    let opened = in_drive("b.txt renamed over a.txt, then opened", move || {
        std::fs::rename(&b, &a).unwrap();
        std::fs::read_to_string(&a).unwrap()
    })
    .await;
    assert_eq!(opened, "bbb");
    eventually("the rename on the server", || async {
        read(&s.path("a.txt")) == "bbb" && !s.path("b.txt").exists()
    })
    .await;
}

/// Through the mount: the library listed, a file opened fetched; a file
/// created, renamed, deleted there reaches the other device.
#[tokio::test(flavor = "multi_thread")]
async fn a_mounted_drive_is_the_library() {
    if !fuse() {
        return;
    }
    let w = world().await;
    let s = w.device("sync").await;
    s.write("a.txt", "hello");
    s.write("d/b.txt", "inside");
    w.converge(&[&s]).await;
    let mut d = w.drive("drive").await;
    let folder = d.folder.clone();
    eventually("the library listed in the drive", || async {
        listed(&folder).await == ["a.txt", "d", "d/b.txt"].map(String::from).into()
    })
    .await;
    let a = folder.join("a.txt");
    assert_eq!(
        in_drive("a.txt opened", move || std::fs::read_to_string(a))
            .await
            .unwrap(),
        "hello"
    );
    assert!(!is_placeholder(&d.backing().join("a.txt")), "fetched");

    let new = folder.join("d/new.txt");
    in_drive("d/new.txt written", move || {
        std::fs::write(&new, "made in the drive")
    })
    .await
    .unwrap();
    eventually("a file made in the drive uploaded", || async {
        read(&s.path("d/new.txt")) == "made in the drive"
    })
    .await;
    // Only read: no new version on the server.
    let fetched = d
        .state()
        .entries(d.pair().id)
        .unwrap()
        .into_iter()
        .find(|e| e.name == "a.txt")
        .unwrap();
    assert_eq!(w.admin.versions(fetched.node).await.unwrap().len(), 1);
    let (from, to) = (folder.join("d/b.txt"), folder.join("renamed.txt"));
    in_drive("d/b.txt renamed", move || std::fs::rename(from, to))
        .await
        .unwrap();
    let gone = folder.join("a.txt");
    in_drive("a.txt deleted", move || std::fs::remove_file(gone))
        .await
        .unwrap();
    eventually("a rename and a delete in the drive synced", || async {
        read(&s.path("renamed.txt")) == "inside"
            && !s.path("d/b.txt").exists()
            && !s.path("a.txt").exists()
    })
    .await;
    d.stop().await;
}

/// Files saved in a drive's folder while Theca wasn't running block its
/// mount, said; moved out, it mounts by itself.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn files_saved_while_stopped_block_the_mount() {
    if !fuse() {
        return;
    }
    let w = world().await;
    let s = w.device("sync").await;
    s.write("a.txt", "hello");
    w.converge(&[&s]).await;
    let mut d = w.drive("drive").await;
    let folder = d.folder.clone();
    let drives = folder.parent().unwrap().to_owned();
    eventually("mounted", || async {
        listed(&folder).await.contains("a.txt")
    })
    .await;
    d.stop().await;
    // The drives folder is Theca's while mounted only: gone with the engine.
    assert!(!drives.exists(), "the drives folder outlives its mount");
    std::fs::create_dir_all(&drives).unwrap();
    std::fs::write(drives.join("saved.txt"), "while stopped").unwrap();
    d.restart();
    eventually("stopped, saying why", || async {
        d.pair().stop.is_some()
            && d.notices()
                .iter()
                .any(|n| n.contains("while Theca wasn't running"))
    })
    .await;
    assert_eq!(
        read(&drives.join("saved.txt")),
        "while stopped",
        "never hidden"
    );
    std::fs::remove_file(drives.join("saved.txt")).unwrap();
    eventually("mounted again", || async {
        listed(&folder).await.contains("a.txt")
    })
    .await;
    assert!(d.pair().stop.is_none());
    d.stop().await;
    assert!(!drives.exists(), "the drives folder outlives its mount");
}

/// Over the drive limit after a fetch, the least recently used leave first,
/// back to placeholders; a file kept (from another process: the pair's
/// counter) is downloaded, and never counted.
#[tokio::test(flavor = "multi_thread")]
async fn the_limit_evicts_the_least_recently_used() {
    if !fuse() {
        return;
    }
    let w = world().await;
    let s = w.device("sync").await;
    for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        s.write(name, &name[..1].repeat(1000));
    }
    w.converge(&[&s]).await;
    let mut d = w.drive("drive").await;
    let (folder, backing) = (d.folder.clone(), d.backing());
    eventually("listed", || async { listed(&folder).await.len() == 4 }).await;
    let open = |name: &str| {
        let path = folder.join(name);
        in_drive("a file opened", move || {
            std::fs::read_to_string(path).unwrap()
        })
    };
    for name in ["a.txt", "b.txt", "c.txt"] {
        assert_eq!(open(name).await.len(), 1000);
    }
    // Their uses, apart (stamps are in seconds).
    let state = d.state();
    let pair = d.pair();
    for (name, used) in [("a.txt", 100), ("b.txt", 200), ("c.txt", 300)] {
        let e = state
            .entry_at(pair.id, pair.root, &[name.to_owned()])
            .unwrap()
            .unwrap();
        state.set_used(pair.id, e.node, used).unwrap();
    }
    state.set_drive_limit(2500).unwrap();
    // A fetch checks the limit: four files, 4000 bytes; the two oldest go.
    assert_eq!(open("d.txt").await.len(), 1000);
    let evicted = |name: &str| is_placeholder(&backing.join(name));
    eventually("the two least recently used evicted", || async {
        evicted("a.txt") && evicted("b.txt")
    })
    .await;
    assert!(!evicted("c.txt") && !evicted("d.txt"));
    assert_eq!(open("a.txt").await.len(), 1000, "fetched again when opened");

    // The rounds that fetch woke ended, and their retry (5 s): only the
    // keep can bring one.
    eventually("up to date", || async { d.pair().activity == "up to date" }).await;
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    theca::ops::keep(&state, &folder.join("b.txt"), true).unwrap();
    eventually("a file kept downloaded", || async { !evicted("b.txt") }).await;
    let usage = theca::drive::usage(&state, &pair).unwrap();
    assert_eq!((usage.kept, usage.waiting), (1, 0));

    // Kept through Explorer (only Windows' pin): never counted either.
    #[cfg(windows)]
    {
        theca::drive::cloud::pin(&folder.join("c.txt"), true).unwrap();
        state.set_drive_limit(1).unwrap();
        eventually("what isn't kept evicted", || async {
            evicted("a.txt") && evicted("d.txt")
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        assert!(!evicted("c.txt") && !evicted("b.txt"));
    }
    d.stop().await;
}

/// A drive removed: an edit not uploaded yet is moved out to
/// `<default folder>/<library> (not uploaded)`, and so is what the server
/// never gets (an ignored folder, a name the scan skips, a link);
/// everything else goes.
#[tokio::test(flavor = "multi_thread")]
async fn removing_a_drive_keeps_only_what_isnt_uploaded() {
    if !fuse() {
        return;
    }
    let w = world().await;
    let s = w.device("sync").await;
    s.write("a.txt", "hello");
    w.converge(&[&s]).await;
    let mut d = w.drive("drive").await;
    let (folder, backing) = (d.folder.clone(), d.backing());
    eventually("listed", || async {
        listed(&folder).await.contains("a.txt")
    })
    .await;
    d.stop().await;
    // Saved, and Theca stopped before it went up.
    std::fs::create_dir(backing.join("new")).unwrap();
    std::fs::write(backing.join("new/edit.txt"), "not uploaded").unwrap();
    std::fs::write(backing.join(".thecaignore"), "build/\n").unwrap();
    std::fs::create_dir(backing.join("build")).unwrap();
    std::fs::write(backing.join("build/out.bin"), "ignored").unwrap();
    std::fs::write(backing.join(".DS_Store"), "skipped").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("a.txt", backing.join("link")).unwrap();
    let state = d.state();
    let kept = theca::ops::remove(&state, &d.pair(), false).unwrap();
    let aside = d.account.default_folder.join("Docs (not uploaded)");
    assert_eq!(read(&aside.join("new/edit.txt")), "not uploaded");
    assert_eq!(read(&aside.join("build/out.bin")), "ignored");
    assert_eq!(read(&aside.join(".DS_Store")), "skipped");
    assert_eq!(read(&aside.join(".thecaignore")), "build/\n");
    #[cfg(unix)]
    assert!(aside.join("link").symlink_metadata().unwrap().is_symlink());
    assert_eq!(kept, if cfg!(unix) { 5 } else { 4 });
    assert!(!backing.exists());
    assert!(state.pairs().unwrap().is_empty());
}

/// `theca free`: a running engine evicts at once (within seconds), a file
/// open right after it closes; with no engine, it goes here and now; an
/// edit not uploaded yet is refused.
#[tokio::test(flavor = "multi_thread")]
async fn free_frees_now() {
    if !fuse() {
        return;
    }
    let w = world().await;
    let s = w.device("sync").await;
    s.write("a.txt", "hello");
    s.write("b.txt", "world");
    w.converge(&[&s]).await;
    let mut d = w.drive("drive").await;
    let (folder, backing) = (d.folder.clone(), d.backing());
    eventually("listed", || async { listed(&folder).await.len() == 2 }).await;
    let evicted = |name: &str| is_placeholder(&backing.join(name));
    for name in ["a.txt", "b.txt"] {
        let path = folder.join(name);
        in_drive("a file opened", move || {
            std::fs::read_to_string(path).unwrap()
        })
        .await;
        assert!(!evicted(name));
    }
    let state = d.state();

    let freed = std::time::Instant::now();
    theca::ops::free(&state, &folder.join("a.txt")).unwrap();
    eventually("freed", || async { evicted("a.txt") }).await;
    assert!(
        freed.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        freed.elapsed()
    );

    // Held open through the mount: it waits for the close.
    let path = folder.join("b.txt");
    let held = in_drive("b.txt opened", move || std::fs::File::open(path).unwrap()).await;
    theca::ops::free(&state, &folder.join("b.txt")).unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(!evicted("b.txt"), "never under an open file");
    in_drive("b.txt closed", move || drop(held)).await;
    eventually("freed once closed", || async { evicted("b.txt") }).await;

    // Stopped: nothing mounted, so it goes here; an edit waits for its upload.
    let a = folder.join("a.txt");
    in_drive("a.txt opened", move || std::fs::read_to_string(a).unwrap()).await;
    d.stop().await;
    theca::ops::free(&state, &folder.join("a.txt")).unwrap();
    assert!(evicted("a.txt"), "at once, with no engine");
    // Saved through the mount before Theca stopped (a placeholder's mark
    // goes with a truncating open): a plain file.
    std::fs::remove_file(backing.join("b.txt")).unwrap();
    std::fs::write(backing.join("b.txt"), "edited, not uploaded").unwrap();
    let refused = theca::ops::free(&state, &folder.join("b.txt")).unwrap_err();
    assert!(
        refused.to_string().contains("isn't uploaded yet"),
        "{refused}"
    );
    assert_eq!(read(&backing.join("b.txt")), "edited, not uploaded");
}

/// A file made in the drive, uploaded, kept, then freed: it stays freed
/// (Windows' pin gone with it), and opens again with its content (a
/// placeholder knowing its node).
#[tokio::test(flavor = "multi_thread")]
async fn a_file_made_here_and_freed_opens_again() {
    if !fuse() {
        return;
    }
    let w = world().await;
    let s = w.device("sync").await;
    s.write("a.txt", "hello");
    w.converge(&[&s]).await;
    let d = w.drive("drive").await;
    let (folder, backing) = (d.folder.clone(), d.backing());
    eventually("listed", || async { listed(&folder).await.len() == 1 }).await;
    let made = folder.join("made.txt");
    let path = made.clone();
    in_drive("made.txt written", move || {
        std::fs::write(path, "made here").unwrap()
    })
    .await;
    let state = d.state();
    let pair = d.pair();
    eventually("uploaded and recorded", || async {
        read(&s.path("made.txt")) == "made here"
            && state
                .entry_at(pair.id, pair.root, &["made.txt".to_owned()])
                .unwrap()
                .is_some()
    })
    .await;
    eventually("up to date", || async { d.pair().activity == "up to date" }).await;
    theca::ops::keep(&state, &made, true).unwrap();
    theca::ops::free(&state, &made).unwrap();
    let evicted = || is_placeholder(&backing.join("made.txt"));
    eventually("freed", || async { evicted() }).await;
    #[cfg(windows)]
    assert!(!theca::drive::cloud::is_pinned(&made), "its pin gone");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(evicted(), "not downloaded again");
    let path = made.clone();
    let opened = in_drive("made.txt opened", move || {
        std::fs::read_to_string(path).unwrap()
    })
    .await;
    assert_eq!(opened, "made here");
    // A drive's folder can't be located elsewhere: it is the system's.
    let refused = theca::ops::locate(&state, &d.pair(), &folder, true).unwrap_err();
    assert!(refused.to_string().contains("is a drive"), "{refused}");
}

/// An open by another program is a use: its `used` moves (every open
/// through the mount on Linux; Windows' open notice, which ignores this
/// process's own).
#[tokio::test(flavor = "multi_thread")]
async fn an_open_is_a_use() {
    if !fuse() {
        return;
    }
    let w = world().await;
    let s = w.device("sync").await;
    s.write("a.txt", "hello");
    w.converge(&[&s]).await;
    let mut d = w.drive("drive").await;
    let folder = d.folder.clone();
    eventually("listed", || async {
        listed(&folder).await.contains("a.txt")
    })
    .await;
    let path = folder.join("a.txt");
    let fetched = path.clone();
    assert_eq!(
        in_drive("a.txt opened", move || read(&fetched)).await,
        "hello"
    );
    let (state, pair) = (d.state(), d.pair());
    let node = state
        .entry_at(pair.id, pair.root, &["a.txt".to_owned()])
        .unwrap()
        .unwrap()
        .node;
    state.set_used(pair.id, node, 100).unwrap();
    let out = in_drive("a.txt read by another program", move || {
        #[cfg(windows)]
        let mut program = {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "type"]);
            c
        };
        #[cfg(not(windows))]
        let mut program = std::process::Command::new("cat");
        program.arg(&path).output().unwrap()
    })
    .await;
    assert_eq!(
        out.stdout,
        b"hello",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    eventually("its use stamped", || async {
        state
            .used(pair.id)
            .unwrap()
            .get(&node)
            .is_some_and(|u| *u > 100)
    })
    .await;
    d.stop().await;
}
