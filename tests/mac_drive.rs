//! macOS's drive end to end (sprint 16): the app's `theca` from inside an
//! installed Theca.app (`THECA_TEST_MAC_APP`, ci/mac-app.sh), its File
//! Provider extension switched on by script, a sync device on the same
//! library. Read through the drive's folder with dataless files
//! materializing (`THECA_TEST_MATERIALIZE`: a CI shell has that off). Without
//! the app it is skipped, unless `THECA_TEST_DRIVE` says it must run.
#![cfg(target_os = "macos")]

mod common;

use std::path::{Path, PathBuf};
use tokio::sync::Mutex as AsyncMutex;

/// The scenarios take turns: each switches its domain on in fileproviderd's
/// one Domains.plist and restarts it, which a second doing the same at once
/// leaves without an extension (found on GitHub's macOS run: one domain was
/// never asked a thing).
static ONE_DOMAIN_AT_A_TIME: AsyncMutex<()> = AsyncMutex::const_new(());
use std::process::{Child, Command, Output};

use common::*;
use theca::drive::provider::domain_of;
use theca::state::State;

fn app() -> Option<PathBuf> {
    let app = std::env::var_os("THECA_TEST_MAC_APP").map(PathBuf::from);
    let required = std::env::var("THECA_TEST_DRIVE").is_ok_and(|v| !v.is_empty() && v != "0");
    assert!(
        app.is_some() || !required,
        "THECA_TEST_DRIVE: no THECA_TEST_MAC_APP"
    );
    if app.is_none() {
        eprintln!("skipped: no installed Theca.app (THECA_TEST_MAC_APP)");
    }
    app
}

/// `command` run with dataless files materializing.
fn materializing(command: &[&str]) -> Output {
    let helper = std::env::var_os("THECA_TEST_MATERIALIZE").expect("THECA_TEST_MATERIALIZE");
    Command::new(helper).args(command).output().unwrap()
}

/// Listed but not downloaded (`SF_DATALESS`), seen without reading it.
fn dataless(path: &Path) -> Option<bool> {
    let out = materializing(&["stat", "-f", "%Xf", &path.to_string_lossy()]);
    let flags = u32::from_str_radix(String::from_utf8_lossy(&out.stdout).trim(), 16).ok()?;
    Some(flags & 0x4000_0000 != 0)
}

fn read(path: &Path) -> String {
    let out = materializing(&["cat", &path.to_string_lossy()]);
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The domain switched on as the user would in System Settings, by script:
/// its `Enabled` in fileproviderd's Domains.plist, fileproviderd restarted
/// (Experiment 3; private, and only for CI).
fn switch_on(domain: &str, theca: &Path, state: &Path) {
    let plist = PathBuf::from(std::env::var("HOME").unwrap())
        .join("Library/Application Support/FileProvider/rs.theca.computer.drive/Domains.plist");
    for _ in 0..60 {
        if plist.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let key = format!("{domain}.Enabled");
    let set = || {
        Command::new("plutil")
            .args(["-replace", &key, "-bool", "YES"])
            .arg(&plist)
            .status()
            .unwrap()
    };
    // A developer's Mac may refuse the edit: switched on by hand there
    // (`ci/mac/ci.sh --drive` says so), waited for as the app sees it (only
    // the app, with its extension, can ask macOS about its drives).
    if !set().success() {
        for _ in 0..240 {
            let status = Command::new(theca)
                .arg("--state")
                .arg(state)
                .arg("status")
                .output()
                .unwrap();
            if String::from_utf8_lossy(&status.stdout).contains("a drive, switched on") {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        panic!("{} not switched on, by script or by hand", plist.display());
    }
    let _ = Command::new("killall")
        .args(["-9", "fileproviderd"])
        .status();
    set();
}

/// `theca run` from inside the app, stopped when dropped.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The test's drives removed from macOS when dropped, the test failed or
/// not: one left would keep its folder's name, and the next run's drive of
/// that name would be refused.
struct Removed(PathBuf, PathBuf);

impl Drop for Removed {
    fn drop(&mut self) {
        let _ = Command::new(&self.0)
            .arg("--state")
            .arg(&self.1)
            .arg("remove-drives")
            .output();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mac_drive_is_the_library() {
    // This runner is a service, whose downloads are off: it reads the domain.
    theca_fileprovider::host::downloads_on_in_process();
    let _one = ONE_DOMAIN_AT_A_TIME.lock().await;
    let Some(app) = app() else {
        return;
    };
    let theca = app.join("Contents/MacOS/theca");
    let w = world().await;
    let s = w.device("sync").await;
    s.write("a.txt", "hello");
    s.write("d/b.txt", "below");
    w.converge(&[&s]).await;

    let (_dir, state_path, _account) = w.logged_in("mac").await;
    let _removed = Removed(theca.clone(), state_path.clone());
    // Its `theca`s' logs where ci/mac/ci.sh collects them, if it says where.
    if let Some(logs) = std::env::var_os("THECA_TEST_LOGS") {
        let set = Command::new(&theca)
            .arg("--state")
            .arg(&state_path)
            .args(["logs", "--level", "debug", "--path"])
            .arg(logs)
            .output()
            .unwrap();
        assert!(
            set.status.success(),
            "{}",
            String::from_utf8_lossy(&set.stderr)
        );
    }
    let added = Command::new(&theca)
        .arg("--state")
        .arg(&state_path)
        .args(["add", "Docs", "--drive"])
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&added.stdout);
    assert!(
        added.status.success(),
        "{said}{}",
        String::from_utf8_lossy(&added.stderr)
    );
    assert!(said.contains("Switch it on"), "{said}");
    let pair = State::open(&state_path).unwrap().pairs().unwrap().remove(0);
    let domain = domain_of(&State::open(&state_path).unwrap()).unwrap();
    switch_on(&domain, &theca, &state_path);
    let _run = Running(
        Command::new(&theca)
            .arg("--state")
            .arg(&state_path)
            .arg("run")
            .spawn()
            .unwrap(),
    );
    // Where macOS put it, as the engine records it once it is on.
    let recorded = State::open(&state_path).unwrap();
    let path = || recorded.pair(pair.id).unwrap().unwrap().path;
    // The drive's folder is in the domain once the system has listed the domain
    // (as Finder does on opening it): list it while waiting.
    eventually("the drive's folder recorded", || async {
        let path = path();
        if let Some(domain) = path.parent() {
            let _ = std::fs::read_dir(domain).map(|d| d.count());
        }
        path.is_dir()
    })
    .await;
    let folder = path();
    let a = folder.join("a.txt");
    eventually("the library in the drive's folder", || async {
        read(&a) == "hello"
    })
    .await;

    // A folder kept: what is below it downloaded, unread (its own policy,
    // inherited); freed: dataless again.
    let b = folder.join("d/b.txt");
    eventually("a file listed, not downloaded", || async {
        dataless(&b) == Some(true)
    })
    .await;
    let theca_on = |command: &str| {
        let out = Command::new(&theca)
            .arg("--state")
            .arg(&state_path)
            .args([command, &folder.join("d").to_string_lossy()])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "theca {command}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    theca_on("keep");
    eventually("a kept folder's file downloaded", || async {
        dataless(&b) == Some(false)
    })
    .await;
    theca_on("free");
    eventually("a freed folder's file dataless again", || async {
        dataless(&b) == Some(true)
    })
    .await;

    s.write("a.txt", "hello again");
    eventually("the server's change in the drive", || async {
        read(&a) == "hello again"
    })
    .await;

    let made = folder.join("made.txt");
    let wrote = materializing(&[
        "sh",
        "-c",
        &format!("printf 'made on a Mac' > '{}'", made.display()),
    ]);
    assert!(
        wrote.status.success(),
        "{}",
        String::from_utf8_lossy(&wrote.stderr)
    );
    eventually("a file made in the drive uploaded", || async {
        std::fs::read_to_string(s.path("made.txt")).is_ok_and(|c| c == "made on a Mac")
    })
    .await;
    let removed = materializing(&["rm", &a.to_string_lossy()]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    eventually("a delete in the drive synced", || async {
        !s.path("a.txt").exists()
    })
    .await;

    // A second library in the same entry: its own folder of the domain's.
    let photos = w
        .admin
        .create_library(w.organization, "Photos")
        .await
        .unwrap();
    w.upload(&photos, "p.txt", b"photo").await;
    let theca_do = |args: &[&str]| {
        Command::new(&theca)
            .arg("--state")
            .arg(&state_path)
            .args(args)
            .output()
            .unwrap()
    };
    let added = theca_do(&["add", "Photos", "--drive"]);
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let photos_folder = || {
        recorded
            .pairs()
            .unwrap()
            .into_iter()
            .find(|p| p.library == photos.id)
            .map(|p| p.path)
            .unwrap_or_default()
    };
    eventually("the second drive in the same entry", || async {
        read(&photos_folder().join("p.txt")) == "photo"
    })
    .await;
    let drives_folder = folder.parent().unwrap().to_owned();
    assert_eq!(photos_folder().parent(), Some(drives_folder.as_path()));

    // A drive's folder isn't the user's to rename (what it holds is left alone).
    let renamed = materializing(&[
        "mv",
        &folder.to_string_lossy(),
        &drives_folder.join("renamed").to_string_lossy(),
    ]);
    assert!(!renamed.status.success(), "a drive's folder was renamed");
    assert!(folder.exists() && !drives_folder.join("renamed").exists());

    // One drive removed: its folder goes, the other stays readable.
    let removed = theca_do(&["remove", "Docs"]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    eventually("the first drive's folder gone", || async {
        !folder.exists()
    })
    .await;
    assert_eq!(read(&photos_folder().join("p.txt")), "photo");
    // The last one takes the domain with it (macOS names a folder for what it
    // kept even when nothing was left: the removal still succeeds).
    let removed = theca_do(&["remove", "Photos"]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
}

/// An edit the system holds and can't upload (the server unreachable) keeps
/// its drive from being removed, nothing lost (theca/computer#94): the removal
/// says so; once the edit is up, it goes through.
#[tokio::test(flavor = "multi_thread")]
async fn a_mac_drive_with_an_edit_waiting_is_not_removed() {
    // This runner is a service, whose downloads are off: it reads the domain.
    theca_fileprovider::host::downloads_on_in_process();
    let _one = ONE_DOMAIN_AT_A_TIME.lock().await;
    use std::sync::atomic::Ordering;
    let Some(app) = app() else {
        return;
    };
    let theca = app.join("Contents/MacOS/theca");
    let (w, wire) = world_cut().await;
    w.upload(&w.library, "a.txt", b"hello").await;
    let (_dir, state_path, _account) = w.logged_in("mac-waiting").await;
    let _removed = Removed(theca.clone(), state_path.clone());
    let theca_do = |args: &[&str]| {
        Command::new(&theca)
            .arg("--state")
            .arg(&state_path)
            .args(args)
            .output()
            .unwrap()
    };
    let added = theca_do(&["add", "Docs", "--drive"]);
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let pair = State::open(&state_path).unwrap().pairs().unwrap().remove(0);
    let domain = domain_of(&State::open(&state_path).unwrap()).unwrap();
    switch_on(&domain, &theca, &state_path);
    let _run = Running(
        Command::new(&theca)
            .arg("--state")
            .arg(&state_path)
            .arg("run")
            .spawn()
            .unwrap(),
    );
    let recorded = State::open(&state_path).unwrap();
    let path = || recorded.pair(pair.id).unwrap().unwrap().path;
    // The drive's folder is in the domain once the system has listed the domain
    // (as Finder does on opening it): list it while waiting.
    eventually("the drive's folder recorded", || async {
        let path = path();
        if let Some(domain) = path.parent() {
            let _ = std::fs::read_dir(domain).map(|d| d.count());
        }
        path.is_dir()
    })
    .await;
    let folder = path();
    eventually("the library in the drive's folder", || async {
        read(&folder.join("a.txt")) == "hello"
    })
    .await;

    // The server out of reach, an edit made in the drive: waiting.
    wire.budget.store(0, Ordering::SeqCst);
    let wrote = materializing(&[
        "sh",
        "-c",
        &format!("printf 'waiting' > '{}'", folder.join("edit.txt").display()),
    ]);
    assert!(wrote.status.success());
    let refused = theca_do(&["remove", "Docs"]);
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(
        !refused.status.success() && said.contains("aren't uploaded yet"),
        "{said}"
    );
    assert!(
        State::open(&state_path)
            .unwrap()
            .pair(pair.id)
            .unwrap()
            .is_some(),
        "the drive was removed with an edit waiting"
    );

    // Back: the edit goes up, and the removal with it.
    wire.budget.store(usize::MAX, Ordering::SeqCst);
    eventually("the edit uploaded", || async {
        w.remote(&w.library).await.get("edit.txt") == Some(&Some(b"waiting".to_vec()))
    })
    .await;
    let removed = theca_do(&["remove", "Docs"]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
}
