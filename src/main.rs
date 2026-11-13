//! `theca`: the CLI and daemon (and, with the `gui` feature, the app).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use theca::engine::{self, Handle, plural};
use theca::keychain::KeychainError;
use theca::state::{self, State};
#[cfg(feature = "self-update")]
use theca::update;
use theca::{exit, ops};

/// Where a Mac's drive is switched on: macOS adds it switched off.
#[cfg(target_os = "macos")]
const SWITCH_ON: &str = "Switch it on in System Settings → General → Login Items & Extensions → File Providers → Theca.";

/// What a command fails with: said on stderr, with its exit code (`exit`).
#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("{0}")]
    Refused(String),
    /// `status`: a pair isn't syncing.
    #[error("{0}")]
    NotSyncing(String),
    /// `status`: an account is signed out.
    #[error("{0}")]
    SignedOut(String),
    #[error(transparent)]
    Op(#[from] ops::OpError),
    #[error(transparent)]
    State(#[from] state::StateError),
    #[error(transparent)]
    Engine(#[from] engine::EngineError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Update(#[from] theca::update::UpdateError),
    #[cfg(feature = "gui")]
    #[error(transparent)]
    App(#[from] tauri::Error),
}

impl CliError {
    fn code(&self) -> u8 {
        match self {
            CliError::NotSyncing(_) => exit::NOT_SYNCING,
            CliError::SignedOut(_)
            | CliError::Op(ops::OpError::Keychain(KeychainError::Missing)) => exit::SIGNED_OUT,
            CliError::Op(ops::OpError::Client(e)) if e.code() == Some("unauthenticated") => {
                exit::SIGNED_OUT
            }
            _ => exit::ERROR,
        }
    }
}

type Result<T> = std::result::Result<T, CliError>;

fn refused<T>(why: impl Into<String>) -> Result<T> {
    Err(CliError::Refused(why.into()))
}

#[derive(Parser)]
#[command(
    version,
    about = "Keeps local folders identical to Theca libraries, both ways"
)]
struct Cli {
    /// The state database (default: in the OS's data directory).
    #[arg(long, env = "THECA_STATE", global = true)]
    state: Option<PathBuf>,
    /// Started at login: the app's window stays closed (its tray only).
    #[arg(long, hide = true)]
    background: bool,
    /// The update manifest read instead of the latest release's, still
    /// verified with the release key (the release checks, ci/package.sh).
    #[arg(long, env = "THECA_UPDATE_MANIFEST", global = true, hide = true)]
    manifest: Option<String>,
    /// None: the app (a build with the `gui` feature).
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Log this device in through the browser (prints the page to open).
    Login {
        /// The instance's address, e.g. https://cloud.example.com
        instance: String,
        /// This device's name (default: its host name).
        #[arg(long)]
        name: Option<String>,
        /// Where its libraries go by default (default: ~/theca).
        #[arg(long)]
        folder: Option<PathBuf>,
        /// An app password made in the web app: no browser (scripts).
        #[arg(long, env = "THECA_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// Keep the app password in this app's state file, not in the OS
        /// keychain (where there is none, it is kept there anyway).
        /// `THECA_NO_KEYRING`: anything but empty, 0, false, no or off.
        #[arg(long, env = "THECA_NO_KEYRING", value_parser = clap::builder::FalseyValueParser::new())]
        no_keyring: bool,
        /// What libraries new to the account do: sync (up to the size
        /// limit), drive, or silent (listed only: `theca libraries`).
        /// Default: silent. Its libraries now are added with `theca add`.
        #[arg(long, value_parser = headless_new_libraries)]
        new_libraries: Option<state::NewLibraries>,
    },
    /// The accounts on this device.
    Accounts,
    /// An account's settings: shown, or changed.
    Account {
        account: String,
        /// Where its libraries go by default.
        #[arg(long)]
        folder: Option<PathBuf>,
        /// What libraries new to the account do: sync (up to the size
        /// limit), drive, notify (the app shows it), or silent (listed only).
        #[arg(long, value_parser = clap::value_parser!(state::NewLibraries))]
        new_libraries: Option<state::NewLibraries>,
        /// New libraries up to this size sync (`--new-libraries sync`):
        /// 500MB, 2GiB, or 0 for no limit.
        #[arg(long, value_parser = limit_value)]
        auto_sync_limit: Option<LimitValue>,
        /// 1.0's flag, kept: libraries up to this many MB sync by
        /// themselves, 0 none (`--new-libraries silent`).
        #[arg(long, hide = true)]
        auto_sync_mb: Option<i64>,
    },
    /// Remove an account from this device.
    RemoveAccount {
        account: String,
        /// Delete the synced files too (only those identical to what was synced).
        #[arg(long)]
        delete_files: bool,
    },
    /// The account's libraries: synced where, or offered, and their sizes.
    Libraries {
        #[arg(long)]
        account: Option<String>,
    },
    /// Start syncing a library (by name) into a folder.
    Add {
        library: String,
        /// Default: <default folder>/<library>.
        path: Option<PathBuf>,
        /// The folder already holds files: merge them with the library.
        #[arg(long)]
        merge: bool,
        /// A drive: the whole library shown, each file downloaded when
        /// opened, kept within the drive limit.
        #[arg(long)]
        drive: bool,
        #[arg(long)]
        account: Option<String>,
    },
    /// Stop syncing a folder (by library name, path or ID).
    Remove {
        pair: String,
        /// Delete the synced files too (only those identical to what was synced).
        #[arg(long)]
        delete_files: bool,
    },
    /// Pause one synced folder, or all.
    Pause { pair: Option<String> },
    /// Resume one synced folder, or all.
    Resume { pair: Option<String> },
    /// Point a stopped folder at where it is now (moved, or on another disk).
    Locate {
        pair: String,
        path: PathBuf,
        /// It isn't the folder that was synced: sync it anyway (a merge).
        #[arg(long)]
        confirm: bool,
    },
    /// What is synced, what it's doing, errors and notices.
    Status,
    /// Keep files or folders of a drive on this computer, downloaded (what
    /// arrives later in a folder kept too), outside the drive limit.
    Keep {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Free the space files or folders of a drive take, now: they stay
    /// listed, downloaded again when opened (not while an edit isn't
    /// uploaded).
    Free {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// How much downloaded content drives keep, shown, or set: 20GB (10^9
    /// bytes) or 20GiB (2^30). Kept files and edits not yet uploaded don't
    /// count.
    DriveLimit {
        /// 20GB, 20GiB, or `default` (10 GiB, or a tenth of the disk).
        #[arg(value_parser = limit_value)]
        size: Option<LimitValue>,
    },
    /// Where the logs are and their settings, shown, or set (each applies
    /// at the next start of the app or `theca run`; `default` resets one).
    Logs {
        /// error, warn, info, debug, trace (default: info).
        #[arg(long, value_parser = reset::<LogLevel>)]
        level: Option<Reset<LogLevel>>,
        /// Days of logs kept (default: 14).
        #[arg(long, value_parser = reset::<u32>)]
        days: Option<Reset<u32>>,
        /// Their size in all, the oldest removed past it: 100MB, 1GiB
        /// (default: 100 MB).
        #[arg(long, value_parser = limit_value)]
        max_size: Option<LimitValue>,
        /// Their folder (default: `logs` beside the state).
        #[arg(long, value_parser = reset::<PathBuf>)]
        path: Option<Reset<PathBuf>>,
    },
    /// Restore what the last deletion moved to the trash.
    Undo,
    /// Run the sync engine in the foreground (what a service runs).
    Run,
    /// Check for a newer release, and install it (in the packages that
    /// update themselves; the others are updated by their package manager).
    Update,
    /// How this computer updates: auto (installs and restarts), ask (says
    /// so), off (never checks by itself); shown, or set.
    Updates {
        #[arg(value_parser = theca::update::Mode::parse)]
        mode: Option<theca::update::Mode>,
    },
    /// Every drive removed, as `remove` does (the Windows uninstaller runs
    /// it: no sync root stays registered).
    #[command(hide = true)]
    RemoveDrives {
        /// On a Mac, also every other Theca drive macOS has, whatever state
        /// added it (an earlier one's, a test's): the checks' clean start.
        #[arg(long)]
        all: bool,
    },
}

fn main() -> ExitCode {
    let matches = Cli::command().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    // Its name only: an argument may be a token (`login --token`).
    let name = matches.subcommand_name().unwrap_or("app").to_owned();
    // Every command logs to the day's file, as the state's settings say (no
    // state: stderr only); the app and `theca run` keep the folder pruned.
    // Read before the state is opened: its migration, if any, is logged.
    let settings = state_path(cli.state.clone())
        .ok()
        .map(|path| theca::logs::settings_at(&path));
    let _logging = theca::logs::init(settings.as_ref());
    let long_running = matches!(cli.command, None | Some(Command::Run));
    if long_running && let Some(settings) = &settings {
        theca::logs::keep(settings);
    }
    let span = tracing::info_span!("command", name);
    let _entered = span.enter();
    if long_running {
        tracing::info!(version = env!("CARGO_PKG_VERSION"), "started");
    }
    if cli.command.is_none() {
        // The app runs its own runtime, on the main thread (the OS's windows
        // want it).
        return exit(app(cli.state, cli.background, cli.manifest));
    }
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    exit(runtime.block_on(command(cli)))
}

fn exit(result: Result<()>) -> ExitCode {
    match result {
        Ok(()) => {
            tracing::info!(target: theca::logs::EXIT, "done");
            ExitCode::SUCCESS
        }
        Err(e) => {
            // In the file too (the terminal has it from the line below).
            tracing::warn!(target: theca::logs::EXIT, code = e.code(), "{e}");
            eprintln!("theca: {e}");
            ExitCode::from(e.code())
        }
    }
}

/// The state database: `--state`, else in the OS's data directory.
fn state_path(given: Option<PathBuf>) -> Result<PathBuf> {
    match given.or_else(state::default_path) {
        Some(path) => Ok(path),
        None => refused("no data directory: give --state"),
    }
}

async fn command(cli: Cli) -> Result<()> {
    let path = state_path(cli.state)?;
    let state = State::open(&path)?;
    #[cfg(feature = "self-update")]
    let manifest_url = cli
        .manifest
        .as_deref()
        .unwrap_or(update::MANIFEST)
        .to_owned();
    match cli.command.expect("the app is started by main") {
        Command::Login {
            instance,
            name,
            folder,
            token,
            no_keyring,
            new_libraries,
        } => {
            let name = name.unwrap_or_else(ops::device_name);
            let account = ops::login(
                &state,
                &instance,
                &name,
                folder,
                token,
                !no_keyring,
                new_libraries,
                state::NewLibraries::Silent,
                |flow: &theca_common::api::LoginFlow| {
                    println!("Open this page to allow \"{name}\": {}", flow.url);
                    println!("Check it shows the code {}", flow.code);
                },
            )
            .await?;
            if !no_keyring && matches!(account.password, state::Password::InState(_)) {
                println!(
                    "No keychain could be reached: the app password is kept in {}.",
                    path.display()
                );
            }
            println!(
                "Logged in as {} on {}. Libraries go into {} by default.",
                account.email,
                account.instance,
                account.default_folder.display()
            );
            println!(
                "Libraries new to it later: {}.",
                new_libraries_said(account.new_libraries, account.auto_sync_limit)
            );
            // Nothing added by the login: each chosen with `theca add` (the
            // login worked even if this list can't be had).
            match ops::libraries(&state, &account).await {
                Ok(libraries) => {
                    let unsynced: Vec<_> = libraries.iter().filter(|l| l.pair.is_none()).collect();
                    if !unsynced.is_empty() {
                        println!("Its libraries, to add as a synced folder or a drive:");
                    }
                    for l in &unsynced {
                        let size = l.size.map_or_else(|| "?".into(), size);
                        // Two of one name: by its ID (`add` refuses the name).
                        let twins = libraries
                            .iter()
                            .filter(|o| {
                                theca::local::key(&o.library.name)
                                    == theca::local::key(&l.library.name)
                            })
                            .count();
                        let which = if twins > 1 {
                            l.library.id.to_string()
                        } else {
                            format!("{:?}", l.library.name)
                        };
                        let organization = l.organization.as_deref().unwrap_or("?");
                        println!("  theca add {which}  (or --drive)  # {size}, {organization}");
                    }
                }
                Err(e) => println!("Its libraries: theca libraries ({e})."),
            }
        }
        Command::Accounts => {
            for a in state.accounts()? {
                let signed_out = if a.signed_out { " (signed out)" } else { "" };
                println!(
                    "{}  {} on {}{signed_out}, into {}, as {}",
                    a.id,
                    a.email,
                    a.instance,
                    a.default_folder.display(),
                    a.device
                );
            }
        }
        Command::Account {
            account,
            folder,
            new_libraries,
            auto_sync_limit,
            auto_sync_mb,
        } => {
            let account = ops::find_account(&state, Some(&account))?;
            if let Some(folder) = folder {
                // Resolved here: `add` runs in another directory.
                let folder = std::path::absolute(&folder).unwrap_or(folder);
                state.set_default_folder(account.id, &folder)?;
            }
            // 1.0's meaning: MB synced by themselves, 0 none.
            if let Some(mb) = auto_sync_mb {
                if mb > 0 {
                    state.set_new_libraries(account.id, state::NewLibraries::Sync)?;
                    state.set_auto_sync_limit(account.id, mb * 1024 * 1024)?;
                } else {
                    state.set_new_libraries(account.id, state::NewLibraries::Silent)?;
                }
            }
            if let Some(new) = new_libraries {
                state.set_new_libraries(account.id, new)?;
            }
            if let Some(limit) = auto_sync_limit {
                let bytes = match limit {
                    LimitValue::Bytes(b) => i64::try_from(b).unwrap_or(i64::MAX),
                    LimitValue::Default => ops::AUTO_SYNC_LIMIT,
                };
                state.set_auto_sync_limit(account.id, bytes)?;
            }
            let a = state.account(account.id)?.unwrap_or(account);
            println!("{} on {}", a.email, a.instance);
            println!("  libraries go into {}", a.default_folder.display());
            println!(
                "  new libraries: {}",
                new_libraries_said(a.new_libraries, a.auto_sync_limit)
            );
        }
        Command::RemoveAccount {
            account,
            delete_files,
        } => {
            let account = ops::find_account(&state, Some(&account))?;
            let kept = ops::remove_account(&state, &account, delete_files).await?;
            println!("Removed {}.", account.email);
            if kept > 0 {
                println!("Kept {} that weren't synced.", plural(kept, "file"));
            }
        }
        Command::Libraries { account } => {
            let account = ops::find_account(&state, account.as_deref())?;
            for l in ops::libraries(&state, &account).await? {
                let size = l.size.map_or_else(|| "?".into(), size);
                let organization = l.organization.as_deref().unwrap_or("?");
                let name = format!("{} ({organization}, {})", l.library.name, l.library.id);
                match &l.pair {
                    Some(p) => println!("{name}  {size}  synced into {}", p.path.display()),
                    None => println!("{name}  {size}  not synced"),
                }
            }
        }
        Command::Add {
            library,
            path,
            merge,
            drive,
            account,
        } => {
            let account = ops::find_account(&state, account.as_deref())?;
            let mode = if drive {
                state::Mode::Drive
            } else {
                state::Mode::Sync
            };
            let pair = ops::add(&state, &account, &library, path, merge, mode).await?;
            let what = if drive { "is a drive in" } else { "syncs into" };
            println!(
                "{} {what} {} (theca run, or the app, does it).",
                pair.library_name,
                pair.path.display()
            );
            #[cfg(target_os = "macos")]
            if drive {
                println!("{SWITCH_ON}");
            }
        }
        Command::Remove { pair, delete_files } => {
            let pair = ops::find_pair(&state, &pair)?;
            let kept = ops::remove(&state, &pair, delete_files)?;
            println!("{} isn't synced anymore.", pair.path.display());
            if kept > 0 {
                println!("Kept {} that weren't synced.", plural(kept, "file"));
            }
        }
        Command::Pause { pair } => pause(&state, pair, true)?,
        Command::Resume { pair } => pause(&state, pair, false)?,
        Command::Locate {
            pair,
            path,
            confirm,
        } => {
            let pair = ops::find_pair(&state, &pair)?;
            let path = path.canonicalize().unwrap_or(path);
            match ops::locate(&state, &pair, &path, confirm)? {
                ops::Located::Moved => {
                    println!("{} syncs into {}.", pair.library_name, path.display())
                }
                ops::Located::Different => {
                    return refused(format!(
                        "{} isn't the folder that was synced: add --confirm to merge it with {}",
                        path.display(),
                        pair.library_name
                    ));
                }
            }
        }
        Command::Status => {
            #[cfg(feature = "self-update")]
            print_newer_release(&state, &manifest_url).await;
            status(&state)?
        }
        #[cfg(feature = "self-update")]
        Command::Update => update_now(&path, &state, &manifest_url).await?,
        #[cfg(feature = "self-update")]
        Command::Updates { mode } => match mode {
            Some(mode) => {
                update::set_mode(&state, mode)?;
                println!("Updates: {}", mode.as_str());
            }
            None => println!("Updates: {}", update::mode(&state)?.as_str()),
        },
        #[cfg(not(feature = "self-update"))]
        Command::Update | Command::Updates { .. } => {
            return refused(theca::update::NOT_SELF_UPDATING);
        }
        Command::RemoveDrives { all } => {
            for pair in state.pairs()? {
                if pair.mode == state::Mode::Drive {
                    let kept = ops::remove(&state, &pair, false)?;
                    println!("{} isn't a drive anymore.", pair.path.display());
                    if kept > 0 {
                        println!("Kept {} that weren't uploaded.", plural(kept, "file"));
                    }
                }
            }
            #[cfg(target_os = "macos")]
            if all {
                let domains = theca_fileprovider::host::domains().map_err(ops::OpError::Refused)?;
                let mut refused = Vec::new();
                for d in domains {
                    match theca_fileprovider::host::remove(&d.id) {
                        Ok(left) => {
                            println!("The drive {} is removed from this Mac.", d.name);
                            if let Some(left) = left {
                                println!("What wasn't uploaded is in {}.", left.display());
                            }
                        }
                        Err(e) => refused.push(format!("{}: {e}", d.name)),
                    }
                }
                if !refused.is_empty() {
                    return Err(ops::OpError::Refused(format!(
                        "macOS didn't remove: {}",
                        refused.join("; ")
                    ))
                    .into());
                }
            }
            #[cfg(not(target_os = "macos"))]
            let _ = all;
        }
        Command::Free { paths } => {
            for path in paths {
                let pair = ops::free(&state, &path)?;
                println!("{} is freed ({}).", path.display(), pair.library_name);
            }
        }
        Command::Keep { paths } => {
            for path in paths {
                let pair = ops::keep(&state, &path, true)?;
                println!(
                    "{} is kept on this computer ({}).",
                    path.display(),
                    pair.library_name
                );
            }
        }
        Command::DriveLimit { size: set } => {
            match set {
                Some(LimitValue::Bytes(bytes)) => state.set_drive_limit(bytes)?,
                Some(LimitValue::Default) => state.clear_drive_limit()?,
                None => {}
            }
            let limit = theca::drive::limit(&state)?;
            let default = state.drive_limit()?.is_none();
            let default = if default {
                " (the default: 10 GiB, or a tenth of the disk if smaller)"
            } else {
                ""
            };
            println!(
                "Drives keep up to {} downloaded{default}.",
                limit_size(limit)
            );
        }
        Command::Logs {
            level,
            days,
            max_size,
            path,
        } => {
            use theca::logs::Change;
            let changes = [
                level.map(|l| Change::Level(l.value().map(|l| l.0))),
                days.map(|d| Change::Days(d.value())),
                max_size.map(|s| {
                    Change::MaxSize(match s {
                        LimitValue::Bytes(b) => Some(b),
                        LimitValue::Default => None,
                    })
                }),
                path.map(|p| {
                    Change::Folder(p.value().map(|p| std::path::absolute(&p).unwrap_or(p)))
                }),
            ];
            let changed = changes.iter().any(Option::is_some);
            for change in changes.into_iter().flatten() {
                theca::logs::set(&state, change)?;
            }
            let s = theca::logs::settings(&state)?;
            let folder = s
                .folder
                .as_ref()
                .map_or_else(|| "none".to_owned(), |f| f.display().to_string());
            let default = if s.default_folder {
                " (the default)"
            } else {
                ""
            };
            println!("Logs: {folder}{default}");
            println!(
                "  level {}, {} days kept, up to {} in all",
                s.level,
                s.days,
                limit_size(s.max_size)
            );
            #[cfg(target_os = "macos")]
            if let Some(container) = theca_fileprovider::host::container() {
                println!(
                    "The drive extension's: {}",
                    container.join("drive.log").display()
                );
            }
            if changed {
                println!("Applies at the next start of the app or `theca run`.");
            }
        }
        Command::Undo => {
            let Some(notice) = state.latest_undo()? else {
                println!("Nothing to undo.");
                return Ok(());
            };
            let restored = ops::undo(&state, notice.id).await?;
            println!("Restored {}.", plural(restored, "item"));
        }
        Command::Run => {
            let lock = theca::lock(&path)?;
            let (stop, stopped) = tokio::sync::watch::channel(false);
            #[cfg(feature = "self-update")]
            let restart = keep_updated(&path, &manifest_url, stop.clone(), stopped.clone());
            tokio::spawn(async move {
                stop_signal().await;
                let _ = stop.send(true);
                // A second one: at once.
                stop_signal().await;
                std::process::exit(130);
            });
            engine::run(path, Handle::default(), stopped).await?;
            // Updated: the new release in this process's place, the engine
            // stopped.
            #[cfg(feature = "self-update")]
            if let Some(installed) = restart.lock().expect("restart").take() {
                // The new one takes the state's lock: released first (on
                // Windows the new process could start before this one ends).
                drop(lock);
                return Err(installed.restart().into());
            }
            drop(lock);
        }
    }
    Ok(())
}

/// `theca update`: the release at `manifest_url`, if newer, installed in any
/// mode (asked for). The app open on this state installs it itself (handed
/// over); a running `theca run` goes on with the old one until it starts
/// again.
#[cfg(feature = "self-update")]
async fn update_now(path: &std::path::Path, state: &State, manifest_url: &str) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let Some(release) = update::check(manifest_url).await? else {
        println!("Theca {current} is the latest.");
        return Ok(());
    };
    // Held while installing: the app doesn't start meanwhile.
    let lock = theca::lock(path);
    #[cfg(feature = "gui")]
    if lock.is_err() {
        if theca::gui::has_display() {
            println!("The app is installing Theca {}.", release.version);
            match theca::gui::hand_over() {
                Ok(()) => {
                    println!("No app answered (`theca run` holds this state): installing here.")
                }
                Err(e) => println!("The app can't be reached ({e}): installing here."),
            }
        } else {
            // No display, no app on it (starting one to ask would panic).
            println!("No display, so no app to hand this to: installing here.");
        }
    }
    let _lock = lock;
    let servers = update::servers(state).await;
    let Some(installed) = update::Installed::this() else {
        #[cfg(feature = "gui")]
        if cfg!(any(target_os = "macos", windows)) {
            theca::gui::install_now(manifest_url, &release, &servers).await?;
            println!(
                "Theca {} installed (was {current}): {}",
                release.version,
                release.notes_url()
            );
            return Ok(());
        }
        return Err(update::UpdateError::NotInstalled.into());
    };
    update::install(&release, update::PUBLIC_KEY, &servers, &installed).await?;
    println!(
        "Theca {} installed (was {current}): {}",
        release.version,
        release.notes_url()
    );
    if matches!(installed, update::Installed::Headless(_)) {
        println!("A running `theca run` goes on with {current} until it starts again.");
    }
    Ok(())
}

/// `theca status`'s first line, a newer release (not stored: asked now,
/// unless updates are `off`).
#[cfg(feature = "self-update")]
async fn print_newer_release(state: &State, manifest_url: &str) {
    if update::mode(state).is_ok_and(|m| m == update::Mode::Off) {
        return;
    }
    // A status answers at once: a manifest slow to come is no news.
    let check = update::check(manifest_url);
    let Ok(checked) = tokio::time::timeout(std::time::Duration::from_secs(3), check).await else {
        return tracing::info!("no update check: the manifest took more than 3 s");
    };
    match checked {
        Ok(Some(release)) => println!(
            "Theca {} is available: theca update ({})\n",
            release.version,
            release.notes_url()
        ),
        Ok(None) => {}
        Err(e) => tracing::info!("no update check: {e}"),
    }
}

/// `theca run`'s checks (`update::schedule`) as the state's mode says: a
/// release found is said in the log; installed (`auto`), the engine stops,
/// and the release to start in this process's place is given back.
#[cfg(feature = "self-update")]
fn keep_updated(
    path: &std::path::Path,
    manifest_url: &str,
    stop: tokio::sync::watch::Sender<bool>,
    stopped: tokio::sync::watch::Receiver<bool>,
) -> std::sync::Arc<std::sync::Mutex<Option<update::Installed>>> {
    use std::ops::ControlFlow;
    let restart = std::sync::Arc::<std::sync::Mutex<_>>::default();
    let Some(installed) = update::Installed::this() else {
        return restart;
    };
    let (path, manifest_url) = (path.to_owned(), manifest_url.to_owned());
    let given = restart.clone();
    let tick = move || {
        let (path, manifest_url, installed, given, stop) = (
            path.clone(),
            manifest_url.clone(),
            installed.clone(),
            given.clone(),
            stop.clone(),
        );
        async move {
            let Ok(state) = State::open(&path) else {
                return ControlFlow::Continue(());
            };
            let to = installed.clone();
            let checked = update::tick(&state, &manifest_url, |manifest, servers| async move {
                update::install(&manifest, update::PUBLIC_KEY, &servers, &to).await
            })
            .await;
            match checked {
                update::Checked::Installed(manifest) => {
                    tracing::info!(version = manifest.version, "installed: starting it");
                    *given.lock().expect("restart") = Some(installed);
                    let _ = stop.send(true);
                    ControlFlow::Break(())
                }
                update::Checked::Offered { manifest, why } => {
                    let why = why.map(|e| format!(" ({e})")).unwrap_or_default();
                    tracing::info!(
                        "Theca {} is available{why}: theca update; {}",
                        manifest.version,
                        manifest.notes_url()
                    );
                    ControlFlow::Continue(())
                }
                _ => ControlFlow::Continue(()),
            }
        }
    };
    tokio::spawn(update::schedule(tick, stopped));
    restart
}

#[cfg(feature = "gui")]
fn app(state: Option<PathBuf>, background: bool, manifest_url: Option<String>) -> Result<()> {
    let path = state_path(state)?;
    State::open(&path)?;
    theca::gui::run(path, background, manifest_url)?;
    Ok(())
}

#[cfg(not(feature = "gui"))]
fn app(_: Option<PathBuf>, _: bool, _: Option<String>) -> Result<()> {
    refused("this build has no app (the `gui` feature): give a command, see --help")
}

fn pause(state: &State, pair: Option<String>, paused: bool) -> Result<()> {
    let pairs = match pair {
        Some(which) => vec![ops::find_pair(state, &which)?],
        None => state.pairs()?,
    };
    for p in pairs {
        ops::set_paused(state, &p, paused)?;
        println!(
            "{} {}",
            p.path.display(),
            if paused { "paused" } else { "resumed" }
        );
    }
    Ok(())
}

fn status(state: &State) -> Result<()> {
    let accounts = state.accounts()?;
    let pairs = state.pairs()?;
    if accounts.is_empty() {
        println!("No account: theca login <instance>");
        return Ok(());
    }
    for a in &accounts {
        println!(
            "{} on {}{}",
            a.email,
            a.instance,
            if a.signed_out { " (signed out)" } else { "" }
        );
        for p in pairs.iter().filter(|p| p.account == a.id) {
            let what = if p.paused {
                "paused".to_owned()
            } else if let Some(stop) = &p.stop {
                format!("stopped: {}", stop.reason())
            } else if let Some(e) = &p.error {
                format!("error: {e}")
            } else {
                p.activity.clone()
            };
            let when = p
                .synced_at
                .map(|t| {
                    format!(
                        ", last synced {}",
                        t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M")
                    )
                })
                .unwrap_or_default();
            println!("  {}  {}  {what}{when}", p.library_name, p.path.display());
            #[cfg(target_os = "macos")]
            if p.mode == state::Mode::Drive {
                let domain = theca::drive::provider::domain_of(p);
                match theca_fileprovider::host::enabled(&domain) {
                    Ok(Some(true)) => println!("    a drive, switched on"),
                    Ok(Some(false)) => {
                        println!("    a drive waiting to be switched on: {SWITCH_ON}")
                    }
                    Ok(None) => {
                        println!("    a drive macOS doesn't have (removed in System Settings?)")
                    }
                    Err(e) => println!("    a drive macOS doesn't say about yet ({e})"),
                }
            }
            #[cfg(any(target_os = "linux", windows))]
            if p.mode == state::Mode::Drive {
                let u = theca::drive::usage(state, p)?;
                println!(
                    "    a drive: {} downloaded, {} kept, {} waiting to upload",
                    size(u.downloaded as i64),
                    u.kept,
                    u.waiting
                );
            }
        }
    }
    let notices = state.notices()?;
    if !notices.is_empty() {
        println!();
        for n in notices.iter().take(20) {
            let undo = if n.undo.is_some() {
                " (theca undo)"
            } else {
                ""
            };
            let path = n
                .path
                .as_deref()
                .map(|p| format!("{p}: "))
                .unwrap_or_default();
            println!("{} {}{}{undo}", n.level, path, n.message);
        }
    }
    // An account signed out, or a pair stopped, syncs nothing until someone
    // acts: a script or a monitor reading the exit code knows.
    if let Some(a) = accounts.iter().find(|a| a.signed_out) {
        return Err(CliError::SignedOut(format!(
            "signed out of {}: theca login {}",
            a.instance, a.instance
        )));
    }
    let stopped = pairs
        .iter()
        .filter(|p| p.not_syncing().is_some() && !p.paused)
        .count();
    if stopped > 0 {
        return Err(CliError::NotSyncing(format!(
            "{} stopped syncing (above)",
            engine::plural(stopped, "folder")
        )));
    }
    Ok(())
}

/// `1.2 MiB`: as the app and the web app show sizes.
/// What libraries new to an account do, said.
fn new_libraries_said(new: state::NewLibraries, limit: i64) -> String {
    use state::NewLibraries::*;
    match new {
        Sync if limit == 0 => "synced by themselves (sync, no size limit)".into(),
        Sync => format!(
            "synced by themselves up to {}, notified above (sync)",
            limit_size(limit.max(0) as u64)
        ),
        Drive => "added as drives (drive)".into(),
        Notify => "listed, with a notification from the app (notify)".into(),
        Silent => "listed only: theca libraries (silent)".into(),
    }
}

/// `login --new-libraries`: what a computer without the app's window can do.
fn headless_new_libraries(value: &str) -> std::result::Result<state::NewLibraries, String> {
    match value.parse()? {
        state::NewLibraries::Notify => {
            Err("notify needs the app's window: sync, drive or silent".into())
        }
        new => Ok(new),
    }
}

/// A setting's value, or `default`: back to it.
#[derive(Clone)]
enum Reset<T> {
    To(T),
    Default,
}

impl<T> Reset<T> {
    fn value(self) -> Option<T> {
        match self {
            Reset::To(v) => Some(v),
            Reset::Default => None,
        }
    }
}

fn reset<T: std::str::FromStr>(value: &str) -> std::result::Result<Reset<T>, String>
where
    T::Err: std::fmt::Display,
{
    if value == "default" {
        return Ok(Reset::Default);
    }
    value.parse::<T>().map(Reset::To).map_err(|e| e.to_string())
}

/// A log level as `theca logs --level` takes it.
#[derive(Clone)]
struct LogLevel(String);

impl std::str::FromStr for LogLevel {
    type Err = String;
    fn from_str(value: &str) -> std::result::Result<Self, String> {
        theca::logs::level(value).map(LogLevel)
    }
}

/// `theca drive-limit`'s value: a size, or back to the default.
#[derive(Clone)]
enum LimitValue {
    Bytes(u64),
    Default,
}

fn limit_value(value: &str) -> std::result::Result<LimitValue, String> {
    if value == "default" {
        return Ok(LimitValue::Default);
    }
    value
        .parse::<bytesize::ByteSize>()
        .map(|b| LimitValue::Bytes(b.as_u64()))
}

/// A limit as it was given: in GB or MB when it's a whole number of them
/// (20GB set reads "20 GB", not "19 GiB"), else as every size shows.
fn limit_size(bytes: u64) -> String {
    const GB: u64 = 1_000_000_000;
    const MB: u64 = 1_000_000;
    if bytes >= GB && bytes.is_multiple_of(GB) {
        format!("{} GB", bytes / GB)
    } else if bytes >= MB && bytes.is_multiple_of(MB) {
        format!("{} MB", bytes / MB)
    } else {
        size(bytes as i64)
    }
}

fn size(bytes: i64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 || value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Ctrl-C, or SIGTERM on Unix (a service manager or `kill` stopping it).
async fn stop_signal() {
    #[cfg(unix)]
    if let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        return;
    }
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A password the server refuses, or gone from the keychain: 4, as a
    /// signed-out account; any other error: 1.
    #[test]
    fn what_exits_with_signed_out() {
        let refused = |code: &str| {
            CliError::Op(ops::OpError::Client(theca_client::Error::Api {
                status: 401,
                error: theca_common::api::ApiError {
                    code: code.into(),
                    message: String::new(),
                    chunks: Vec::new(),
                },
            }))
        };
        assert_eq!(refused("unauthenticated").code(), exit::SIGNED_OUT);
        assert_eq!(refused("forbidden").code(), exit::ERROR);
        let missing = CliError::Op(ops::OpError::Keychain(KeychainError::Missing));
        assert_eq!(missing.code(), exit::SIGNED_OUT);
        let locked = CliError::Op(ops::OpError::Keychain(KeychainError::Unreachable(
            "locked".into(),
        )));
        assert_eq!(locked.code(), exit::ERROR);
    }
}
