//! The app, driven like a user through its window (`tauri-driver`, WebKit's
//! WebDriver): signing in through the browser flow, a library synced, an error
//! shown. Linux only, where WebKitGTK has a WebDriver; run under a virtual
//! screen and a D-Bus session (`scripts/ci.sh app`).

#![cfg(all(feature = "gui", target_os = "linux"))]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use fantoccini::{Client, ClientBuilder, Locator};
use sqlx::Connection;

/// One app at a time: it is single-instance, a second launch hands over to
/// the first and exits (its test's window gone).
static ONE_APP: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `tauri-driver`, killed when dropped.
struct Driver(Child);

impl Drop for Driver {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The app started by `tauri-driver` on its own state, and a session on its
/// window.
async fn app(state: &std::path::Path) -> (Driver, Client) {
    let state = state.to_string_lossy();
    app_from(
        std::path::Path::new(env!("CARGO_BIN_EXE_theca")),
        &["--state", &state],
        &[],
    )
    .await
}

/// The app at `application` started by `tauri-driver` with `args`, the
/// driver (and so the app) given `env`, and a session on its window.
async fn app_from(
    application: &std::path::Path,
    args: &[&str],
    env: &[(&str, &std::path::Path)],
) -> (Driver, Client) {
    let port = free_port();
    let driver = Driver(
        Command::new("tauri-driver")
            .args(["--port", &port.to_string()])
            .envs(env.iter().copied())
            .stdout(Stdio::null())
            .spawn()
            .expect("tauri-driver (the CI image has it)"),
    );
    let mut capabilities = serde_json::Map::new();
    capabilities.insert(
        "tauri:options".into(),
        serde_json::json!({
            "application": application,
            "args": args,
        }),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let connector = hyper_util::client::legacy::connect::HttpConnector::new();
        match ClientBuilder::new(connector)
            .capabilities(capabilities.clone())
            .connect(&format!("http://127.0.0.1:{port}"))
            .await
        {
            Ok(client) => return (driver, client),
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(e) => panic!("no session with the app: {e}"),
        }
    }
}

/// An element whose text is `text`, once it shows (within 30 s).
async fn shown(client: &Client, text: &str) -> fantoccini::elements::Element {
    let xpath = format!("//*[contains(normalize-space(text()), \"{text}\")]");
    client
        .wait()
        .at_most(Duration::from_secs(30))
        .for_element(Locator::XPath(&xpath))
        .await
        .unwrap_or_else(|e| panic!("never shown: {text:?} ({e})"))
}

/// The login flow the app started, approved by the admin, as the web app's
/// `/connect` page would (its ID read from the server's database: the app
/// opens it in a browser the test doesn't have).
async fn approve(w: &World) {
    let mut db = sqlx::PgConnection::connect(&w.server.database)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let flow: String = loop {
        let pending: Option<String> =
            sqlx::query_scalar("SELECT uuid::text FROM login_flow WHERE account IS NULL")
                .fetch_optional(&mut db)
                .await
                .unwrap();
        if let Some(flow) = pending {
            break flow;
        }
        assert!(Instant::now() < deadline, "the app never started a login");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let _: () = w
        .admin
        .call(
            reqwest::Method::POST,
            &format!("/api/v1/auth/flows/{flow}/approve"),
            Some(&serde_json::json!({})),
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_in_sync_and_an_error() {
    let _one = ONE_APP.lock().await;
    let w = world().await;
    w.upload(&w.library, "hello.txt", b"hello").await;
    let photos = w
        .admin
        .create_library(w.organization, "Photos")
        .await
        .unwrap();
    let archive = w
        .admin
        .create_library(w.organization, "Archive")
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("theca");
    let (_driver, client) = app(&dir.path().join("state.sqlite")).await;

    // First launch: the welcome form; the server, the default folder; Sign in.
    shown(&client, "Welcome to Theca").await;
    let instance = client.find(Locator::Id("instance")).await.unwrap();
    instance.send_keys(&w.server.url).await.unwrap();
    let field = client.find(Locator::Id("folder")).await.unwrap();
    field.clear().await.unwrap();
    field.send_keys(&folder.to_string_lossy()).await.unwrap();
    client
        .find(Locator::XPath("//button[normalize-space()='Sign in']"))
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    approve(&w).await;

    // Signed in: the first step. Docs synced, Photos a drive (its choice
    // greyed where this computer can't hold one: the CI image), Archive
    // left; libraries added later listed quietly.
    shown(&client, "Choose your libraries").await;
    let choose = |library: uuid::Uuid, choice: &'static str| {
        let client = client.clone();
        async move {
            // The list loads after the step shows.
            client
                .wait()
                .at_most(Duration::from_secs(30))
                .for_element(Locator::Id(&format!("choice-{library}-{choice}")))
                .await
                .unwrap()
        }
    };
    choose(w.library.id, "sync").await.click().await.unwrap();
    let drive = choose(photos.id, "drive").await;
    if theca::drive::unavailable().is_some() {
        assert!(drive.attr("disabled").await.unwrap().is_some());
    } else {
        drive.click().await.unwrap();
    }
    client
        .find(Locator::Id("new-libraries"))
        .await
        .unwrap()
        .select_by_value("silent")
        .await
        .unwrap();
    client
        .find(Locator::XPath("//button[normalize-space()='Add']"))
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    shown(&client, "Up to date").await;
    let state = theca::state::State::open(&dir.path().join("state.sqlite")).unwrap();
    eventually("the choices stored", || async {
        let pairs = state.pairs().unwrap();
        let account = &state.accounts().unwrap()[0];
        account.new_libraries == theca::state::NewLibraries::Silent
            && pairs
                .iter()
                .any(|p| p.library == w.library.id && p.path == folder.join("Docs"))
            && pairs.iter().any(|p| p.library == photos.id) == theca::drive::unavailable().is_none()
    })
    .await;
    assert!(
        !state
            .pairs()
            .unwrap()
            .iter()
            .any(|p| p.library == archive.id)
    );
    let hello = folder.join("Docs").join("hello.txt");
    eventually("hello.txt synced", || async {
        std::fs::read(&hello).ok().as_deref() == Some(&b"hello"[..])
    })
    .await;

    // A name the server refuses: reported in the window.
    std::fs::write(folder.join("Docs").join("bad:name.txt"), "x").unwrap();
    settle(&folder.join("Docs").join("bad:name.txt"));
    shown(&client, "not synced").await;

    // C19: launched again on its state, it hands over to the running app
    // (whose window comes forward) and exits, instead of failing on the lock.
    let again = Command::new(env!("CARGO_BIN_EXE_theca"))
        .arg("--state")
        .arg(dir.path().join("state.sqlite"))
        .status()
        .unwrap();
    assert!(again.success(), "{again:?}");

    client.close().await.unwrap();
}

/// A drive in the window: its line (the Drive badge, its numbers), the drive
/// limit set in the settings, the Drive tick when adding a library (greyed
/// where this computer can't hold one: the CI image has no FUSE).
#[tokio::test(flavor = "multi_thread")]
async fn a_drive_in_the_window() {
    let _one = ONE_APP.lock().await;
    let w = world().await;
    let (dir, state_path, account) = w.logged_in("app").await;
    let state = theca::state::State::open(&state_path).unwrap();
    theca::ops::add(
        &state,
        &account,
        "Docs",
        Some(dir.path().join("theca/Docs")),
        false,
        theca::state::Mode::Drive,
    )
    .await
    .unwrap();
    let other = w
        .admin
        .create_library(w.organization, "Photos")
        .await
        .unwrap();
    let (_driver, client) = app(&state_path).await;

    // Its line.
    shown(&client, "Drive").await;
    shown(&client, "kept").await;

    // The drive limit, from the settings.
    let limit = client.find(Locator::Id("drive-limit")).await.unwrap();
    limit.clear().await.unwrap();
    limit.send_keys("2").await.unwrap();
    client
        .find(Locator::XPath("//button[normalize-space()='Save']"))
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    eventually("the limit saved", || async {
        state.drive_limit().unwrap() == Some(2 * 1024 * 1024 * 1024)
    })
    .await;

    // The Drive tick beside a library to add.
    client
        .find(Locator::XPath(
            "//button[normalize-space()='Add a library…']",
        ))
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    let tick = client
        .wait()
        .at_most(Duration::from_secs(30))
        .for_element(Locator::Id(&format!("drive-{}", other.id)))
        .await
        .unwrap();
    let greyed = tick.attr("disabled").await.unwrap().is_some()
        || tick.attr("data-disabled").await.unwrap().is_some();
    assert_eq!(greyed, theca::drive::unavailable().is_some());

    client.close().await.unwrap();
}

/// The logs' settings in the window: the defaults shown, then a level, days
/// and a size saved (the folder is a native dialog: `theca logs` tests it).
#[tokio::test(flavor = "multi_thread")]
async fn the_logs_in_the_window() {
    let _one = ONE_APP.lock().await;
    let w = world().await;
    let (_dir, state_path, _account) = w.logged_in("app").await;
    let state = theca::state::State::open(&state_path).unwrap();
    let (_driver, client) = app(&state_path).await;
    let level = client
        .wait()
        .at_most(Duration::from_secs(30))
        .for_element(Locator::Id("log-level"))
        .await
        .unwrap();
    assert_eq!(level.prop("value").await.unwrap().as_deref(), Some("info"));
    level.select_by_value("debug").await.unwrap();
    for (id, value) in [("log-days", "30"), ("log-size", "200")] {
        let field = client.find(Locator::Id(id)).await.unwrap();
        field.clear().await.unwrap();
        field.send_keys(value).await.unwrap();
    }
    client
        .find(Locator::XPath(
            "//div[@id='logs-settings']//button[normalize-space()='Save']",
        ))
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    eventually("the logs' settings saved", || async {
        let s = theca::logs::settings(&state).unwrap();
        (s.level.as_str(), s.days, s.max_size) == ("debug", 30, 200_000_000)
    })
    .await;
    client.close().await.unwrap();
}

/// The first step's Later: nothing added, the step gone, the libraries
/// listed; new ones notified (the window's default).
#[tokio::test(flavor = "multi_thread")]
async fn the_first_step_later_adds_nothing() {
    let _one = ONE_APP.lock().await;
    let w = world().await;
    let dir = tempfile::tempdir().unwrap();
    let (_driver, client) = app(&dir.path().join("state.sqlite")).await;
    shown(&client, "Welcome to Theca").await;
    let instance = client.find(Locator::Id("instance")).await.unwrap();
    instance.send_keys(&w.server.url).await.unwrap();
    client
        .find(Locator::XPath("//button[normalize-space()='Sign in']"))
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    approve(&w).await;
    shown(&client, "Choose your libraries").await;
    client
        .find(Locator::XPath("//button[normalize-space()='Later']"))
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    // The account's card, its library offered.
    shown(&client, "Docs").await;
    assert!(client.find(Locator::Id("first-step")).await.is_err());
    let state = theca::state::State::open(&dir.path().join("state.sqlite")).unwrap();
    assert!(state.pairs().unwrap().is_empty());
    assert_eq!(
        state.accounts().unwrap()[0].new_libraries,
        theca::state::NewLibraries::Notify
    );
    client.close().await.unwrap();
}

/// An app that doesn't update itself (the .deb): its version, no update
/// setting, no Check for updates.
#[cfg(not(feature = "self-update"))]
#[tokio::test(flavor = "multi_thread")]
async fn no_update_setting_without_the_feature() {
    let _one = ONE_APP.lock().await;
    // The settings show once an account is there.
    let w = world().await;
    let (_dir, state, _account) = w.logged_in("app").await;
    let (_driver, client) = app(&state).await;
    shown(&client, &format!("Theca {}", env!("CARGO_PKG_VERSION"))).await;
    let section = client.find(Locator::Id("updates-settings")).await.unwrap();
    assert!(section.find(Locator::Id("updates-mode")).await.is_err());
    let buttons = section.find_all(Locator::Css("button")).await.unwrap();
    assert!(buttons.is_empty(), "a button without self-update");
    client.close().await.unwrap();
}

/// Self-update in the app, on Linux (`scripts/ci.sh app` builds it with a
/// key made for the run, `THECA_TEST_UPDATER_KEY` its private half): a copy
/// of this build stands in for the `.AppImage` (`$APPIMAGE` names it), and a
/// release of it (the copy, a marker appended) is served, signed by Tauri's
/// signer as `release.yml` signs.
#[cfg(feature = "self-update")]
mod self_update {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    const MARKER: &[u8] = b"\0the new release";

    struct Release {
        _dir: tempfile::TempDir,
        appimage: PathBuf,
        manifest: String,
    }

    /// Every process still running from the release's `.AppImage` killed
    /// when dropped: a test failing (or an app started where none should
    /// be) leaves nothing to keep the D-Bus session, and CI, waiting.
    impl Drop for Release {
        fn drop(&mut self) {
            for pid in running_from(&self.appimage) {
                let _ = Command::new("kill").args(["-9", &pid]).status();
            }
        }
    }

    /// The processes whose binary is `path` (`/proc/<pid>/exe`).
    fn running_from(path: &Path) -> Vec<String> {
        let Ok(file) = std::fs::metadata(path) else {
            return Vec::new();
        };
        std::fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                std::fs::metadata(e.path().join("exe"))
                    .is_ok_and(|m| m.ino() == file.ino() && m.dev() == file.dev())
            })
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    }

    fn release() -> Release {
        let key = std::env::var("THECA_TEST_UPDATER_KEY")
            .expect("THECA_TEST_UPDATER_KEY: run scripts/ci.sh app");
        let dir = tempfile::tempdir().unwrap();
        let appimage = dir.path().join("Theca.AppImage");
        std::fs::copy(env!("CARGO_BIN_EXE_theca"), &appimage).unwrap();
        let new = dir.path().join("Theca_9.0.0_amd64.AppImage");
        let bytes = [std::fs::read(&appimage).unwrap(), MARKER.to_vec()].concat();
        std::fs::write(&new, &bytes).unwrap();
        let signed = Command::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/ui/node_modules/.bin/tauri"
        ))
        .args(["signer", "sign", "-f", &key, "-p", ""])
        .arg(&new)
        .stdout(Stdio::null())
        .status()
        .unwrap();
        assert!(signed.success(), "tauri signer sign");
        let signature =
            std::fs::read_to_string(dir.path().join("Theca_9.0.0_amd64.AppImage.sig")).unwrap();
        let (base, _) = serve(|base| {
            let manifest = serde_json::json!({
                "version": "9.0.0",
                "platforms": {
                    format!("linux-{}", std::env::consts::ARCH): {
                        "url": format!("{base}/Theca_9.0.0_amd64.AppImage"),
                        "signature": signature,
                    }
                }
            });
            HashMap::from([
                ("/latest.json", manifest.to_string().into_bytes()),
                ("/Theca_9.0.0_amd64.AppImage", bytes),
            ])
        });
        Release {
            _dir: dir,
            appimage,
            manifest: format!("{base}/latest.json"),
        }
    }

    /// The app (or `theca run`) runs on `state`.
    fn taken(state: &Path) -> bool {
        theca::lock(state).is_err()
    }

    fn wait(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !done() {
            assert!(Instant::now() < deadline, "never {what}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// `theca update` from the `.AppImage` as a user runs it: whether it
    /// succeeded, and what it said. Its output goes to a file, and only the
    /// command is waited for (a process it started would hold a pipe open).
    fn update(r: &Release, state: &Path) -> (bool, String) {
        update_from(r, state, Command::new(&r.appimage))
    }

    /// `update` from `command` (the environment it sets kept).
    fn update_from(r: &Release, state: &Path, mut command: Command) -> (bool, String) {
        let out = tempfile::NamedTempFile::new().unwrap();
        // One handle for both (two would each write from the start).
        let file = out.reopen().unwrap();
        let status = command
            .env("APPIMAGE", &r.appimage)
            .arg("--state")
            .arg(state)
            .args(["--manifest", &r.manifest, "update"])
            .stdout(file.try_clone().unwrap())
            .stderr(file)
            .status()
            .unwrap();
        (
            status.success(),
            std::fs::read_to_string(out.path()).unwrap(),
        )
    }

    /// The app open: `theca update` hands over, says so; the app installs
    /// the release and starts again in its place (the same process, now
    /// the new file).
    #[tokio::test(flavor = "multi_thread")]
    async fn theca_update_hands_over_to_the_app() {
        let _one = ONE_APP.lock().await;
        let r = release();
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.sqlite");
        // Its own checks off (they'd read the real releases): only the one
        // `theca update` asks for.
        let off = Command::new(&r.appimage)
            .arg("--state")
            .arg(&state)
            .args(["updates", "off"])
            .output()
            .unwrap();
        assert!(off.status.success());
        let mut app = Command::new(&r.appimage)
            .env("APPIMAGE", &r.appimage)
            .arg("--state")
            .arg(&state)
            .arg("--background")
            .spawn()
            .unwrap();
        wait("started", || taken(&state));
        let (ok, said) = update(&r, &state);
        assert!(ok, "{said}");
        assert!(
            said.contains("The app is installing Theca 9.0.0."),
            "{said}"
        );
        assert!(!said.contains("installing here"), "{said}");
        wait("installed", || {
            std::fs::read(&r.appimage).is_ok_and(|b| b.ends_with(MARKER))
        });
        let exe = format!("/proc/{}/exe", app.id());
        let new = std::fs::metadata(&r.appimage).unwrap().ino();
        wait("started again from the new file", || {
            std::fs::metadata(&exe).is_ok_and(|m| m.ino() == new)
        });
        wait("running again", || taken(&state));
        let _ = app.kill();
        let _ = app.wait();
    }

    /// No app open: `theca update` installs, and starts nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn theca_update_without_the_app_starts_nothing() {
        let _one = ONE_APP.lock().await;
        let r = release();
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.sqlite");
        let (ok, said) = update(&r, &state);
        assert!(ok, "{said}");
        assert!(said.contains("Theca 9.0.0 installed"), "{said}");
        assert!(std::fs::read(&r.appimage).unwrap().ends_with(MARKER));
        std::thread::sleep(Duration::from_secs(2));
        assert!(!taken(&state), "something runs on the state");
        assert!(running_from(&r.appimage).is_empty(), "an app started");
    }

    /// The window's Updates section: its version and mode (changed, stored);
    /// Check for updates offers the release, Install installs it and the
    /// app starts again from the new file.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_window_checks_and_installs() {
        let _one = ONE_APP.lock().await;
        let r = release();
        // The settings show once an account is there.
        let w = world().await;
        let (_dir, state, _account) = w.logged_in("app").await;
        let s = theca::state::State::open(&state).unwrap();
        // Off: no check at start, the button's only.
        theca::update::set_mode(&s, theca::update::Mode::Off).unwrap();
        let state_arg = state.to_string_lossy();
        let (_driver, client) = app_from(
            &r.appimage,
            &["--state", &state_arg, "--manifest", &r.manifest],
            &[("APPIMAGE", &r.appimage)],
        )
        .await;
        shown(&client, &format!("Theca {}", env!("CARGO_PKG_VERSION"))).await;
        let mode = client.find(Locator::Id("updates-mode")).await.unwrap();
        assert_eq!(mode.prop("value").await.unwrap().as_deref(), Some("off"));
        mode.select_by_value("ask").await.unwrap();
        eventually("the mode stored", || async {
            theca::update::mode(&s).unwrap() == theca::update::Mode::Ask
        })
        .await;
        let button = |text: &'static str| {
            let client = client.clone();
            async move {
                client
                    .find(Locator::XPath(&format!(
                        "//div[@id='updates-settings']//button[normalize-space()='{text}']"
                    )))
                    .await
                    .unwrap()
            }
        };
        button("Check for updates").await.click().await.unwrap();
        shown(&client, "Theca 9.0.0 is available").await;
        assert!(
            running_from(&r.appimage).len() == 1,
            "the app alone runs from it"
        );
        button("Install and restart").await.click().await.unwrap();
        wait("installed", || {
            std::fs::read(&r.appimage).is_ok_and(|b| b.ends_with(MARKER))
        });
        // Running from the new file: the old one is gone (replaced), so a
        // process matching its inode is the new release.
        wait("started again from the new file", || {
            !running_from(&r.appimage).is_empty()
        });
        wait("running again", || taken(&state));
    }

    /// `$pid` runs the new release: the same process, now from the new file
    /// (an `exec`), the state taken again.
    fn started_again(r: &Release, pid: u32, state: &Path) {
        wait("installed", || {
            std::fs::read(&r.appimage).is_ok_and(|b| b.ends_with(MARKER))
        });
        let new = std::fs::metadata(&r.appimage).unwrap().ino();
        let exe = format!("/proc/{pid}/exe");
        wait("started again from the new file", || {
            std::fs::metadata(&exe).is_ok_and(|m| m.ino() == new)
        });
        wait("running again", || taken(state));
    }

    /// `theca run` in `auto` (the `.AppImage`'s, as a service runs it):
    /// its check at start installs the release, the engine stops, and the
    /// new one goes on in its place.
    #[tokio::test(flavor = "multi_thread")]
    async fn theca_run_in_auto_installs_and_goes_on() {
        let _one = ONE_APP.lock().await;
        let r = release();
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.sqlite");
        let s = theca::state::State::open(&state).unwrap();
        theca::update::set_mode(&s, theca::update::Mode::Auto).unwrap();
        let mut run = Command::new(&r.appimage)
            .env("APPIMAGE", &r.appimage)
            .arg("--state")
            .arg(&state)
            .args(["--manifest", &r.manifest, "run"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        started_again(&r, run.id(), &state);
        let _ = run.kill();
        let _ = run.wait();
    }

    /// The app in `auto`: its check at start installs the release, and it
    /// starts again by itself.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_app_in_auto_installs_and_restarts() {
        let _one = ONE_APP.lock().await;
        let r = release();
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.sqlite");
        let s = theca::state::State::open(&state).unwrap();
        theca::update::set_mode(&s, theca::update::Mode::Auto).unwrap();
        let mut app = Command::new(&r.appimage)
            .env("APPIMAGE", &r.appimage)
            .arg("--state")
            .arg(&state)
            .args(["--manifest", &r.manifest, "--background"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        started_again(&r, app.id(), &state);
        let _ = app.kill();
        let _ = app.wait();
    }

    /// No display (a server, over ssh), the state taken by `theca run`:
    /// no app to hand over to, `theca update` installs itself, says so.
    #[tokio::test(flavor = "multi_thread")]
    async fn theca_update_without_a_display_installs_itself() {
        let _one = ONE_APP.lock().await;
        let r = release();
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.sqlite");
        let s = theca::state::State::open(&state).unwrap();
        theca::update::set_mode(&s, theca::update::Mode::Off).unwrap();
        let mut run = Command::new(&r.appimage)
            .arg("--state")
            .arg(&state)
            .arg("run")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait("theca run started", || taken(&state));
        let mut headless = Command::new(&r.appimage);
        headless.env_remove("DISPLAY").env_remove("WAYLAND_DISPLAY");
        let (ok, said) = update_from(&r, &state, headless);
        assert!(ok, "{said}");
        assert!(
            said.contains("no app to hand this to: installing here"),
            "{said}"
        );
        assert!(std::fs::read(&r.appimage).unwrap().ends_with(MARKER));
        let _ = run.kill();
        let _ = run.wait();
    }
}
