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
    s.write("c.txt", "third");
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

    // The app and the extension meet in the app group's folder
    // (theca/computer#118), not in the extension's sandbox: the socket the
    // app made, what the extension logged and staged for that fetch.
    let group = PathBuf::from(std::env::var("HOME").unwrap())
        .join("Library/Group Containers")
        .join(theca_fileprovider::protocol::group());
    assert!(group.join("theca.sock").exists(), "{group:?}: no socket");
    assert!(group.join("drive.log").is_file(), "{group:?}: no drive.log");
    assert!(group.join("staging").is_dir(), "{group:?}: no staging");
    let logs = Command::new(&theca)
        .arg("--state")
        .arg(&state_path)
        .arg("logs")
        .output()
        .unwrap();
    let logs = String::from_utf8_lossy(&logs.stdout);
    assert!(
        logs.contains(&group.join("drive.log").to_string_lossy().into_owned()),
        "theca logs names the extension's log in the group: {logs}"
    );

    // A process that serves no drive (a sync device, here in the test's own
    // process: an engine with no drive) leaves the extension's socket to the
    // one that does: bound last, it took the socket and answered "no drive"
    // (found on a runner: the drive's folder never appeared).
    let c = folder.join("c.txt");
    assert_eq!(dataless(&c), Some(true), "c.txt not downloaded yet");
    let _bystander = w.device("bystander").await;
    eventually(
        "a file opens through the extension with a bystander",
        || async { read(&c) == "third" },
    )
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
    // The system takes a moment to know the edit is up (until then `remove`
    // says "try again once online", -2005).
    eventually("the drive removed once the edit is known up", || async {
        theca_do(&["remove", "Docs"]).status.success()
    })
    .await;
}

/// A Mac drive reads ahead the folders below what is listed (theca/computer
/// #124): started, the app lists three levels below the drive's folder
/// itself; one more level is listed only when the system asks about it (a
/// click into a folder it hasn't read). Seen through the app's log: the
/// `List` it answers about folders this test never touched. Level four
/// isn't read ahead (the app's own reads make the system ask about the three
/// levels, which start nothing: one click would walk the whole library).
#[tokio::test(flavor = "multi_thread")]
async fn a_mac_drive_reads_ahead_three_levels_below_what_is_listed() {
    theca_fileprovider::host::downloads_on_in_process();
    let _one = ONE_DOMAIN_AT_A_TIME.lock().await;
    let Some(app) = app() else {
        return;
    };
    let theca = app.join("Contents/MacOS/theca");
    let w = world().await;
    let s = w.device("sync").await;
    s.write("p/q/r/s/t.txt", "level four");
    s.write("p/q/r/s/u/v.txt", "level five");
    w.converge(&[&s]).await;
    let (_dir, state_path, _account) = w.logged_in("mac").await;
    let _removed = Removed(theca.clone(), state_path.clone());
    let logs = tempfile::tempdir().unwrap();
    let set = Command::new(&theca)
        .arg("--state")
        .arg(&state_path)
        .args(["logs", "--level", "debug", "--path"])
        .arg(logs.path())
        .output()
        .unwrap();
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stderr)
    );
    let added = Command::new(&theca)
        .arg("--state")
        .arg(&state_path)
        .args(["add", "Docs", "--drive"])
        .output()
        .unwrap();
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
    eventually("the drive's folder recorded", || async {
        let path = path();
        if let Some(domain) = path.parent() {
            let _ = std::fs::read_dir(domain).map(|d| d.count());
        }
        path.is_dir()
    })
    .await;
    let folder = path();
    let uuid = recorded.pair_uuid(pair.id).unwrap().unwrap();
    // The node below the library's root, by names, once the app has them.
    let node = |names: &[&str]| {
        names.iter().try_fold(pair.root, |parent, name| {
            recorded
                .children(pair.id, parent)
                .ok()?
                .into_iter()
                .find(|e| e.name == *name)
                .map(|e| e.node)
        })
    };
    // How many times the app answered a `List` of that folder.
    let listed = |names: &[&str]| {
        let Some(node) = node(names) else {
            return 0;
        };
        let wanted = format!("container: \"{uuid}/{node}\"");
        std::fs::read_dir(logs.path())
            .map(|dir| {
                dir.flatten()
                    .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                    .map(|text| text.matches(&wanted).count())
                    .sum::<usize>()
            })
            .unwrap_or_default()
    };

    // Three levels below the drive's folder, read by the app: p, p/q, p/q/r.
    eventually("three levels read ahead at the start", || async {
        listed(&["p", "q", "r"]) > 0
    })
    .await;
    // Not a fourth, nor the ones the app's own reads made the system ask
    // about: left a while to be sure.
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;
    assert_eq!(
        listed(&["p", "q", "r", "s"]),
        0,
        "level four was read ahead: the app's own reads started more"
    );

    // A click into level four: the system asks, and the folders below it are
    // read ahead (p/q/r/s/u), which this test never touches.
    let four = folder.join("p/q/r/s");
    let ls = materializing(&["ls", &four.to_string_lossy()]);
    assert!(
        ls.status.success(),
        "{}",
        String::from_utf8_lossy(&ls.stderr)
    );
    eventually("the folder below a listed one read ahead", || async {
        listed(&["p", "q", "r", "s", "u"]) > 0
    })
    .await;
}

/// `run` stopped as a user quits (SIGTERM, which `theca run` answers by
/// stopping its engine), and waited for.
fn stop(mut run: Running) {
    let pid = run.0.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    for _ in 0..120 {
        if let Some(status) = run.0.try_wait().unwrap() {
            assert!(status.success(), "theca run stopped with {status}");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    panic!("theca run didn't stop within a minute");
}

/// A Mac's drives are cut from their extension while Theca isn't running
/// and served again when it starts (theca/computer#120): quit, the drive
/// stays in Finder, its downloaded files read, one not downloaded fails at
/// once without the extension being asked (connected, it asks the app, which
/// isn't there, again and again); started, it opens. The drive's folder keeps
/// its name through both (a disconnect made with the wrong display name
/// renamed it `Theca-<id>` for good).
#[tokio::test(flavor = "multi_thread")]
async fn a_mac_drive_is_cut_off_while_theca_is_stopped_and_back_when_it_starts() {
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
    // One nobody opens while Theca is stopped (theca/computer#129).
    s.write("c.txt", "untouched");
    w.converge(&[&s]).await;
    let (_dir, state_path, _account) = w.logged_in("mac").await;
    let _removed = Removed(theca.clone(), state_path.clone());
    let added = Command::new(&theca)
        .arg("--state")
        .arg(&state_path)
        .args(["add", "Docs", "--drive"])
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let pair = State::open(&state_path).unwrap().pairs().unwrap().remove(0);
    let domain = domain_of(&State::open(&state_path).unwrap()).unwrap();
    switch_on(&domain, &theca, &state_path);
    let start = || {
        Running(
            Command::new(&theca)
                .arg("--state")
                .arg(&state_path)
                .arg("run")
                .spawn()
                .unwrap(),
        )
    };
    let recorded = State::open(&state_path).unwrap();
    let path = || recorded.pair(pair.id).unwrap().unwrap().path;
    let listed = || {
        let path = path();
        if let Some(domain) = path.parent() {
            let _ = std::fs::read_dir(domain).map(|d| d.count());
        }
        path
    };

    let run = start();
    eventually("the drive's folder recorded", || async {
        listed().is_dir()
    })
    .await;
    let folder = path();
    let (a, b) = (folder.join("a.txt"), folder.join("d/b.txt"));
    eventually("a.txt downloaded by reading it", || async {
        read(&a) == "hello"
    })
    .await;
    eventually("b.txt listed, not downloaded", || async {
        dataless(&b) == Some(true)
    })
    .await;

    // The extension's log, its lines saying the app didn't answer.
    let group = PathBuf::from(std::env::var("HOME").unwrap())
        .join("Library/Group Containers")
        .join(theca_fileprovider::protocol::group());
    let unanswered = || {
        std::fs::read_to_string(group.join("drive.log"))
            .unwrap_or_default()
            .matches("the app didn't answer")
            .count()
    };

    // What CloudStorage lists: a disconnect made with the wrong display name
    // renamed the domain's folder `Theca-<id>` for good.
    let storage = || {
        let dir = PathBuf::from(std::env::var("HOME").unwrap()).join("Library/CloudStorage");
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    };
    let entries = storage();
    assert!(
        entries.iter().any(|n| folder.starts_with(
            PathBuf::from(std::env::var("HOME").unwrap())
                .join("Library/CloudStorage")
                .join(n)
        )),
        "the drive's folder is in a CloudStorage entry: {entries:?}"
    );

    stop(run);
    let before = unanswered();
    let started = std::time::Instant::now();
    let failed = materializing(&["cat", &b.to_string_lossy()]);
    assert!(
        !failed.status.success(),
        "a file not downloaded opened with Theca stopped: {}",
        String::from_utf8_lossy(&failed.stdout)
    );
    // The disconnect's own error (errno 60), not another failure.
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("Operation timed out"),
        "the open failed another way: {}",
        String::from_utf8_lossy(&failed.stderr)
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(25),
        "the open failed after {:?}: the extension was asked, not cut off",
        started.elapsed()
    );
    assert_eq!(unanswered(), before, "the extension asked an app not there");
    assert_eq!(read(&a), "hello", "a downloaded file reads");
    assert!(
        listed().is_dir() && path() == folder,
        "the drive's folder moved or went"
    );
    assert_eq!(
        storage(),
        entries,
        "a disconnect renamed or moved the domain's folder"
    );

    // Started again: served, and the same folder. A file not opened while
    // Theca was stopped opens; the one that failed then is told apart from it
    // (the system may hold off on an item whose open failed: #129).
    let _run = start();
    let c = folder.join("c.txt");
    eventually("a file not downloaded opens again", || async {
        read(&c) == "untouched"
    })
    .await;
    eventually("the file that failed opens again", || async {
        read(&b) == "below"
    })
    .await;
    assert!(
        path() == folder && folder.is_dir(),
        "the drive's folder moved or went"
    );
    assert_eq!(
        storage(),
        entries,
        "a reconnect renamed or moved the domain's folder"
    );
}
