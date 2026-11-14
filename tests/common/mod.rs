//! The end-to-end harness: the real `theca-server` binary (built from the
//! pinned core, or `THECA_SERVER`) on a database of its own, and sync clients
//! as engines on their own state and folder, driven like a user would.

#![allow(dead_code)] // each test uses part of it

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde_json::json;
use sqlx::Connection;
use sqlx::Executor;
use theca::engine::{self, Handle};
use theca::ops;
use theca::state::{Account, Mode, NewLibraries, Pair, State};
use theca_client::{Cancel, Client, DownloadOptions, MemorySink, download};
use theca_common::api;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::Instrument;
use uuid::Uuid;

/// The keychain the tests' app passwords go into: the OS's where CI has one
/// that never prompts (`THECA_TEST_KEYCHAIN=os`: GitHub's macOS and Windows
/// runners), else an in-memory one (a developer's Mac would ask at every
/// build; Linux runners have no Secret Service).
pub fn keychain() {
    if std::env::var("THECA_TEST_KEYCHAIN").as_deref() != Ok("os") {
        theca::keychain::use_mock();
    }
}

/// The script every child of the tests runs under on Unix: it ends the child
/// when the test process is gone or silent (its header says how).
#[cfg(unix)]
pub fn child_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/common/child.sh")
}

/// Detaches every mount at or below `folder`, as `child.sh` does once its
/// command ended (a daemon killed leaves its mounts dead).
#[cfg(unix)]
fn detach_below(folder: &Path) {
    Tended::start(Path::new("true"), 0, Some(folder), |_| {}).stop();
}

/// The file a child engine's output goes to, in its device's folder.
const OUTPUT: &str = "theca.out";

/// A process is there and not a zombie nobody reaped yet.
#[cfg(unix)]
pub fn running(pid: u32) -> bool {
    let state = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let state = String::from_utf8_lossy(&state.stdout);
    !state.trim().is_empty() && !state.trim().starts_with('Z')
}

/// How a tended command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// By itself, or on SIGTERM: its exit code (143: the signal's default).
    Exited(i32),
    /// It had to be killed: after its grace, or at once without one.
    Killed,
}

/// The silence `child.sh` takes for a test process gone, in seconds.
#[cfg(unix)]
const SILENCE: u32 = 10;

/// A child's end of the heartbeat: closed (emptied) to stop it.
#[cfg(unix)]
type Pipe = Arc<Mutex<Option<std::process::ChildStdin>>>;

/// `pipe` among those this process writes a line a second on, from one
/// thread: a line missing for `SILENCE` seconds is how `child.sh` learns of a
/// test process killed with one of its threads stuck (its files stay open).
#[cfg(unix)]
fn beat(pipe: std::process::ChildStdin) -> Pipe {
    static PIPES: Mutex<Vec<Pipe>> = Mutex::new(Vec::new());
    static BEATING: std::sync::Once = std::sync::Once::new();
    let pipe = Arc::new(Mutex::new(Some(pipe)));
    PIPES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(pipe.clone());
    BEATING.call_once(|| {
        let beating = std::thread::Builder::new().name("heartbeat".into());
        let _ = beating.spawn(|| {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                // A pipe closed here or by its script's end is forgotten.
                PIPES
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|pipe| {
                        let mut pipe = pipe.lock().unwrap_or_else(|e| e.into_inner());
                        if pipe.as_mut().is_some_and(|p| p.write_all(b"\n").is_err()) {
                            *pipe = None;
                        }
                        pipe.is_some()
                    });
            }
        });
    });
    pipe
}

/// A process the tests start, ended with them however they end: on Unix it
/// runs under `child.sh`, fed this process's heartbeat. On Windows it is the
/// process itself, killed when stopped, and left behind by a killed test.
pub struct Tended {
    child: Child,
    grace: u32,
    #[cfg(unix)]
    pipe: Pipe,
}

impl Tended {
    /// `program`, as `set` completes it (arguments, environment, folder,
    /// output; never its input). Stopped by SIGTERM and, `grace` seconds
    /// later, SIGKILL (0: SIGKILL at once); every mount at or below `drives`
    /// (a device's own folder) is detached once it ended.
    pub fn start(
        program: &Path,
        grace: u32,
        drives: Option<&Path>,
        set: impl FnOnce(&mut Command),
    ) -> Tended {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let mut command = Command::new("bash");
            command
                .arg(child_script())
                .args([SILENCE.to_string(), grace.to_string()])
                .arg(drives.map_or(std::ffi::OsStr::new("-"), Path::as_os_str))
                .arg(program);
            set(&mut command);
            // A group of its own: no signal sent to the tests' group (Ctrl-C,
            // `timeout`) reaches the script, which then sees the tests end.
            command.stdin(Stdio::piped()).process_group(0);
            let mut child = command.spawn().expect("bash, for child.sh");
            let pipe = beat(child.stdin.take().expect("piped"));
            Tended { child, grace, pipe }
        }
        #[cfg(not(unix))]
        {
            let _ = drives;
            let mut command = Command::new(program);
            set(&mut command);
            let child = command.spawn().expect("the program");
            Tended { child, grace }
        }
    }

    /// The script's process on Unix (its command is its child), the
    /// program's own on Windows.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// How it ended, once it has.
    pub fn ended(&mut self) -> Option<Ended> {
        let status = self.child.try_wait().ok()??;
        // The script exits as its command did: 137 is SIGKILL's.
        #[cfg(unix)]
        let ended = match status.code() {
            Some(137) | None => Ended::Killed,
            Some(code) => Ended::Exited(code),
        };
        #[cfg(not(unix))]
        let ended = Ended::Exited(status.code().unwrap_or(1));
        Some(ended)
    }

    /// Stops it, and says how it ended. The script is never killed itself
    /// (its command would be left running): it is waited for.
    pub fn stop(&mut self) -> Ended {
        if let Some(ended) = self.ended() {
            return ended;
        }
        #[cfg(unix)]
        self.pipe.lock().unwrap_or_else(|e| e.into_inner()).take();
        #[cfg(not(unix))]
        let _ = self.child.kill();
        let deadline = Instant::now() + Duration::from_secs(u64::from(self.grace) + 10);
        loop {
            if let Some(ended) = self.ended() {
                return if cfg!(unix) { ended } else { Ended::Killed };
            }
            if Instant::now() > deadline {
                assert!(std::thread::panicking(), "child.sh never ended");
                return Ended::Killed;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Tended {
    fn drop(&mut self) {
        self.stop();
    }
}

pub const ADMIN: &str = "admin@test.theca";
pub const PASSWORD: &str = "a long password";

/// The server: a process of the test's own, killed when dropped.
pub struct Server {
    child: Tended,
    pub url: String,
    /// Its own database's URL (a test reads it, as the app's tests do).
    pub database: String,
    _dir: tempfile::TempDir,
}

impl Server {
    /// Gone, as a server down or unreachable.
    pub fn kill(&mut self) {
        self.child.stop();
    }

    /// The process tending it: on Unix `child.sh`, whose child the server is.
    pub fn script_pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.child.stop();
        // Its database too: a few dozen MB each, they filled the dev Postgres.
        drop_database(&self.database);
    }
}

/// Drops a test server's database (by its URL), on a thread of its own: this
/// runs inside a test's runtime too.
pub fn drop_database(database: &str) {
    if let Ok(admin) = std::env::var("DATABASE_URL") {
        let name = database.rsplit_once('/').unwrap().1.to_owned();
        let _ = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let mut admin = sqlx::PgConnection::connect(&admin).await?;
                let drop = format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)");
                admin.execute(sqlx::AssertSqlSafe(drop)).await.map(|_| ())
            })
        })
        .join();
    }
}

fn server_binary() -> PathBuf {
    std::env::var_os("THECA_SERVER").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/theca-server"),
        PathBuf::from,
    )
}

/// A free port, never the same twice in this process: a port freed here can
/// come back from the OS to a parallel test before the server given it binds
/// it (and that test then talks to another test's server).
pub fn free_port() -> u16 {
    static TAKEN: Mutex<BTreeSet<u16>> = Mutex::new(BTreeSet::new());
    loop {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        if TAKEN.lock().unwrap().insert(port) {
            return port;
        }
    }
}

/// Bytes a connection carries either way before the proxy cuts it, once told
/// to: API calls pass, a chunk of a bigger file never does.
const CUT_AFTER: usize = 256 * 1024;

/// What the proxy does to connections.
pub struct Wire {
    /// Cut each past `CUT_AFTER` bytes either way (a transfer cut mid-stream,
    /// as a network would).
    pub cut: AtomicBool,
    /// Bytes all of them may still carry before stalling, until raised (a
    /// transfer stalled once some of it went through).
    pub budget: AtomicUsize,
}

impl Default for Wire {
    fn default() -> Self {
        Self {
            cut: AtomicBool::new(false),
            budget: AtomicUsize::new(usize::MAX),
        }
    }
}

/// A TCP proxy on `listen` to the server on `upstream`, as `wire` says.
fn proxy(listen: u16, upstream: u16, wire: Arc<Wire>) {
    let listener = TcpListener::bind(("127.0.0.1", listen)).unwrap();
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let Ok(server) = TcpStream::connect(("127.0.0.1", upstream)) else {
                continue;
            };
            let pumps = [
                (client.try_clone().unwrap(), server.try_clone().unwrap()),
                (server, client),
            ];
            for (mut from, mut to) in pumps {
                let wire = wire.clone();
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 16 * 1024];
                    let mut carried = 0;
                    while let Ok(n) = from.read(&mut buffer) {
                        carried += n;
                        if n == 0 || (wire.cut.load(Ordering::SeqCst) && carried > CUT_AFTER) {
                            break;
                        }
                        while wire
                            .budget
                            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |b| b.checked_sub(n))
                            .is_err()
                        {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        if to.write_all(&buffer[..n]).is_err() {
                            break;
                        }
                    }
                    let _ = from.shutdown(Shutdown::Both);
                    let _ = to.shutdown(Shutdown::Both);
                });
            }
        }
    });
}

/// A fresh database on `DATABASE_URL`'s Postgres (scripts/dev.sh's, CI's),
/// and the server on it.
pub async fn server() -> Server {
    server_with(None).await
}

/// The server, behind a proxy doing what `wire` says.
pub async fn server_with(wire: Option<Arc<Wire>>) -> Server {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    dotenvy::from_path(Path::new(env!("CARGO_MANIFEST_DIR")).join(".env")).ok();
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL: run scripts/dev.sh");
    let name = format!("computer_{}", Uuid::now_v7().simple());
    let mut admin = sqlx::PgConnection::connect(&base).await.unwrap();
    let create = format!("CREATE DATABASE {name}");
    admin.execute(sqlx::AssertSqlSafe(create)).await.unwrap();
    let database = format!("{}/{name}", base.rsplit_once('/').unwrap().0);
    let dir = tempfile::tempdir().unwrap();
    let file = |n: &str, content: &str| {
        let path = dir.path().join(n);
        std::fs::write(&path, content).unwrap();
        path
    };
    let port = free_port();
    // Behind the proxy, its public address (presigned URLs too) is the proxy's.
    let public = match wire {
        Some(wire) => {
            let listen = free_port();
            proxy(listen, port, wire);
            listen
        }
        None => port,
    };
    let url = format!("http://127.0.0.1:{public}");
    let binary = server_binary();
    assert!(
        binary.exists(),
        "{}: scripts/ci.sh test builds it (from the pinned core), or set THECA_SERVER",
        binary.display()
    );
    // No grace: a server gone is a server killed (`Server::kill`).
    let child = Tended::start(&binary, 0, None, |server| {
        server
            .current_dir(dir.path())
            .env("DATABASE_URL__FILE", file("database_url", &database))
            .env("SERVER_KEY__FILE", file("server_key", &"1".repeat(64)))
            .env("DATA_KEY__FILE", file("data_key", &"2".repeat(64)))
            .env("PUBLIC_URL", &url)
            .env("LISTEN_ADDR", format!("127.0.0.1:{port}"))
            .env("SETUP_OPEN", "true")
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null());
    });
    let mut server = Server {
        child,
        url,
        database,
        _dir: dir,
    };
    for _ in 0..300 {
        // It listens once migrated and configured.
        if Client::new(&server.url)
            .unwrap()
            .check_version()
            .await
            .is_ok()
        {
            return server;
        }
        if let Some(ended) = server.child.ended() {
            panic!("the server exited: {ended:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the server didn't become healthy");
}

/// A server set up, an admin logged in, and one library.
pub struct World {
    pub server: Server,
    pub admin: Client,
    pub library: api::Library,
    pub organization: Uuid,
    /// The storage's folder (kept while the world lives).
    storage: tempfile::TempDir,
}

pub async fn world() -> World {
    world_on(server().await).await
}

/// A world whose every connection goes through a proxy its `Wire` cuts or
/// stalls.
pub async fn world_cut() -> (World, Arc<Wire>) {
    let wire = Arc::new(Wire::default());
    (world_on(server_with(Some(wire.clone())).await).await, wire)
}

async fn world_on(server: Server) -> World {
    let storage = tempfile::tempdir().unwrap();
    let mut admin = Client::new(&server.url).unwrap();
    let setup: api::Setup = serde_json::from_value(json!({
        "admin_email": ADMIN,
        "admin_password": PASSWORD,
        "organization": "Acme",
        "storages": [{ "name": "main", "implementation": "filesystem", "path": storage.path() }],
    }))
    .unwrap();
    let me = admin.setup(&setup, None).await.unwrap();
    let organization = me.organizations[0].organization;
    let library = admin.create_library(organization, "Docs").await.unwrap();
    World {
        server,
        admin,
        library,
        organization,
        storage,
    }
}

/// A sync client: its state, its folder, its engine running.
pub struct Device {
    /// Its app password, as the account's list shows it (none when the test
    /// gave one).
    pub app_password: Option<Uuid>,
    pub state_path: PathBuf,
    pub folder: PathBuf,
    pub account: Account,
    /// Its engine's handle (what it offers, a wake-up), kept across restarts.
    pub handle: Handle,
    /// Its name, on its engine's log lines (every device's pair is 1).
    name: String,
    runs: Runs,
    stop: watch::Sender<bool>,
    engine: Option<JoinHandle<()>>,
    /// Its engine when a child's (`Runs::Child`).
    daemon: Option<Tended>,
    /// A test made its engine call into its own drive (`expect_own_calls`).
    own_calls_expected: bool,
    _dir: tempfile::TempDir,
}

/// Where a device's engine runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runs {
    /// In this process, as a task.
    Here,
    /// In a child `theca run`: on Linux, every engine that may mount a drive.
    /// A process that serves a FUSE mount and calls into it can't be killed
    /// while it waits on itself (theca/computer#1): this one is only ever a
    /// drive's client, as a user's programs are. `handle` reaches no child.
    Child,
}

/// Where a drive's engine runs.
const DRIVE_RUNS: Runs = if cfg!(target_os = "linux") {
    Runs::Child
} else {
    Runs::Here
};

/// The drive's scenarios run where its backend is (Linux: `/dev/fuse` and
/// `fusermount3`; Windows: always), and skip without it, unless
/// `THECA_TEST_DRIVE` says it must be there (GitHub's runners). Safe to run
/// anywhere: a drive's engine is never this process (`Runs::Child`).
#[cfg(any(target_os = "linux", windows))]
pub fn fuse() -> bool {
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

impl Drop for Device {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if self.runs == Runs::Here {
            return;
        }
        // Stopped, and nothing left mounted, before its folder goes with
        // `_dir`: a daemon's own drives, and one a `theca` a test ran and
        // killed left dead (the script detaches what is below the folder).
        self.daemon.take();
        #[cfg(unix)]
        if let Ok(own) = self.dir().canonicalize() {
            detach_below(&own);
        }
        if std::thread::panicking() {
            self.print_said();
        } else {
            self.never_called_its_own_drive();
        }
    }
}

impl World {
    /// A file uploaded through the API (as another client would).
    pub async fn upload(&self, library: &api::Library, name: &str, content: &[u8]) {
        theca_client::upload(
            &self.admin,
            &theca_client::MemorySource(content.to_vec()),
            theca_client::Target::New {
                library: library.id,
                parent: library.root,
                name: theca_common::name::Name::new(name).unwrap(),
            },
            &theca_client::UploadOptions::default(),
            |_| {},
            &theca_client::Cancel::default(),
        )
        .await
        .unwrap();
    }

    /// A second account, a member of the world's organization (not its
    /// owner): its client, logged in, and its user in the organization.
    /// Registering needs the address verified, done through the server's
    /// database (no email here).
    pub async fn member(&self, email: &str) -> (Client, Uuid) {
        let database = &self.server.database;
        // Registration needs SMTP set: a server nobody listens on (emails
        // fail in the background; the address is verified below instead).
        let settings = |smtp: Option<&str>| {
            json!({
                "registration": "open",
                "smtp_host": smtp,
                "smtp_port": "9",
                "smtp_tls": "none",
                "smtp_from": smtp.map(|_| "theca@test.theca"),
            })
        };
        let patch = reqwest::Method::PATCH;
        let _: serde_json::Value = self
            .admin
            .call(
                patch.clone(),
                "/api/v1/admin/settings",
                Some(&settings(Some("127.0.0.1"))),
            )
            .await
            .unwrap();
        let register = json!({ "email": email, "password": PASSWORD });
        let _: () = self
            .admin
            .call(
                reqwest::Method::POST,
                "/api/v1/auth/register",
                Some(&register),
            )
            .await
            .unwrap();
        let _: serde_json::Value = self
            .admin
            .call(patch, "/api/v1/admin/settings", Some(&settings(None)))
            .await
            .unwrap();
        let mut db = sqlx::PgConnection::connect(database).await.unwrap();
        sqlx::query("UPDATE account SET verified = true WHERE email = $1")
            .bind(email)
            .execute(&mut db)
            .await
            .unwrap();
        let mut client = Client::new(&self.server.url).unwrap();
        client.login(email, PASSWORD, None).await.unwrap();
        let path = format!("/api/v1/organizations/{}/members", self.organization);
        let member: api::Member = self
            .admin
            .call(
                reqwest::Method::POST,
                &path,
                Some(&json!({ "email": email })),
            )
            .await
            .unwrap();
        (client, member.id)
    }

    /// Gives `user` write access to `library`, or takes it away.
    pub async fn set_member(&self, library: &api::Library, user: Uuid, member: bool) {
        let path = format!("/api/v1/libraries/{}/members/{user}", library.id);
        if member {
            let _: serde_json::Value = self
                .admin
                .call(
                    reqwest::Method::PUT,
                    &path,
                    Some(&json!({ "role": "write" })),
                )
                .await
                .unwrap();
        } else {
            let _: () = self
                .admin
                .call(reqwest::Method::DELETE, &path, None::<&()>)
                .await
                .unwrap();
        }
    }

    /// A device of the admin's, syncing `library` (the world's by default).
    pub async fn device(&self, name: &str) -> Device {
        self.device_for(name, &self.library, true).await
    }

    /// A device whose app password is in its state (`--no-keyring`): a child
    /// `theca` runs on it, and can't read this process's keychain (the mock),
    /// nor, on macOS, an entry another binary wrote without a prompt.
    pub async fn device_without_keychain(&self, name: &str) -> Device {
        self.device_made(name, &self.library, true, false, Mode::Sync, Runs::Here)
            .await
    }

    pub async fn device_for(&self, name: &str, library: &api::Library, start: bool) -> Device {
        self.device_made(name, library, start, true, Mode::Sync, Runs::Here)
            .await
    }

    /// A syncing device whose engine is a child `theca run` (`Runs::Child`):
    /// one that may come to mount a drive. Without `start`, `restart` starts
    /// it.
    #[cfg(unix)]
    pub async fn device_in_a_child(&self, name: &str, start: bool) -> Device {
        self.device_made(name, &self.library, start, false, Mode::Sync, Runs::Child)
            .await
    }

    /// A device logged in, its app password in its state (a child `theca`
    /// runs on it), no library added yet: its folder, state and account.
    pub async fn logged_in(&self, name: &str) -> (tempfile::TempDir, PathBuf, Account) {
        let created = self
            .admin
            .create_app_password(&api::CreateAppPassword {
                name: name.into(),
                organization: None,
                library: None,
                read_only: false,
            })
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.sqlite");
        let state = State::open(&state_path).unwrap();
        let account = ops::login(
            &state,
            &self.server.url,
            name,
            Some(dir.path().join("theca")),
            Some(created.password),
            false,
            // New libraries listed only, unless a test says otherwise.
            Some(NewLibraries::Silent),
            NewLibraries::Silent,
            |_| panic!("a token needs no browser"),
        )
        .await
        .unwrap();
        (dir, state_path, account)
    }

    /// A device whose pair is a drive. On Linux its engine is a child
    /// (`Runs::Child`), which reads its app password in the state.
    pub async fn drive(&self, name: &str) -> Device {
        let keyring = !cfg!(target_os = "linux");
        self.device_made(name, &self.library, true, keyring, Mode::Drive, DRIVE_RUNS)
            .await
    }

    /// A drive whose app password is in its state: a child `theca` runs on
    /// it (`device_without_keychain`).
    pub async fn drive_without_keychain(&self, name: &str) -> Device {
        self.device_made(name, &self.library, true, false, Mode::Drive, DRIVE_RUNS)
            .await
    }

    async fn device_made(
        &self,
        name: &str,
        library: &api::Library,
        start: bool,
        keyring: bool,
        mode: Mode,
        runs: Runs,
    ) -> Device {
        let created = self
            .admin
            .create_app_password(&api::CreateAppPassword {
                name: name.into(),
                organization: None,
                library: None,
                read_only: false,
            })
            .await
            .unwrap();
        let mut device = self
            .device_in(name, library, &created.password, start, keyring, mode, runs)
            .await;
        device.app_password = Some(created.app_password.id);
        device
    }

    pub async fn device_with(
        &self,
        name: &str,
        library: &api::Library,
        password: &str,
        start: bool,
        keyring: bool,
    ) -> Device {
        let here = Runs::Here;
        self.device_in(name, library, password, start, keyring, Mode::Sync, here)
            .await
    }

    #[allow(clippy::too_many_arguments)] // a device's every choice, made by the few above
    async fn device_in(
        &self,
        name: &str,
        library: &api::Library,
        password: &str,
        start: bool,
        keyring: bool,
        mode: Mode,
        runs: Runs,
    ) -> Device {
        keychain();
        // Windows takes no sync root in the temp folder (`UNABLE_TO_MASK_PATH`):
        // a drive's folders in the user's profile, as a user's are.
        #[cfg(windows)]
        let dir = match mode {
            Mode::Drive => tempfile::Builder::new()
                .prefix("theca-drive-")
                .tempdir_in(std::env::var("USERPROFILE").unwrap())
                .unwrap(),
            Mode::Sync => tempfile::tempdir().unwrap(),
        };
        #[cfg(not(windows))]
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.sqlite");
        let folder = dir.path().join("theca");
        let state = State::open(&state_path).unwrap();
        // The app password as a script gives it (`THECA_TOKEN`): no flow.
        let account = ops::login(
            &state,
            &self.server.url,
            name,
            Some(folder.clone()),
            Some(password.into()),
            keyring,
            // New libraries listed only, unless a test says otherwise.
            Some(NewLibraries::Silent),
            NewLibraries::Silent,
            |_| panic!("a token needs no browser"),
        )
        .await
        .unwrap();
        let pair = ops::add(
            &state,
            &account,
            &library.id.to_string(),
            Some(folder.join(&library.name)),
            false,
            mode,
        )
        .await
        .unwrap();
        let (stop, stopped) = watch::channel(false);
        let mut device = Device {
            app_password: None,
            folder: pair.path.clone(),
            state_path,
            account,
            handle: Handle::default(),
            name: name.into(),
            runs,
            stop,
            engine: None,
            daemon: None,
            own_calls_expected: false,
            _dir: dir,
        };
        if start {
            device.start(stopped);
        }
        device
    }

    /// The library's content on the server: path → content (none: a folder).
    pub async fn remote(&self, library: &api::Library) -> BTreeMap<String, Option<Vec<u8>>> {
        // A node listed, then gone before its download: listed again.
        loop {
            if let Some(tree) = self.try_remote(library).await {
                return tree;
            }
        }
    }

    async fn try_remote(
        &self,
        library: &api::Library,
    ) -> Option<BTreeMap<String, Option<Vec<u8>>>> {
        let mut tree = BTreeMap::new();
        let mut lines = Box::pin(self.admin.tree(library.root).await.unwrap());
        while let Some(node) = lines.next().await {
            let node = node.unwrap();
            let content = if node.is_file {
                let sink = MemorySink::default();
                let options = DownloadOptions::default();
                download(
                    &self.admin,
                    node.id,
                    None,
                    &sink,
                    &options,
                    |_| {},
                    &Cancel::default(),
                )
                .await
                .ok()?;
                Some(sink.0.into_inner().unwrap())
            } else {
                None
            };
            tree.insert(node.path, content);
        }
        Some(tree)
    }

    /// Waits until every device's folder holds what the library holds.
    pub async fn converge(&self, devices: &[&Device]) {
        self.converge_on(&self.library, devices).await;
    }

    pub async fn converge_on(&self, library: &api::Library, devices: &[&Device]) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let remote = self.remote(library).await;
            let locals: Vec<_> = devices.iter().map(|d| local(&d.folder)).collect();
            if locals.iter().all(|l| *l == remote) {
                return;
            }
            if Instant::now() > deadline {
                let notices: Vec<_> = devices
                    .iter()
                    .map(|d| {
                        let state = State::open(&d.state_path).unwrap();
                        let pair = state.pairs().unwrap().remove(0);
                        (
                            state.notices().unwrap(),
                            pair,
                            state.remote(pair_id(&state)).unwrap(),
                            state.entries(pair_id(&state)).unwrap().len(),
                        )
                    })
                    .collect();
                panic!(
                    "no convergence\nremote: {:?}\nlocals: {:?}\nnotices: {notices:?}",
                    names(&remote),
                    locals.iter().map(names).collect::<Vec<_>>()
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

/// Polls `check` until it holds (or panics after 30 s, with `what`).
pub async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(30);
    while !check().await {
        assert!(Instant::now() < deadline, "never happened: {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// What a task that panicked said.
pub fn panic_message(panicked: tokio::task::JoinError) -> String {
    let panic = panicked.into_panic();
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default()
}

/// How long a call into a drive may take: a drive that answers nothing fails
/// its test, never hangs it.
pub const DRIVE_ANSWERS: Duration = Duration::from_secs(30);

/// `call`, which reaches into a drive's folder, on a thread of its own, and
/// what it returned: or the test fails once `DRIVE_ANSWERS` has passed. The
/// thread then waits on, until the test's cleanup stops the drive's engine
/// (the call fails at once when its daemon is gone); nothing waits for it.
pub async fn in_drive<T: Send + 'static>(
    what: &str,
    call: impl FnOnce() -> T + Send + 'static,
) -> T {
    in_drive_within(DRIVE_ANSWERS, what, call).await
}

/// `in_drive`, given `limit`.
pub async fn in_drive_within<T: Send + 'static>(
    limit: Duration,
    what: &str,
    call: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (done, ended) = tokio::sync::oneshot::channel();
    let calling = std::thread::spawn(move || {
        let _ = done.send(call());
    });
    match tokio::time::timeout(limit, ended).await {
        Ok(Ok(returned)) => returned,
        // The call panicked: its own panic, here.
        Ok(Err(_)) => match calling.join() {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(()) => unreachable!("it returned without its value"),
        },
        Err(_) => panic!("the drive didn't answer in {limit:?}: {what}"),
    }
}

impl Device {
    /// Stops its engine (offline, or the app quit).
    pub async fn stop(&mut self) {
        let _ = self.stop.send(true);
        // A child's: told to stop, it ends by itself, or the test fails here.
        if let Some(mut daemon) = self.daemon.take() {
            let ended = tokio::task::spawn_blocking(move || daemon.stop())
                .await
                .unwrap();
            if ended != Ended::Exited(0) {
                self.print_said();
            }
            assert_eq!(ended, Ended::Exited(0), "the engine stops");
            self.never_called_its_own_drive();
        }
        // Gone once it returns: what the test changes next, it changes offline.
        if let Some(engine) = self.engine.take() {
            tokio::time::timeout(Duration::from_secs(10), engine)
                .await
                .expect("the engine stops")
                .unwrap();
        }
    }

    /// Starts its engine again.
    pub fn restart(&mut self) {
        let (stop, stopped) = watch::channel(false);
        self.stop = stop;
        self.start(stopped);
    }

    pub fn notices(&self) -> Vec<String> {
        self.state()
            .notices()
            .unwrap()
            .into_iter()
            .map(|n| format!("{}: {}", n.path.unwrap_or_default(), n.message))
            .collect()
    }

    /// Its own folder, which holds all it has: its state, its pairs' folders.
    fn dir(&self) -> &Path {
        self.state_path.parent().expect("in its folder")
    }

    /// Prints what its child engine wrote, for a failing test.
    fn print_said(&self) {
        eprintln!("--- {}'s `theca run` said:\n{}", self.name, self.said());
    }

    /// What its child engine wrote (its log's lines, at `debug`): nothing
    /// captures a child's output.
    fn said(&self) -> String {
        std::fs::read_to_string(self.dir().join(OUTPUT)).unwrap_or_default()
    }

    /// Its child engine never called into a drive it served: the thread
    /// making such a call waits on its own process, which then can't be
    /// killed (the drive's warning, `fuse::OWN_CALL`).
    fn never_called_its_own_drive(&self) {
        #[cfg(target_os = "linux")]
        if !self.own_calls_expected && self.called_its_own_drive() {
            self.print_said();
            panic!("{}'s engine {}", self.name, theca::drive::fuse::OWN_CALL);
        }
        // Only a FUSE drive says it.
        #[cfg(not(target_os = "linux"))]
        let _ = self.own_calls_expected;
    }

    /// Its child engine said it called into a drive it served.
    #[cfg(target_os = "linux")]
    pub fn called_its_own_drive(&self) -> bool {
        self.said().contains(theca::drive::fuse::OWN_CALL)
    }

    /// The test makes its engine call into its own drive, on purpose: its
    /// stop won't fail for it.
    pub fn expect_own_calls(&mut self) {
        self.own_calls_expected = true;
    }

    /// The process tending its child engine: on Unix `child.sh`, whose child
    /// `theca` is.
    pub fn script_pid(&self) -> Option<u32> {
        self.daemon.as_ref().map(Tended::id)
    }

    /// `theca run` on its state, a child under `child.sh`: given 10 s to stop
    /// once told (what an in-process engine gets), its drives detached after.
    fn daemon(&self) -> Tended {
        let out = std::fs::File::options()
            .create(true)
            .append(true)
            .open(self.dir().join(OUTPUT))
            .unwrap();
        let err = out.try_clone().unwrap();
        // As the mount table spells it: the script matches mounts by path.
        let own = self.dir().canonicalize().unwrap();
        let theca = Path::new(env!("CARGO_BIN_EXE_theca"));
        Tended::start(theca, 10, Some(&own), |theca| {
            theca
                .arg("--state")
                .arg(&self.state_path)
                .arg("run")
                .env("RUST_LOG", "theca=debug")
                .stdout(out)
                .stderr(err);
        })
    }

    fn start(&mut self, stopped: watch::Receiver<bool>) {
        if self.runs == Runs::Child {
            self.daemon = Some(self.daemon());
            return;
        }
        let path = self.state_path.clone();
        let handle = self.handle.clone();
        // The engine's target: `RUST_LOG=theca=debug` shows it.
        let span = tracing::info_span!(target: "theca", "device", name = self.name);
        self.engine = Some(tokio::spawn(
            async move {
                engine::run(path, handle, stopped).await.unwrap();
            }
            .instrument(span),
        ));
    }

    pub fn state(&self) -> State {
        State::open(&self.state_path).unwrap()
    }

    pub fn pair(&self) -> Pair {
        self.state().pairs().unwrap().remove(0)
    }

    /// The libraries its engine offers (not synced), by ID.
    pub fn offered(&self) -> Vec<Uuid> {
        let offered = self.handle.offered.lock().unwrap();
        offered
            .get(&self.account.id)
            .map(|o| o.iter().map(|o| o.library).collect())
            .unwrap_or_default()
    }

    pub fn path(&self, path: &str) -> PathBuf {
        self.folder.join(path)
    }

    /// Where a drive's rounds work: a FUSE drive's backing folder, a
    /// Windows drive's own folder.
    #[cfg(any(target_os = "linux", windows))]
    pub fn backing(&self) -> PathBuf {
        theca::drive::root_of(&self.state(), &self.pair()).unwrap()
    }

    /// Writes a file, its time set in the past: the engine doesn't wait for
    /// it to settle.
    pub fn write(&self, path: &str, content: &str) {
        let full = self.path(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, content).unwrap();
        settle(&full);
    }
}

/// Sets a file's modification time a few seconds back (the engine waits for
/// files modified less than 2 s ago), a distinct one each call.
pub fn settle(path: &Path) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let back = Duration::from_secs(10) + Duration::from_millis(N.fetch_add(1, Ordering::Relaxed));
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(std::time::SystemTime::now() - back)
        .unwrap();
}

/// A local folder's content, as `remote` gives the library's (names in NFC).
pub fn local(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut tree = BTreeMap::new();
    let Ok(scan) = theca::local::scan(root) else {
        return tree;
    };
    for item in scan.items {
        let path: Vec<String> = item
            .path
            .iter()
            .map(|n| theca_common::name::Name::new(n).map_or(n.clone(), |n| n.as_str().to_owned()))
            .collect();
        let full = item.path.iter().fold(root.to_path_buf(), |p, n| p.join(n));
        let content = item
            .is_file
            .then(|| std::fs::read(&full).unwrap_or_default());
        tree.insert(path.join("/"), content);
    }
    tree
}

fn names(tree: &BTreeMap<String, Option<Vec<u8>>>) -> Vec<String> {
    tree.iter()
        .map(|(p, c)| match c {
            Some(c) => format!("{p} ({} B)", c.len()),
            None => format!("{p}/"),
        })
        .collect()
}

fn pair_id(state: &State) -> i64 {
    state.pairs().unwrap()[0].id
}

/// The files `files` makes for the base URL they're served on, served on
/// 127.0.0.1 until the process ends; that URL, and the requests counted.
pub fn serve(
    files: impl FnOnce(&str) -> std::collections::HashMap<&'static str, Vec<u8>>,
) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let files = files(&base);
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            counted.fetch_add(1, Ordering::SeqCst);
            let mut request = String::new();
            let mut reader = std::io::BufReader::new(&stream);
            if std::io::BufRead::read_line(&mut reader, &mut request).is_err() {
                continue;
            }
            // The headers, up to the empty line.
            let mut line = String::new();
            while std::io::BufRead::read_line(&mut reader, &mut line).is_ok_and(|n| n > 2) {
                line.clear();
            }
            let path = request.split(' ').nth(1).unwrap_or("");
            let response = match files.get(path) {
                Some(body) => {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    [head.as_bytes(), body].concat()
                }
                None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            };
            let _ = stream.write_all(&response);
        }
    });
    (base, requests)
}
