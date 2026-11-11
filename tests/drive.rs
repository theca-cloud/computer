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

/// The drive's backend here (Linux: `/dev/fuse` and `fusermount3`; Windows:
/// always): the scenarios are skipped without it, unless `THECA_TEST_DRIVE`
/// says it must be there (GitHub's runners).
fn fuse() -> bool {
    #[cfg(target_os = "linux")]
    let here = Path::new("/dev/fuse").exists() && theca::drive::fuse::available();
    #[cfg(windows)]
    let here = true;
    let required = std::env::var("THECA_TEST_DRIVE").is_ok_and(|v| !v.is_empty() && v != "0");
    assert!(here || !required, "THECA_TEST_DRIVE: no FUSE here");
    if !here {
        eprintln!("skipped: no FUSE here");
    }
    here
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
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

    let folder = state
        .entry_at(pair.id, pair.root, &["d".to_owned()])
        .unwrap()
        .unwrap();
    state.set_kept(pair.id, folder.node, true).unwrap();
    d.handle.wake();
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

/// `f` on a blocking thread: the mount is served by this same process.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

/// What the drive's folder lists, through the mount.
async fn listed(folder: &Path) -> BTreeSet<String> {
    let folder = folder.to_owned();
    blocking(move || {
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
    let opened = blocking(move || {
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
        blocking(move || std::fs::read_to_string(a)).await.unwrap(),
        "hello"
    );
    assert!(!is_placeholder(&d.backing().join("a.txt")), "fetched");

    let new = folder.join("d/new.txt");
    blocking(move || std::fs::write(&new, "made in the drive"))
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
    blocking(move || std::fs::rename(from, to)).await.unwrap();
    let gone = folder.join("a.txt");
    blocking(move || std::fs::remove_file(gone)).await.unwrap();
    eventually("a rename and a delete in the drive synced", || async {
        read(&s.path("renamed.txt")) == "inside"
            && !s.path("d/b.txt").exists()
            && !s.path("a.txt").exists()
    })
    .await;
    // Unmounted before the test's folders go: deleting them through the
    // mount would wait on this very process.
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
    eventually("mounted", || async {
        listed(&folder).await.contains("a.txt")
    })
    .await;
    d.stop().await;
    assert!(
        listed(&folder).await.is_empty(),
        "unmounted with the engine"
    );
    std::fs::write(folder.join("saved.txt"), "while stopped").unwrap();
    d.restart();
    eventually("stopped, saying why", || async {
        d.pair().stop.is_some()
            && d.notices()
                .iter()
                .any(|n| n.contains("while Theca wasn't running"))
    })
    .await;
    assert_eq!(
        read(&folder.join("saved.txt")),
        "while stopped",
        "never hidden"
    );
    std::fs::remove_file(folder.join("saved.txt")).unwrap();
    eventually("mounted again", || async {
        listed(&folder).await.contains("a.txt")
    })
    .await;
    assert!(d.pair().stop.is_none());
    d.stop().await;
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
        blocking(move || std::fs::read_to_string(path).unwrap())
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
        blocking(move || std::fs::read_to_string(path).unwrap()).await;
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

    // Held open (on a thread, through the mount): it waits for the close.
    let (close, closed) = std::sync::mpsc::channel::<()>();
    let path = folder.join("b.txt");
    let holder = std::thread::spawn(move || {
        let file = std::fs::File::open(path).unwrap();
        closed.recv().unwrap();
        drop(file);
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    theca::ops::free(&state, &folder.join("b.txt")).unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(!evicted("b.txt"), "never under an open file");
    close.send(()).unwrap();
    blocking(move || holder.join().unwrap()).await;
    eventually("freed once closed", || async { evicted("b.txt") }).await;

    // Stopped: nothing mounted, so it goes here; an edit waits for its upload.
    let a = folder.join("a.txt");
    blocking(move || std::fs::read_to_string(a).unwrap()).await;
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
    blocking(move || std::fs::write(path, "made here").unwrap()).await;
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
    let opened = blocking(move || std::fs::read_to_string(path).unwrap()).await;
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
    assert_eq!(blocking(move || read(&fetched)).await, "hello");
    let (state, pair) = (d.state(), d.pair());
    let node = state
        .entry_at(pair.id, pair.root, &["a.txt".to_owned()])
        .unwrap()
        .unwrap()
        .node;
    state.set_used(pair.id, node, 100).unwrap();
    let out = blocking(move || {
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
