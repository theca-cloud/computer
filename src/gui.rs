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
use crate::state::{Notice, State, Update};

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
}

/// Kept for the app's life: the engine runs alone on this state.
struct Lock(
    #[expect(dead_code, reason = "held, never read: its drop releases the lock")] std::fs::File,
);

/// Runs the app until it quits. `background`: started at login, the window
/// stays closed.
pub fn run(state_path: PathBuf, background: bool) -> Result<(), tauri::Error> {
    let engine = Handle::default();
    let (stop, stopped) = watch::channel(false);
    let app = tauri::Builder::default()
        // Launched again: the running app's window comes forward.
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
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
        })
        .invoke_handler(tauri::generate_handler![
            status,
            libraries,
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
            quit,
        ])
        .setup(move |app| {
            // Taken here, once the single-instance plugin had its turn: a
            // second launch hands over to the running app (its window comes
            // forward) instead of failing on the lock.
            app.manage(Lock(crate::lock(&state_path)?));
            let handle = app.handle().clone();
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
        }
        _ => {}
    });
    Ok(())
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
                    if !matches!(update, Update::Offered { .. }) {
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
            _ => {}
        })
        .build(app)?;
    Ok(Tray {
        icon,
        summary,
        pause,
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
        let what = if p.stopped.is_none() {
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

/// The tray's icon and summary from the state: attention first (an account
/// signed out, a pair stopped), then syncing, then paused.
fn refresh_tray(app: &AppHandle) {
    let a = app.state::<App>();
    let tray = a.tray.lock().expect("tray");
    let Some(tray) = tray.as_ref() else {
        return;
    };
    let state = state(app);
    let (Ok(accounts), Ok(pairs)) = (state.accounts(), state.pairs()) else {
        return;
    };
    let (overall, summary) = if accounts.iter().any(|a| a.signed_out) {
        (Overall::Attention, "Signed out: open Theca".to_owned())
    } else if pairs.iter().any(|p| p.stopped.is_some()) {
        (Overall::Attention, "A folder stopped syncing".to_owned())
    } else if pairs
        .iter()
        .any(|p| !p.paused && p.activity != "up to date" && p.activity != "waiting")
    {
        (Overall::Syncing, "Syncing…".to_owned())
    } else if !pairs.is_empty() && pairs.iter().all(|p| p.paused) {
        (Overall::Paused, "Paused".to_owned())
    } else if pairs.is_empty() {
        (Overall::Idle, "Nothing synced yet".to_owned())
    } else {
        (Overall::Idle, "Up to date".to_owned())
    };
    let _ = tray.icon.set_icon(Some(tray_icon(overall)));
    let _ = tray.icon.set_icon_as_template(cfg!(target_os = "macos"));
    let _ = tray.icon.set_tooltip(Some(format!("Theca: {summary}")));
    let _ = tray.summary.set_text(summary);
    let paused = !pairs.is_empty() && pairs.iter().all(|p| p.paused);
    let _ = tray
        .pause
        .set_text(if paused { "Resume" } else { "Pause all" });
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
    auto_sync_limit: Option<i64>,
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
    stopped: Option<String>,
    writable: bool,
    activity: String,
    synced_at: Option<chrono::DateTime<chrono::Utc>>,
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
                auto_sync_limit: a.auto_sync_limit,
                signed_out: a.signed_out,
            })
            .collect(),
        pairs: state
            .pairs()
            .map_err(err)?
            .into_iter()
            .map(|p| PairView {
                id: p.id,
                account: p.account,
                library: p.library,
                library_name: p.library_name,
                path: p.path,
                paused: p.paused,
                stopped: p.stopped,
                writable: p.writable,
                activity: p.activity,
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
        let result = ops::login(&state, &instance, &device, folder, None, |flow| {
            let _ = shown.send(flow.clone());
        })
        .await;
        let outcome = match result {
            Ok(account) => {
                background.state::<App>().engine.wake();
                serde_json::json!({ "account": account.id })
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
) -> Answer<Added> {
    let state = state(&app);
    let account = state
        .account(account)
        .map_err(err)?
        .ok_or("no such account")?;
    match ops::add(&state, &account, &library.to_string(), path, merge).await {
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

/// An account's settings: its default folder, its auto-sync limit (none:
/// never).
#[tauri::command]
fn set_account(
    app: AppHandle,
    account: i64,
    default_folder: PathBuf,
    auto_sync_limit: Option<i64>,
) -> Answer<()> {
    let state = state(&app);
    state
        .set_default_folder(account, &default_folder)
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
