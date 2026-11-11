//! macOS's drive end to end (sprint 16): the app's `theca` from inside an
//! installed Theca.app (`THECA_TEST_MAC_APP`, ci/mac-app.sh), its File
//! Provider extension switched on by script, a sync device on the same
//! library. Read through the drive's folder with dataless files
//! materializing (`THECA_TEST_MATERIALIZE`: a CI shell has that off). Without
//! the app it is skipped, unless `THECA_TEST_DRIVE` says it must run.
#![cfg(target_os = "macos")]

mod common;

use std::path::{Path, PathBuf};
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
    switch_on(&domain_of(&pair), &theca, &state_path);
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
    eventually("the drive's folder recorded", || async { path().is_dir() }).await;
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

    // Removed: macOS names a folder for what it kept even when nothing was
    // left: the removal still succeeds.
    let removed = Command::new(&theca)
        .arg("--state")
        .arg(&state_path)
        .args(["remove", "Docs"])
        .output()
        .unwrap();
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
}
