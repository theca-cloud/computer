//! The app: a tray and one window, the engine in the same process (ADR 0004;
//! the sprint 09's GUI). The page is a webview: it acts through the commands
//! below and follows the engine through the `update` events (`state::Update`,
//! forwarded as they're published).

use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use tauri_plugin_opener::OpenerExt;
use theca_common::api;
use tokio::sync::{broadcast, watch};
use uuid::Uuid;

use crate::engine::{self, Handle};
use crate::ops;
use crate::state::{Mode, NewLibraries, Notice, State, Update};

/// What the app keeps.
struct App {
    state_path: PathBuf,
    engine: Handle,
    stop: watch::Sender<bool>,
    /// The pairs whose stop was notified (once per stop).
    notified: Mutex<std::collections::HashSet<i64>>,
    /// The engine's task, while it runs: Quit waits for it to stop.
    running: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    /// The tray and the menu items it updates, when the desktop has one
    /// (stock GNOME doesn't).
    tray: Mutex<Option<Tray>>,
    /// The newer release found and not installed (`ask`, `auto` that
    /// couldn't, a check asked for): offered in the window and the tray.
    #[cfg(feature = "self-update")]
    offered: Mutex<Option<Waiting>>,
    /// An update installed: how to start it once the engine has stopped.
    #[cfg(feature = "self-update")]
    restart: Mutex<Option<Restart>>,
    /// Held while an update installs: one at a time, wherever it was asked
    /// (the schedule, the button, the tray).
    #[cfg(feature = "self-update")]
    installing: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// Where the releases are (`update::MANIFEST`, or `--manifest`'s).
    #[cfg(feature = "self-update")]
    manifest_url: String,
}

/// A release offered, and why it wasn't installed by itself, if it was
/// tried (`auto`).
#[cfg(feature = "self-update")]
#[derive(Clone)]
struct Waiting {
    manifest: crate::update::Manifest,
    why: Option<String>,
}

/// The window's Updates section: this version, and in a build that updates
/// itself, the mode and the release waiting.
#[derive(Serialize, Clone)]
struct Updates {
    version: &'static str,
    self_update: bool,
    mode: Option<&'static str>,
    offered: Option<OfferedView>,
}

#[derive(Serialize, Clone)]
struct OfferedView {
    version: String,
    notes: String,
    why: Option<String>,
}

/// How the app starts again after an update.
#[cfg(feature = "self-update")]
enum Restart {
    /// The `.AppImage` our code replaced, in this process's place.
    Exec(crate::update::Installed),
    /// The `.app` Tauri's updater replaced (macOS; Windows' installer starts
    /// the app itself).
    Tauri,
}

/// Kept for the app's life: the engine runs alone on this state.
struct Lock(
    #[expect(dead_code, reason = "held, never read: its drop releases the lock")] std::fs::File,
);

/// Runs the app until it quits. `background`: started at login, the window
/// stays closed.
pub fn run(
    state_path: PathBuf,
    background: bool,
    manifest_url: Option<String>,
) -> Result<(), tauri::Error> {
    #[cfg(not(feature = "self-update"))]
    let _ = manifest_url;
    let engine = Handle::default();
    let (stop, stopped) = watch::channel(false);
    let builder = tauri::Builder::default();
    // The key handed over from here (`tauri.conf.json` has an empty one).
    #[cfg(feature = "self-update")]
    let builder = builder.plugin(
        tauri_plugin_updater::Builder::new()
            .pubkey(crate::update::PUBLIC_KEY)
            .build(),
    );
    let app = builder
        // Launched again: the running app's window comes forward; by
        // `theca update`, the app updates (`hand_over`).
        .plugin(tauri_plugin_single_instance::init(|app, args, _| {
            #[cfg(feature = "self-update")]
            if let Some(manifest_url) = update_asked(&args) {
                return update_now(app, manifest_url);
            }
            let _ = args;
            open_window(app)
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--background"]),
        ))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(App {
            state_path: state_path.clone(),
            engine: engine.clone(),
            stop,
            notified: Mutex::default(),
            running: Mutex::new(None),
            tray: Mutex::new(None),
            #[cfg(feature = "self-update")]
            offered: Mutex::new(None),
            #[cfg(feature = "self-update")]
            restart: Mutex::new(None),
            #[cfg(feature = "self-update")]
            installing: std::sync::Arc::default(),
            #[cfg(feature = "self-update")]
            manifest_url: manifest_url.unwrap_or_else(|| crate::update::MANIFEST.to_owned()),
        })
        .invoke_handler(tauri::generate_handler![
            status,
            libraries,
            first_step,
            proposed_folder,
            login,
            add,
            remove,
            set_paused,
            locate,
            undo,
            dismiss,
            set_account,
            remove_account,
            drive_support,
            kept,
            keep,
            free,
            drive_limit,
            set_drive_limit,
            logs,
            set_logs,
            open_logs,
            updates,
            set_updates,
            check_updates,
            install_update,
            quit,
        ])
        .setup(move |app| {
            // Taken here, once the single-instance plugin had its turn: a
            // second launch hands over to the running app (its window comes
            // forward) instead of failing on the lock.
            app.manage(Lock(crate::lock(&state_path)?));
            let handle = app.handle().clone();
            #[cfg(feature = "self-update")]
            keep_updated(&handle, stopped.clone());
            let running = tauri::async_runtime::spawn(async move {
                if let Err(e) = engine::run(state_path, engine, stopped).await {
                    tracing::error!("the engine stopped: {e}");
                }
            });
            *app.state::<App>().running.lock().expect("running") = Some(running);
            forward_updates(&handle);
            match tray(&handle) {
                Ok(tray) => *app.state::<App>().tray.lock().expect("tray") = Some(tray),
                Err(e) => tracing::info!("no tray ({e}): closing the window asks"),
            }
            refresh_tray(&handle);
            notify_stops(&handle);
            let first_launch = state(&handle).accounts().map_or(true, |a| a.is_empty());
            if !background || first_launch {
                open_window(&handle);
            } else {
                in_dock(&handle, false);
            }
            Ok(())
        })
        .build(tauri::generate_context!())?;
    app.run(|app, event| match event {
        // The window closing doesn't quit: the engine runs on (the tray, or
        // launching the app again, brings the window back). Only Quit does,
        // with an exit code.
        RunEvent::ExitRequested {
            code: None, api, ..
        } => api.prevent_exit(),
        // Quit: the engine stops first (its transfers cancelled, what finished
        // recorded, within seconds), then the app exits.
        RunEvent::ExitRequested {
            code: Some(code),
            api,
            ..
        } => {
            let running = app.state::<App>().running.lock().expect("running").take();
            if let Some(running) = running {
                api.prevent_exit();
                let _ = app.state::<App>().stop.send(true);
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = running.await;
                    app.exit(code);
                });
            }
        }
        RunEvent::Exit => {
            let _ = app.state::<App>().stop.send(true);
            #[cfg(feature = "self-update")]
            match app.state::<App>().restart.lock().expect("restart").take() {
                Some(Restart::Exec(installed)) => {
                    tracing::error!("the update didn't start: {}", installed.restart());
                }
                Some(Restart::Tauri) => tauri::process::restart(&app.env()),
                None => {}
            }
        }
        _ => {}
    });
    Ok(())
}

/// The app's checks (`update::schedule`) as the state's mode says: a release
/// found is notified; installed (`auto`), the app quits as Quit does (the
/// engine stopped first) and starts again (`RunEvent::Exit`).
#[cfg(feature = "self-update")]
fn keep_updated(app: &AppHandle, stopped: watch::Receiver<bool>) {
    use crate::update::{self, Checked};
    use std::ops::ControlFlow;
    let app = app.clone();
    let tick = move || {
        let app = app.clone();
        async move {
            let to = app.clone();
            let url = app.state::<App>().manifest_url.clone();
            let checked = update::tick(&state(&app), &url, |manifest, servers| {
                install(to, manifest, servers)
            })
            .await;
            match checked {
                Checked::Installed(manifest) => {
                    start_installed(&app, &manifest);
                    ControlFlow::Break(())
                }
                Checked::Offered { manifest, why } => {
                    offer(&app, &manifest, why.as_ref());
                    ControlFlow::Continue(())
                }
                _ => ControlFlow::Continue(()),
            }
        }
    };
    tauri::async_runtime::spawn(update::schedule(tick, stopped));
}

/// An update installed, and how the app starts it: the `.AppImage` by our
/// code, the `.app` and the installer by Tauri's updater.
#[cfg(feature = "self-update")]
async fn install(
    app: AppHandle,
    manifest: crate::update::Manifest,
    servers: Vec<(String, Option<String>)>,
) -> Result<(), crate::update::UpdateError> {
    use crate::update::{self, Installed, UpdateError};
    let installing = app.state::<App>().installing.clone();
    let Ok(_one) = installing.try_lock_owned() else {
        return Err(UpdateError::Busy);
    };
    // One installed, waiting for the app to start it: not again.
    if app
        .state::<App>()
        .restart
        .lock()
        .expect("restart")
        .is_some()
    {
        return Err(UpdateError::Busy);
    }
    let restart = match Installed::this() {
        Some(installed) => {
            update::install(&manifest, update::PUBLIC_KEY, &servers, &installed).await?;
            Restart::Exec(installed)
        }
        None if cfg!(any(target_os = "macos", windows)) => {
            let url = app.state::<App>().manifest_url.clone();
            let stop = app.state::<App>().stop.clone();
            tauri_install(&app, &url, &manifest, &servers, Some(stop)).await?;
            Restart::Tauri
        }
        None => return Err(UpdateError::NotInstalled),
    };
    *app.state::<App>().restart.lock().expect("restart") = Some(restart);
    Ok(())
}

/// Tauri's updater, on the manifest at `url` and the same key
/// (`min_server` checked here: Tauri doesn't know it). On Windows the
/// installer closes the app and starts it again: the engine is told to stop
/// first (`stop`).
#[cfg(feature = "self-update")]
async fn tauri_install<R: tauri::Runtime>(
    app: &AppHandle<R>,
    url: &str,
    manifest: &crate::update::Manifest,
    servers: &[(String, Option<String>)],
    stop: Option<watch::Sender<bool>>,
) -> Result<(), crate::update::UpdateError> {
    use crate::update::UpdateError;
    use tauri_plugin_updater::UpdaterExt;
    manifest.check_servers(servers)?;
    let error = |e: tauri_plugin_updater::Error| UpdateError::Updater(e.to_string());
    let endpoint = url
        .parse()
        .map_err(|e| UpdateError::Updater(format!("{url}: {e}")))?;
    let updater = app
        .updater_builder()
        .endpoints(vec![endpoint])
        .map_err(error)?
        .on_before_exit(move || {
            if let Some(stop) = &stop {
                let _ = stop.send(true);
            }
        })
        .build()
        .map_err(error)?;
    let found = updater.check().await.map_err(error)?;
    let found = found.ok_or_else(|| UpdateError::Updater("no newer release".into()))?;
    found
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(error)
}

/// Whether a release not installed, for `why`, is offered: not while another
/// install is under way (`Busy`), which installs it.
#[cfg(feature = "self-update")]
fn worth_offering(why: Option<&crate::update::UpdateError>) -> bool {
    !matches!(why, Some(crate::update::UpdateError::Busy))
}

/// An update installed: the app quits as Quit does (the engine stopped
/// first), then starts it (`RunEvent::Exit`).
#[cfg(feature = "self-update")]
fn start_installed(app: &AppHandle, release: &crate::update::Manifest) {
    tracing::info!(version = release.version, "installed: starting it");
    app.exit(0);
}

/// `theca update`'s arguments, forwarded by the single-instance plugin: the
/// manifest it reads (`--manifest`'s, else the app's), when it is one.
#[cfg(feature = "self-update")]
fn update_asked(args: &[String]) -> Option<Option<String>> {
    let mut words = args.iter().skip(1);
    let mut manifest = None;
    let mut update = false;
    while let Some(word) = words.next() {
        match word.as_str() {
            "--manifest" => manifest = words.next().cloned(),
            "--state" => {
                words.next();
            }
            "update" => update = true,
            w => {
                if let Some(m) = w.strip_prefix("--manifest=") {
                    manifest = Some(m.to_owned());
                }
            }
        }
    }
    update.then_some(manifest)
}

/// `theca update` handed over: a newer release installed at once, whatever
/// the mode, and the app started again; one that can't be is notified.
#[cfg(feature = "self-update")]
fn update_now(app: &AppHandle, manifest_url: Option<String>) {
    use crate::update;
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let url = manifest_url.unwrap_or_else(|| app.state::<App>().manifest_url.clone());
        let release = match update::check(&url).await {
            Ok(Some(release)) => release,
            Ok(None) => return,
            Err(e) => return tracing::warn!("theca update: {e}"),
        };
        let servers = update::servers(&state(&app)).await;
        match install(app.clone(), release.clone(), servers).await {
            Ok(()) => start_installed(&app, &release),
            Err(e) => offer(&app, &release, Some(&e)),
        }
    });
}

/// A display the app could be on: on Linux, X11's or Wayland's (without
/// one, GTK can't start, and a Tauri app panics); elsewhere always.
#[cfg(feature = "self-update")]
pub fn has_display() -> bool {
    !cfg!(target_os = "linux")
        || display_in(
            std::env::var_os("DISPLAY"),
            std::env::var_os("WAYLAND_DISPLAY"),
        )
}

/// `DISPLAY` and `WAYLAND_DISPLAY` name a display: either set, not empty.
#[cfg(feature = "self-update")]
fn display_in(display: Option<std::ffi::OsString>, wayland: Option<std::ffi::OsString>) -> bool {
    [display, wayland]
        .iter()
        .any(|v| v.as_ref().is_some_and(|d| !d.is_empty()))
}

/// `theca update` while the state is taken: its arguments handed over to
/// the running app by the single-instance plugin, which ends this process.
/// Returns when no app answered (`theca run` holds the state).
#[cfg(feature = "self-update")]
pub fn hand_over() -> Result<(), tauri::Error> {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|_, _, _| {}))
        .build(tauri::generate_context!())?;
    drop(app);
    Ok(())
}

/// `theca update` with no app open, on macOS and Windows: Tauri's updater in
/// an app without a window, which starts nothing (Windows' installer starts
/// the app again by itself).
#[cfg(feature = "self-update")]
pub async fn install_now(
    url: &str,
    manifest: &crate::update::Manifest,
    servers: &[(String, Option<String>)],
) -> Result<(), crate::update::UpdateError> {
    use crate::update::UpdateError;
    let app = tauri::Builder::default()
        .plugin(
            tauri_plugin_updater::Builder::new()
                .pubkey(crate::update::PUBLIC_KEY)
                .build(),
        )
        .build(tauri::generate_context!())
        .map_err(|e| UpdateError::Updater(e.to_string()))?;
    tauri_install(app.handle(), url, manifest, servers, None).await
}

/// A newer release offered: kept for the window and the tray, notified
/// once a version (`ask`, or `auto` that couldn't install it, saying why).
#[cfg(feature = "self-update")]
fn offer(
    app: &AppHandle,
    manifest: &crate::update::Manifest,
    why: Option<&crate::update::UpdateError>,
) {
    use tauri_plugin_notification::NotificationExt;
    if !worth_offering(why) {
        return;
    }
    let waiting = Waiting {
        manifest: manifest.clone(),
        why: why.map(ToString::to_string),
    };
    let known = app
        .state::<App>()
        .offered
        .lock()
        .expect("offered")
        .replace(waiting)
        .is_some_and(|w| w.manifest.version == manifest.version);
    let _ = app.emit("updates", updates(app.clone()));
    refresh_tray(app);
    if known {
        return;
    }
    let body = match why {
        Some(why) => format!("It couldn't be installed: {why}"),
        None => "Open Theca to install it.".to_owned(),
    };
    let _ = app
        .notification()
        .builder()
        .title(format!("Theca {} is available", manifest.version))
        .body(body)
        .show();
}

#[tauri::command]
fn updates(app: AppHandle) -> Updates {
    #[cfg(feature = "self-update")]
    {
        let offered = app.state::<App>().offered.lock().expect("offered").clone();
        Updates {
            version: env!("CARGO_PKG_VERSION"),
            self_update: true,
            mode: crate::update::mode(&state(&app)).ok().map(|m| m.as_str()),
            offered: offered.map(|w| OfferedView {
                notes: w.manifest.notes_url(),
                version: w.manifest.version,
                why: w.why,
            }),
        }
    }
    #[cfg(not(feature = "self-update"))]
    {
        let _ = app;
        Updates {
            version: env!("CARGO_PKG_VERSION"),
            self_update: false,
            mode: None,
            offered: None,
        }
    }
}

#[tauri::command]
fn set_updates(app: AppHandle, mode: String) -> Answer<()> {
    #[cfg(feature = "self-update")]
    {
        let mode = crate::update::Mode::parse(&mode)?;
        crate::update::set_mode(&state(&app), mode).map_err(err)
    }
    #[cfg(not(feature = "self-update"))]
    {
        let _ = (app, mode);
        Err(crate::update::NOT_SELF_UPDATING.to_owned())
    }
}

/// Check for updates: the manifest asked now, whatever the mode; a newer
/// release offered (never installed from here: Install does).
#[tauri::command]
async fn check_updates(app: AppHandle) -> Answer<Updates> {
    #[cfg(feature = "self-update")]
    {
        let url = app.state::<App>().manifest_url.clone();
        match crate::update::check(&url).await.map_err(err)? {
            Some(manifest) => offer(&app, &manifest, None),
            None => {
                app.state::<App>().offered.lock().expect("offered").take();
                refresh_tray(&app);
            }
        }
        Ok(updates(app))
    }
    #[cfg(not(feature = "self-update"))]
    {
        let _ = app;
        Err(crate::update::NOT_SELF_UPDATING.to_owned())
    }
}

/// Install: the release offered installed, and the app started again.
#[tauri::command]
async fn install_update(app: AppHandle) -> Answer<()> {
    #[cfg(feature = "self-update")]
    {
        let waiting = app.state::<App>().offered.lock().expect("offered").clone();
        let Some(waiting) = waiting else {
            return Err("no update to install: check for updates first".to_owned());
        };
        let servers = crate::update::servers(&state(&app)).await;
        match install(app.clone(), waiting.manifest.clone(), servers).await {
            Ok(()) => {
                start_installed(&app, &waiting.manifest);
                Ok(())
            }
            Err(e) => {
                offer(&app, &waiting.manifest, Some(&e));
                Err(e.to_string())
            }
        }
    }
    #[cfg(not(feature = "self-update"))]
    {
        let _ = app;
        Err(crate::update::NOT_SELF_UPDATING.to_owned())
    }
}

/// The state, publishing its changes to the window.
fn state(app: &AppHandle) -> State {
    let a = app.state::<App>();
    State::open(&a.state_path)
        .expect("the state database opened at startup")
        .with_updates(a.engine.updates.clone())
}

/// Every update to the page (`update` events), and to the tray's icon. Fallen
/// behind (the channel's `Lagged`), the page reloads the whole status.
fn forward_updates(app: &AppHandle) {
    let mut updates = app.state::<App>().engine.updates.subscribe();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            match updates.recv().await {
                Ok(update) => {
                    if let Update::NewLibrary { name, .. } = &update {
                        notify_new_library(&app, name);
                    }
                    if !matches!(update, Update::Offered { .. } | Update::NewLibrary { .. }) {
                        refresh_tray(&app);
                        notify_stops(&app);
                    }
                    let _ = app.emit("update", update);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    refresh_tray(&app);
                    notify_stops(&app);
                    let _ = app.emit("update", serde_json::json!({ "kind": "lagged" }));
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

/// macOS: in the Dock and Cmd+Tab while the window is open, the tray's only
/// otherwise (an "accessory" app).
fn in_dock(app: &AppHandle, shown: bool) {
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(if shown {
        tauri::ActivationPolicy::Regular
    } else {
        tauri::ActivationPolicy::Accessory
    });
    #[cfg(not(target_os = "macos"))]
    let _ = (app, shown);
}

fn open_window(app: &AppHandle) {
    in_dock(app, true);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
        .title("Theca")
        .inner_size(760.0, 640.0)
        .min_inner_size(520.0, 420.0)
        .build();
    let window = match window {
        Ok(window) => window,
        Err(e) => {
            tracing::error!("the window: {e}");
            return;
        }
    };
    let app = app.clone();
    window.clone().on_window_event(move |event| {
        if let WindowEvent::Destroyed = event {
            in_dock(&app, false);
        }
        // Closing destroys the window (the webview's memory goes); without a
        // tray, nothing would bring it back but launching the app again: ask.
        if let WindowEvent::CloseRequested { api, .. } = event
            && app.state::<App>().tray.lock().expect("tray").is_none()
        {
            api.prevent_close();
            let (app, window) = (app.clone(), window.clone());
            app.dialog()
                .message("Keep syncing in the background? Open Theca again from your apps to see this window.")
                .title("Quit Theca?")
                .buttons(MessageDialogButtons::OkCancelCustom(
                    "Keep syncing".into(),
                    "Quit".into(),
                ))
                .show(move |keep| {
                    if keep {
                        let _ = window.destroy();
                    } else {
                        app.exit(0);
                    }
                });
        }
    });
}

struct Tray {
    icon: TrayIcon,
    summary: MenuItem<tauri::Wry>,
    pause: MenuItem<tauri::Wry>,
    /// What it shows: set again only when it changes (setting the same
    /// icon redraws it, a blink in the menu bar).
    shown: Option<(Overall, String, bool)>,
}

/// The tray: the overall state, Open, Pause all or Resume, Quit.
fn tray(app: &AppHandle) -> tauri::Result<Tray> {
    let summary = MenuItem::with_id(app, "summary", "Theca", false, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "Open Theca", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause", "Pause all", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Theca", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[&summary, &separator, &open, &pause, &separator, &quit],
    )?;
    #[cfg(feature = "self-update")]
    menu.insert(
        &MenuItem::with_id(app, "check", "Check for updates…", true, None::<&str>)?,
        4,
    )?;
    let icon = TrayIconBuilder::with_id("theca")
        .icon(tray_icon(Overall::Idle))
        .icon_as_template(cfg!(target_os = "macos"))
        .tooltip("Theca")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => open_window(app),
            "pause" => {
                let state = state(app);
                let all_paused = state
                    .pairs()
                    .is_ok_and(|p| !p.is_empty() && p.iter().all(|p| p.paused));
                if let Ok(pairs) = state.pairs() {
                    for p in pairs {
                        let _ = state.set_paused(p.id, !all_paused);
                    }
                }
                app.state::<App>().engine.wake();
            }
            "quit" => app.exit(0),
            // Its summary says "Update to X.Y.Z" while one waits: installed.
            #[cfg(feature = "self-update")]
            "summary" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = install_update(app).await {
                        tracing::warn!("the update from the tray: {e}");
                    }
                });
            }
            #[cfg(feature = "self-update")]
            "check" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    checked_from_tray(&app, check_updates(app.clone()).await);
                });
            }
            _ => {}
        })
        .build(app)?;
    Ok(Tray {
        icon,
        summary,
        pause,
        shown: None,
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Overall {
    Idle,
    Syncing,
    Attention,
    Paused,
}

fn tray_icon(overall: Overall) -> tauri::image::Image<'static> {
    // macOS: black template images it recolors for the menu bar; elsewhere the
    // mark's teal.
    let bytes: &[u8] = match (overall, cfg!(target_os = "macos")) {
        (Overall::Idle, true) => include_bytes!("../icons/tray/tray-idle-template.png"),
        (Overall::Syncing, true) => include_bytes!("../icons/tray/tray-syncing-template.png"),
        (Overall::Attention, true) => include_bytes!("../icons/tray/tray-attention-template.png"),
        (Overall::Paused, true) => include_bytes!("../icons/tray/tray-paused-template.png"),
        (Overall::Idle, false) => include_bytes!("../icons/tray/tray-idle.png"),
        (Overall::Syncing, false) => include_bytes!("../icons/tray/tray-syncing.png"),
        (Overall::Attention, false) => include_bytes!("../icons/tray/tray-attention.png"),
        (Overall::Paused, false) => include_bytes!("../icons/tray/tray-paused.png"),
    };
    tauri::image::Image::from_bytes(bytes).expect("a bundled PNG")
}

/// An OS notification for each pair that stops syncing (stopped, or its
/// account's engine stopped on an error), and at launch for one still
/// stopped: its files stop syncing, and a tray's icon is easy to miss, or
/// missing (stock GNOME). Once until it syncs again: an engine failing every
/// 30 s says it once.
fn notify_stops(app: &AppHandle) {
    use tauri_plugin_notification::NotificationExt;
    let Ok(pairs) = state(app).pairs() else {
        return;
    };
    let a = app.state::<App>();
    let mut notified = a.notified.lock().expect("notified");
    notified.retain(|id| {
        pairs
            .iter()
            .any(|p| p.id == *id && !p.paused && p.activity != "up to date")
    });
    for p in pairs.iter().filter(|p| !p.paused) {
        let Some(why) = p.not_syncing() else { continue };
        if !notified.insert(p.id) {
            continue;
        }
        let path = p.path.display().to_string();
        let what = if p.stop.is_none() {
            "Theca tries again; the files stay."
        } else if p.stopped_for_its_folder() {
            "Its files stay; it syncs again once the folder is back, or open Theca to locate it."
        } else {
            "Its files stay."
        };
        let body = if why.contains(&path) {
            format!("{why}. {what}")
        } else {
            format!("{path}: {why}. {what}")
        };
        let _ = app
            .notification()
            .builder()
            .title(format!("{} stopped syncing", p.library_name))
            .body(body)
            .show();
    }
}

/// A library new to an account (`notify`): an OS notification, nothing to
/// answer; the window lists it.
fn notify_new_library(app: &AppHandle, name: &str) {
    use tauri_plugin_notification::NotificationExt;
    let _ = app
        .notification()
        .builder()
        .title(format!("A new library: {name}"))
        .body("Open Theca to sync it or make it a drive.")
        .show();
}

/// The tray's icon and summary: attention first (an account signed out, a
/// pair stopped), then an update waiting, then syncing, then paused; and
/// whether clicking it installs (it names the update).
fn summary(
    accounts: &[crate::state::Account],
    pairs: &[crate::state::Pair],
    waiting: Option<&str>,
) -> (Overall, String, bool) {
    if accounts.iter().any(|a| a.signed_out) {
        (
            Overall::Attention,
            "Signed out: open Theca".to_owned(),
            false,
        )
    } else if pairs.iter().any(|p| p.stop.is_some()) {
        (
            Overall::Attention,
            "A folder stopped syncing".to_owned(),
            false,
        )
    } else if let Some(version) = waiting {
        (Overall::Idle, format!("Update to {version}"), true)
    } else if pairs
        .iter()
        .any(|p| !p.paused && p.activity != "up to date" && p.activity != "waiting")
    {
        (Overall::Syncing, "Syncing…".to_owned(), false)
    } else if !pairs.is_empty() && pairs.iter().all(|p| p.paused) {
        (Overall::Paused, "Paused".to_owned(), false)
    } else if pairs.is_empty() {
        (Overall::Idle, "Nothing synced yet".to_owned(), false)
    } else {
        (Overall::Idle, "Up to date".to_owned(), false)
    }
}

/// Check for updates… in the tray: what it found, said by a notification.
#[cfg(feature = "self-update")]
fn checked_from_tray(app: &AppHandle, checked: Answer<Updates>) {
    use tauri_plugin_notification::NotificationExt;
    let (title, body) = match checked {
        // A release found was notified by `offer`.
        Ok(Updates {
            offered: Some(_), ..
        }) => return,
        Ok(_) => (
            "Theca is up to date".to_owned(),
            format!("{} is the latest.", env!("CARGO_PKG_VERSION")),
        ),
        Err(e) => ("No update check".to_owned(), e),
    };
    let _ = app.notification().builder().title(title).body(body).show();
}

/// The tray's icon and summary from the state (`summary`).
fn refresh_tray(app: &AppHandle) {
    let a = app.state::<App>();
    let mut tray = a.tray.lock().expect("tray");
    let Some(tray) = tray.as_mut() else {
        return;
    };
    let state = state(app);
    let (Ok(accounts), Ok(pairs)) = (state.accounts(), state.pairs()) else {
        return;
    };
    #[cfg(feature = "self-update")]
    let waiting = a
        .offered
        .lock()
        .expect("offered")
        .as_ref()
        .map(|w| w.manifest.version.clone());
    #[cfg(not(feature = "self-update"))]
    let waiting: Option<String> = None;
    let (overall, summary, installs) = summary(&accounts, &pairs, waiting.as_deref());
    let paused = !pairs.is_empty() && pairs.iter().all(|p| p.paused);
    let now = (overall, summary, paused);
    if tray.shown.as_ref() == Some(&now) {
        return;
    }
    let _ = tray.icon.set_icon(Some(tray_icon(overall)));
    let _ = tray.icon.set_icon_as_template(cfg!(target_os = "macos"));
    let _ = tray.icon.set_tooltip(Some(format!("Theca: {}", now.1)));
    let _ = tray.summary.set_text(&now.1);
    // Clickable while it names the update (installs it), only then.
    let _ = tray.summary.set_enabled(installs);
    let _ = tray
        .pause
        .set_text(if paused { "Resume" } else { "Pause all" });
    tray.shown = Some(now);
}

/// A command's result: the page shows the error's text.
type Answer<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// An account as the page sees it (never its app password).
#[derive(Serialize)]
struct AccountView {
    id: i64,
    instance: String,
    email: String,
    device: String,
    default_folder: PathBuf,
    new_libraries: NewLibraries,
    /// New libraries up to it sync (`sync`); 0: no limit.
    auto_sync_limit: i64,
    signed_out: bool,
}

#[derive(Serialize)]
struct PairView {
    id: i64,
    account: i64,
    library: Uuid,
    library_name: String,
    path: PathBuf,
    paused: bool,
    /// Why it stopped, as the user is told.
    stopped: Option<String>,
    /// Stopped over its folder: the user locates it, or tries again.
    folder_stop: bool,
    writable: bool,
    activity: String,
    /// The account's engine stopped on it, until the pair's next activity.
    error: Option<String>,
    synced_at: Option<chrono::DateTime<chrono::Utc>>,
    /// A drive's: none for a sync pair.
    drive: Option<DriveView>,
}

/// A drive's numbers, as each OS can tell them.
#[derive(Serialize)]
struct DriveView {
    /// Bytes downloaded (Linux, Windows).
    downloaded: Option<u64>,
    kept: usize,
    /// Files not uploaded yet (Linux, Windows).
    waiting: Option<usize>,
    /// macOS added it switched off: the user switches it on.
    switch_on: bool,
}

fn drive_view(state: &State, p: &crate::state::Pair) -> Option<DriveView> {
    if p.mode != Mode::Drive {
        return None;
    }
    let kept = state.kept(p.id).map_or(0, |k| k.len());
    #[cfg(any(target_os = "linux", windows))]
    let view = crate::drive::usage(state, p).ok().map(|u| DriveView {
        downloaded: Some(u.downloaded),
        kept: u.kept,
        waiting: Some(u.waiting),
        switch_on: false,
    });
    #[cfg(target_os = "macos")]
    let view = Some(DriveView {
        downloaded: None,
        kept,
        waiting: None,
        switch_on: theca_fileprovider::host::enabled(&crate::drive::provider::domain_of(p))
            == Ok(Some(false)),
    });
    view.or(Some(DriveView {
        downloaded: None,
        kept,
        waiting: None,
        switch_on: false,
    }))
}

#[derive(Serialize)]
struct NoticeView {
    id: i64,
    pair: Option<i64>,
    at: chrono::DateTime<chrono::Utc>,
    level: String,
    path: Option<String>,
    message: String,
    undo: bool,
}

impl From<Notice> for NoticeView {
    fn from(n: Notice) -> Self {
        Self {
            id: n.id,
            pair: n.pair,
            at: n.at,
            level: n.level,
            path: n.path,
            message: n.message,
            undo: n.undo.is_some(),
        }
    }
}

/// Everything the page shows, at once (on opening, and after `lagged`).
#[derive(Serialize)]
struct Status {
    accounts: Vec<AccountView>,
    pairs: Vec<PairView>,
    notices: Vec<NoticeView>,
    /// Per account (its ID, a string as a JSON key), the libraries offered.
    offered: std::collections::HashMap<i64, Vec<crate::state::Offer>>,
}

#[tauri::command]
fn status(app: AppHandle) -> Answer<Status> {
    let state = state(&app);
    Ok(Status {
        accounts: state
            .accounts()
            .map_err(err)?
            .into_iter()
            .map(|a| AccountView {
                id: a.id,
                instance: a.instance,
                email: a.email,
                device: a.device,
                default_folder: a.default_folder,
                new_libraries: a.new_libraries,
                auto_sync_limit: a.auto_sync_limit,
                signed_out: a.signed_out,
            })
            .collect(),
        pairs: state
            .pairs()
            .map_err(err)?
            .into_iter()
            .map(|p| PairView {
                drive: drive_view(&state, &p),
                folder_stop: p.stopped_for_its_folder(),
                id: p.id,
                account: p.account,
                library: p.library,
                library_name: p.library_name,
                path: p.path,
                paused: p.paused,
                stopped: p.stop.as_ref().map(|s| s.reason().to_owned()),
                writable: p.writable,
                activity: p.activity,
                error: p.error,
                synced_at: p.synced_at,
            })
            .collect(),
        notices: state
            .notices()
            .map_err(err)?
            .into_iter()
            .map(Into::into)
            .collect(),
        offered: app
            .state::<App>()
            .engine
            .offered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
    })
}

#[derive(Serialize)]
struct LibraryView {
    id: Uuid,
    name: String,
    size: Option<i64>,
    files: Option<i64>,
    writable: bool,
    /// Synced by this pair.
    pair: Option<i64>,
}

/// The account's libraries, synced or not, with their sizes (from the server).
#[tauri::command]
async fn libraries(app: AppHandle, account: i64) -> Answer<Vec<LibraryView>> {
    let state = state(&app);
    let account = state
        .account(account)
        .map_err(err)?
        .ok_or("no such account")?;
    Ok(ops::libraries(&state, &account)
        .await
        .map_err(err)?
        .into_iter()
        .map(|l| LibraryView {
            id: l.library.id,
            name: l.library.name,
            size: l.size,
            files: l.files,
            writable: l.library.writable,
            pair: l.pair.map(|p| p.id),
        })
        .collect())
}

/// A library of the first step: not synced, with its size and its folder by
/// default under the parent chosen.
#[derive(Serialize)]
struct Proposed {
    id: Uuid,
    name: String,
    size: Option<i64>,
    path: PathBuf,
}

/// The first step's libraries (those not synced), each with its folder by
/// default under `parent`.
#[tauri::command]
async fn first_step(app: AppHandle, account: i64, parent: PathBuf) -> Answer<Vec<Proposed>> {
    let state = state(&app);
    let account = state
        .account(account)
        .map_err(err)?
        .ok_or("no such account")?;
    Ok(ops::proposed_paths(&state, &account, &parent)
        .await
        .map_err(err)?
        .into_iter()
        .map(|(l, path)| Proposed {
            id: l.library.id,
            name: l.library.name,
            size: l.size,
            path,
        })
        .collect())
}

/// The default folder a new account would get, and this device's name.
#[derive(Serialize)]
struct Proposal {
    folder: PathBuf,
    device: String,
}

#[tauri::command]
fn proposed_folder(app: AppHandle, instance: String) -> Answer<Proposal> {
    Ok(Proposal {
        folder: ops::proposed_folder(&state(&app), &instance).map_err(err)?,
        device: ops::device_name(),
    })
}

/// Starts the browser login: the page shows the code; the result comes as a
/// `login` event (the account, or why it failed).
#[tauri::command]
async fn login(
    app: AppHandle,
    instance: String,
    device: String,
    folder: Option<PathBuf>,
) -> Answer<api::LoginFlow> {
    let (shown, flow) = tokio::sync::oneshot::channel();
    let background = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = state(&background);
        // New here: the page shows its first step (an account signed in
        // again keeps its choices).
        let before: Vec<i64> = state
            .accounts()
            .map_or_else(|_| Vec::new(), |a| a.iter().map(|a| a.id).collect());
        // Nothing added: the first step chooses; libraries new later notified.
        let result = ops::login(
            &state,
            &instance,
            &device,
            folder,
            None,
            true,
            None,
            NewLibraries::Notify,
            |flow| {
                let _ = shown.send(flow.clone());
            },
        )
        .await;
        let outcome = match result {
            Ok(account) => {
                background.state::<App>().engine.wake();
                serde_json::json!({ "account": account.id, "fresh": !before.contains(&account.id) })
            }
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        };
        let _ = background.emit("login", outcome);
    });
    // The login failing before a flow exists (no server there) drops `shown`.
    let flow = flow
        .await
        .map_err(|_| "no Theca server answered at that address".to_owned())?;
    let _ = app.opener().open_url(&flow.url, None::<&str>);
    Ok(flow)
}

#[tauri::command]
async fn add(
    app: AppHandle,
    account: i64,
    library: Uuid,
    path: Option<PathBuf>,
    merge: bool,
    drive: bool,
) -> Answer<Added> {
    let state = state(&app);
    let account = state
        .account(account)
        .map_err(err)?
        .ok_or("no such account")?;
    match ops::add(
        &state,
        &account,
        &library.to_string(),
        path,
        merge,
        if drive { Mode::Drive } else { Mode::Sync },
    )
    .await
    {
        Ok(pair) => {
            app.state::<App>().engine.wake();
            Ok(Added::Pair(pair.id))
        }
        Err(ops::OpError::Holds { files, .. }) => Ok(Added::Holds(files)),
        Err(e) => Err(err(e)),
    }
}

/// What `add` did: the pair, or nothing yet: the folder holds files (how
/// many), and the merge is asked first.
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Added {
    Pair(i64),
    Holds(usize),
}

/// Why this computer can't hold a drive (the Drive tick greyed), if it can't.
#[tauri::command]
fn drive_support() -> Option<String> {
    crate::drive::unavailable()
}

/// What a drive keeps on this computer, by path below its folder.
#[tauri::command]
fn kept(app: AppHandle, pair: i64) -> Answer<Vec<PathBuf>> {
    let state = state(&app);
    let pair = state.pair(pair).map_err(err)?.ok_or("no such drive")?;
    crate::drive::kept_paths(&state, &pair).map_err(err)
}

/// A drive's file or folder kept on this computer (`theca keep`).
#[tauri::command]
fn keep(app: AppHandle, path: PathBuf) -> Answer<()> {
    let state = state(&app);
    ops::keep(&state, &path, true).map_err(err)?;
    Ok(())
}

/// A drive's file or folder freed now (`theca free`).
#[tauri::command]
fn free(app: AppHandle, path: PathBuf) -> Answer<()> {
    let state = state(&app);
    ops::free(&state, &path).map_err(err)?;
    Ok(())
}

/// The drive limit, and whether it is the default.
#[derive(Serialize)]
struct Limit {
    bytes: u64,
    default: bool,
}

#[tauri::command]
fn drive_limit(app: AppHandle) -> Answer<Limit> {
    let state = state(&app);
    Ok(Limit {
        bytes: crate::drive::limit(&state).map_err(err)?,
        default: state.drive_limit().map_err(err)?.is_none(),
    })
}

#[tauri::command]
fn set_drive_limit(app: AppHandle, bytes: u64) -> Answer<()> {
    state(&app).set_drive_limit(bytes).map_err(err)
}

/// The logs' settings (each applies at the next start).
#[tauri::command]
fn logs(app: AppHandle) -> Answer<crate::logs::Settings> {
    crate::logs::settings(&state(&app)).map_err(err)
}

/// The four set at once, as the settings section saves them; `folder` none:
/// the default one.
#[tauri::command]
fn set_logs(
    app: AppHandle,
    level: String,
    days: u32,
    max_size: u64,
    folder: Option<PathBuf>,
) -> Answer<()> {
    use crate::logs::Change;
    let state = state(&app);
    let level = crate::logs::level(&level)?;
    for change in [
        Change::Level(Some(level)),
        Change::Days(Some(days)),
        Change::MaxSize(Some(max_size)),
        Change::Folder(folder),
    ] {
        crate::logs::set(&state, change).map_err(err)?;
    }
    Ok(())
}

/// The logs' folder, in the file manager.
#[tauri::command]
fn open_logs(app: AppHandle) -> Answer<()> {
    let folder = crate::logs::settings(&state(&app))
        .map_err(err)?
        .folder
        .ok_or("no logs folder")?;
    app.opener()
        .open_path(folder.to_string_lossy(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// Stops syncing a pair; how many files were kept (not synced) when deleting.
#[tauri::command]
fn remove(app: AppHandle, pair: i64, delete_files: bool) -> Answer<usize> {
    let state = state(&app);
    let pair = state.pair(pair).map_err(err)?.ok_or("no such folder")?;
    ops::remove(&state, &pair, delete_files).map_err(err)
}

/// Pauses or resumes a pair, or every pair (`pair` none).
#[tauri::command]
fn set_paused(app: AppHandle, pair: Option<i64>, paused: bool) -> Answer<()> {
    let state = state(&app);
    for p in state.pairs().map_err(err)? {
        if pair.is_none_or(|id| id == p.id) {
            ops::set_paused(&state, &p, paused).map_err(err)?;
        }
    }
    app.state::<App>().engine.wake();
    Ok(())
}

/// Points a stopped pair at its folder: `different` until confirmed when it
/// isn't the folder that was synced.
#[tauri::command]
fn locate(app: AppHandle, pair: i64, path: PathBuf, confirm: bool) -> Answer<ops::Located> {
    let state = state(&app);
    let pair = state.pair(pair).map_err(err)?.ok_or("no such folder")?;
    let located = ops::locate(&state, &pair, &path, confirm).map_err(err)?;
    app.state::<App>().engine.wake();
    Ok(located)
}

#[tauri::command]
async fn undo(app: AppHandle, notice: i64) -> Answer<usize> {
    ops::undo(&state(&app), notice).await.map_err(err)
}

#[tauri::command]
fn dismiss(app: AppHandle, notice: i64) -> Answer<()> {
    state(&app).dismiss(notice).map_err(err)
}

/// An account's settings: its default folder, what libraries new to it do,
/// and the size up to which they sync (0: no limit).
#[tauri::command]
fn set_account(
    app: AppHandle,
    account: i64,
    default_folder: PathBuf,
    new_libraries: NewLibraries,
    auto_sync_limit: i64,
) -> Answer<()> {
    let state = state(&app);
    state
        .set_default_folder(account, &default_folder)
        .map_err(err)?;
    state
        .set_new_libraries(account, new_libraries)
        .map_err(err)?;
    state
        .set_auto_sync_limit(account, auto_sync_limit)
        .map_err(err)?;
    app.state::<App>().engine.wake();
    Ok(())
}

#[tauri::command]
async fn remove_account(app: AppHandle, account: i64, delete_files: bool) -> Answer<usize> {
    let state = state(&app);
    let account = state
        .account(account)
        .map_err(err)?
        .ok_or("no such account")?;
    let kept = ops::remove_account(&state, &account, delete_files)
        .await
        .map_err(err)?;
    app.state::<App>().engine.wake();
    Ok(kept)
}

#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(signed_out: bool) -> crate::state::Account {
        crate::state::Account {
            id: 1,
            instance: "https://cloud.example".into(),
            uuid: Uuid::nil(),
            email: "a@example.com".into(),
            device: "laptop".into(),
            password: crate::state::Password::InState("x".into()),
            default_folder: "/home/a/theca".into(),
            new_libraries: NewLibraries::Notify,
            auto_sync_limit: 0,
            signed_out,
        }
    }

    /// A release not installed is offered, but for another install under
    /// way: that one installs it.
    #[cfg(feature = "self-update")]
    #[test]
    fn busy_is_not_offered() {
        use crate::update::UpdateError;
        assert!(worth_offering(None));
        assert!(worth_offering(Some(&UpdateError::OtherKey)));
        assert!(!worth_offering(Some(&UpdateError::Busy)));
    }

    /// A display named by X11's variable or Wayland's, either; an empty one
    /// names none.
    #[cfg(feature = "self-update")]
    #[test]
    fn a_display_is_either_variable_set() {
        let some = |v: &str| Some(std::ffi::OsString::from(v));
        assert!(display_in(some(":0"), None));
        assert!(display_in(None, some("wayland-0")));
        assert!(display_in(some(":0"), some("wayland-0")));
        assert!(display_in(some(""), some("wayland-0")));
        assert!(!display_in(some(""), None));
        assert!(!display_in(None, None));
    }

    /// The tray says an update waiting ("Update to X.Y.Z"), after what
    /// needs the user's attention.
    #[test]
    fn the_tray_says_an_update_waiting() {
        let (overall, said, installs) = summary(&[account(false)], &[], Some("9.0.0"));
        assert!(overall == Overall::Idle);
        assert_eq!((said.as_str(), installs), ("Update to 9.0.0", true));
        let (_, said, installs) = summary(&[account(false)], &[], None);
        assert_eq!((said.as_str(), installs), ("Nothing synced yet", false));
        // Signed out, an update waiting: the summary says the first, and a
        // click doesn't install.
        let (overall, said, installs) = summary(&[account(true)], &[], Some("9.0.0"));
        assert!(overall == Overall::Attention);
        assert_eq!((said.as_str(), installs), ("Signed out: open Theca", false));
        // A folder stopped, an update waiting: the same.
        let stopped = crate::state::Pair {
            id: 1,
            account: 1,
            library: Uuid::nil(),
            root: Uuid::nil(),
            library_name: "Docs".into(),
            path: "/home/a/theca/Docs".into(),
            root_file_id: None,
            position: None,
            position_event: None,
            paused: false,
            stop: Some(crate::state::Stop::LibraryDeleted),
            writable: true,
            activity: "up to date".into(),
            error: None,
            synced_at: None,
            mode: Mode::Sync,
        };
        let (overall, said, installs) = summary(&[account(false)], &[stopped], Some("9.0.0"));
        assert!(overall == Overall::Attention);
        assert_eq!(
            (said.as_str(), installs),
            ("A folder stopped syncing", false)
        );
    }
}
