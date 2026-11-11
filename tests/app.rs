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
    let port = free_port();
    let driver = Driver(
        Command::new("tauri-driver")
            .args(["--port", &port.to_string()])
            .stdout(Stdio::null())
            .spawn()
            .expect("tauri-driver (the CI image has it)"),
    );
    let mut capabilities = serde_json::Map::new();
    capabilities.insert(
        "tauri:options".into(),
        serde_json::json!({
            "application": env!("CARGO_BIN_EXE_theca"),
            "args": ["--state", state],
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

    // Signed in: the library (under the auto-sync limit) syncs by itself.
    shown(&client, "Up to date").await;
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
