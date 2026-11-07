//! `theca`: the CLI and daemon (and, with the `gui` feature, the app).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use theca::engine::{self, Handle, plural};
use theca::ops;
use theca::state::{self, State};

/// What a command fails with: said on stderr, exit code 1.
#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Op(#[from] ops::OpError),
    #[error(transparent)]
    State(#[from] state::StateError),
    #[error(transparent)]
    Engine(#[from] engine::EngineError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[cfg(feature = "gui")]
    #[error(transparent)]
    App(#[from] tauri::Error),
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
    },
    /// The accounts on this device.
    Accounts,
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
        #[arg(long)]
        account: Option<String>,
        /// Libraries up to this many MB appearing later are synced by
        /// themselves (0: never).
        #[arg(long)]
        auto_sync_mb: Option<i64>,
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
    /// Restore what the last deletion moved to the trash.
    Undo,
    /// Run the sync engine in the foreground (what a service runs).
    Run,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "theca=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    if cli.command.is_none() {
        // The app runs its own runtime, on the main thread (the OS's windows
        // want it).
        return exit(app(cli.state, cli.background));
    }
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    exit(runtime.block_on(command(cli)))
}

fn exit(result: Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("theca: {e}");
            ExitCode::FAILURE
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
    match cli.command.expect("the app is started by main") {
        Command::Login {
            instance,
            name,
            folder,
            token,
        } => {
            let name = name.unwrap_or_else(ops::device_name);
            let account = ops::login(&state, &instance, &name, folder, token, |flow| {
                println!("Open this page to allow \"{name}\": {}", flow.url);
                println!("Check it shows the code {}", flow.code);
            })
            .await?;
            println!(
                "Logged in as {} on {}. Libraries go into {} by default.",
                account.email,
                account.instance,
                account.default_folder.display()
            );
            println!("Next: theca libraries, then theca add <library>");
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
            account,
            auto_sync_mb,
        } => {
            let account = ops::find_account(&state, account.as_deref())?;
            if let Some(mb) = auto_sync_mb {
                state.set_auto_sync_limit(account.id, (mb > 0).then_some(mb * 1024 * 1024))?;
            }
            let pair = ops::add(&state, &account, &library, path, merge).await?;
            println!(
                "{} syncs into {} (theca run, or the app, does it).",
                pair.library_name,
                pair.path.display()
            );
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
        Command::Status => status(&state)?,
        Command::Undo => {
            let Some(notice) = state.latest_undo()? else {
                println!("Nothing to undo.");
                return Ok(());
            };
            let restored = ops::undo(&state, notice.id).await?;
            println!("Restored {}.", plural(restored, "item"));
        }
        Command::Run => {
            let _lock = theca::lock(&path)?;
            let (stop, stopped) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                stop_signal().await;
                let _ = stop.send(true);
                // A second one: at once.
                stop_signal().await;
                std::process::exit(130);
            });
            engine::run(path, Handle::default(), stopped).await?;
        }
    }
    Ok(())
}

#[cfg(feature = "gui")]
fn app(state: Option<PathBuf>, background: bool) -> Result<()> {
    let path = state_path(state)?;
    State::open(&path)?;
    theca::gui::run(path, background)?;
    Ok(())
}

#[cfg(not(feature = "gui"))]
fn app(_: Option<PathBuf>, _: bool) -> Result<()> {
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
            } else if let Some(why) = &p.stopped {
                format!("stopped: {why}")
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
    // A pair stopped syncs nothing until someone acts: a script or a monitor
    // reading the exit code knows.
    let stopped = pairs
        .iter()
        .filter(|p| p.not_syncing().is_some() && !p.paused)
        .count();
    if stopped > 0 {
        return refused(format!(
            "{} stopped syncing (above)",
            engine::plural(stopped, "folder")
        ));
    }
    Ok(())
}

/// `1.2 MiB`: as the app and the web app show sizes.
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
