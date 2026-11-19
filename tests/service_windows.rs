//! The daemon as a Windows service: `theca service on` registers `theca run
//! --service` for a throwaway local user (made here, with a password of its
//! own), and the service runs as them, syncs, stops as asked and goes with
//! `theca service off`. Needs an administrator (GitHub's Windows runner is
//! one): it runs only where `THECA_TEST_WINDOWS_SERVICE` says so (test.yml), as
//! a developer's own machine would get a user and a service made and removed.
//!
//! Not covered, and said so: a self-update under the service (the binary it
//! runs would need this run's test key built in, which only the app's test
//! build has), and the app password in Credential Manager (written as the
//! throwaway user, who never logs on): the login here keeps it in the state
//! file.

#![cfg(windows)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::*;
use theca_common::api;
use uuid::Uuid;

/// Whether this machine is one the test may make users and services on
/// (GitHub's Windows runner, test.yml): elsewhere it is skipped.
fn allowed() -> bool {
    let allowed =
        std::env::var("THECA_TEST_WINDOWS_SERVICE").is_ok_and(|v| !v.is_empty() && v != "0");
    if !allowed {
        eprintln!("skipped: THECA_TEST_WINDOWS_SERVICE isn't set (the runner's test.yml sets it)");
    }
    allowed
}

/// A throwaway local user, a folder they have the run of, and `theca.exe`
/// copied there (the user needs no access to the build's folder).
struct Machine {
    user: String,
    password: String,
    dir: PathBuf,
    /// A folder of Program Files of this run's own, where only administrators
    /// write (made by the test that needs it, removed with the machine).
    program_files: PathBuf,
    /// The service's name.
    service: String,
}

fn windows(program: &str, args: &[&str]) -> (bool, String) {
    let out = Command::new(program).args(args).output().unwrap();
    (out.status.success(), text(&out))
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

impl Machine {
    /// The user, the folder and the copy of `theca`; the library `world` holds
    /// synced into `<dir>\Docs` by the user's state (the app password in it).
    async fn new(world: &World) -> Self {
        let id = Uuid::now_v7().simple().to_string();
        let machine = Self {
            user: format!("thecasvc{}", &id[24..]),
            // Complex enough for a local policy.
            // 14 characters at most: `net user` asks a question about older
            // Windows beyond that, which nothing here answers. Upper, lower,
            // a digit and a symbol, for a local policy.
            password: format!("Th3ca!{}", &Uuid::new_v4().simple().to_string()[..8]),
            dir: PathBuf::from(r"C:\ProgramData").join(format!("theca-test-{id}")),
            program_files: PathBuf::from(
                std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into()),
            )
            .join(format!("theca-test-{id}")),
            service: format!("theca-thecasvc{}", &id[24..]),
        };
        let (ok, said) = windows("net", &["user", &machine.user, &machine.password, "/add"]);
        assert!(ok, "net user /add: {said}");
        std::fs::create_dir_all(&machine.dir).unwrap();
        let (ok, said) = windows(
            "icacls",
            &[
                machine.dir.to_str().unwrap(),
                "/grant",
                &format!("{}:(OI)(CI)F", machine.user),
            ],
        );
        assert!(ok, "icacls: {said}");
        std::fs::copy(env!("CARGO_BIN_EXE_theca"), machine.exe()).unwrap();

        // Logged in (the app password in the state file, where the user's
        // service finds it), and the library added.
        let created = world
            .admin
            .create_app_password(&api::CreateAppPassword {
                name: "service".into(),
                organization: None,
                library: None,
                read_only: false,
            })
            .await
            .unwrap();
        let out = Command::new(machine.exe())
            .arg("--state")
            .arg(machine.state())
            .args(["login", &world.server.url, "--no-keyring", "--folder"])
            .arg(machine.dir.join("theca"))
            .env("THECA_TOKEN", &created.password)
            .output()
            .unwrap();
        assert!(out.status.success(), "login: {}", text(&out));
        let out = machine.theca(&["add", "Docs", machine.docs().to_str().unwrap()], None);
        assert!(out.status.success(), "add: {}", text(&out));
        machine
    }

    fn exe(&self) -> PathBuf {
        self.dir.join("theca.exe")
    }

    fn state(&self) -> PathBuf {
        self.dir.join("state.sqlite")
    }

    fn docs(&self) -> PathBuf {
        self.dir.join("Docs")
    }

    /// `theca` on this machine's state, with `stdin` for what it reads.
    fn theca(&self, args: &[&str], stdin: Option<&str>) -> Output {
        self.theca_from(&self.exe(), args, stdin)
    }

    /// The same, from the binary at `exe`.
    fn theca_from(&self, exe: &Path, args: &[&str], stdin: Option<&str>) -> Output {
        self.theca_env(exe, args, stdin, &[])
    }

    /// The same, from the binary at `exe`, with `env` set for it.
    fn theca_env(
        &self,
        exe: &Path,
        args: &[&str],
        stdin: Option<&str>,
        env: &[(&str, &str)],
    ) -> Output {
        let mut command = Command::new(exe);
        command.envs(env.iter().copied());
        command.arg("--state").arg(self.state()).args(args);
        match stdin {
            Some(text) => {
                command.stdin(Stdio::piped());
                let mut child = command
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                std::io::Write::write_all(&mut child.stdin.take().unwrap(), text.as_bytes())
                    .unwrap();
                child.wait_with_output().unwrap()
            }
            None => command.stdin(Stdio::null()).output().unwrap(),
        }
    }

    /// `theca service on` for the user, with `password`.
    fn on(&self, password: &str) -> Output {
        self.theca(
            &["service", "on", "--account", &self.user],
            Some(&format!("{password}\r\n")),
        )
    }

    fn off(&self) -> Output {
        self.theca(&["service", "off", "--account", &self.user], None)
    }

    fn sc(&self, query: &str) -> (bool, String) {
        windows("sc", &[query, &self.service])
    }

    /// Every log the service and the commands wrote (beside the state).
    fn logs(&self) -> String {
        let mut all = String::new();
        if let Ok(files) = std::fs::read_dir(self.dir.join("logs")) {
            for file in files.flatten() {
                all.push_str(&std::fs::read_to_string(file.path()).unwrap_or_default());
            }
        }
        all
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("the service's logs:\n{}", self.logs());
            eprintln!("sc query: {}", self.sc("query").1);
        }
        let _ = windows("sc", &["stop", &self.service]);
        let _ = windows("sc", &["delete", &self.service]);
        let _ = windows("net", &["user", &self.user, "/delete"]);
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_dir_all(&self.program_files);
    }
}

/// Polls `done` for 30 s, then fails with `what`.
fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "never happened: {what}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// `theca run` on a state a service holds: refused, within 20 s.
fn run_is_refused(machine: &Machine) -> String {
    let mut child = Command::new(machine.exe())
        .arg("--state")
        .arg(machine.state())
        .arg("run")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("a second `theca run` kept running beside the service");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(!status.success());
    let mut said = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut said).unwrap();
    said
}

/// The service as a user would have it: registered as their account, started
/// with Windows and restarted after a failure (also one that isn't a crash),
/// running and syncing both ways; a second `theca run` is refused; stopped as
/// asked it reports no error; `off` removes it, twice without harm; and the
/// password never reached a log.
#[tokio::test(flavor = "multi_thread")]
async fn the_daemon_runs_as_a_windows_service_for_a_user() {
    if !allowed() {
        return;
    }
    let world = world().await;
    let machine = Machine::new(&world).await;

    let out = machine.on(&machine.password);
    assert!(out.status.success(), "service on: {}", text(&out));
    let (ok, config) = machine.sc("qc");
    assert!(ok, "{config}");
    assert!(config.contains("AUTO_START"), "{config}");
    assert!(config.contains("run --service"), "{config}");
    assert!(
        config.to_lowercase().contains(&machine.user.to_lowercase()),
        "the service doesn't run as the user: {config}"
    );
    let (_, failure) = machine.sc("qfailure");
    assert!(
        failure.contains("RESTART -- Delay = 5000 milliseconds"),
        "{failure}"
    );
    let (_, flag) = machine.sc("qfailureflag");
    assert!(
        flag.contains("TRUE"),
        "the restart ignores a stop that isn't a crash: {flag}"
    );
    let (_, status) = machine.sc("query");
    assert!(status.contains("RUNNING"), "{status}");

    // `on` again while it runs (a changed password, say): the service is
    // stopped, changed and started again, where a start of a running service
    // would fail.
    let out = machine.on(&machine.password);
    assert!(
        out.status.success(),
        "service on, a second time: {}",
        text(&out)
    );
    let (_, status) = machine.sc("query");
    assert!(status.contains("RUNNING"), "{status}");

    // It syncs, each way.
    std::fs::write(machine.docs().join("from-here.txt"), "from the machine").unwrap();
    eventually("a file made in the folder reaches the library", || async {
        world
            .remote(&world.library)
            .await
            .get("from-here.txt")
            .is_some_and(|c| c.as_deref() == Some(b"from the machine".as_slice()))
    })
    .await;
    world
        .upload(&world.library, "from-there.txt", b"from the library")
        .await;
    eventually("a file made in the library reaches the folder", || async {
        std::fs::read_to_string(machine.docs().join("from-there.txt"))
            .is_ok_and(|c| c == "from the library")
    })
    .await;

    // The state is the service's: a second `theca run` is refused.
    let said = run_is_refused(&machine);
    assert!(said.contains("already running"), "{said}");

    // The password was used once and written nowhere.
    let logs = machine.logs();
    assert!(!logs.is_empty(), "no log to look in");
    assert!(
        !logs.contains(&machine.password),
        "the password is in a log"
    );

    // Stopped as the manager asks: the engine's stop, reported as such.
    let (ok, said) = machine.sc("stop");
    assert!(ok || said.contains("STOP_PENDING"), "{said}");
    wait_until("the service stops", || {
        machine.sc("query").1.contains("STOPPED")
    });
    let (_, status) = machine.sc("query");
    assert!(
        status.contains("WIN32_EXIT_CODE    : 0"),
        "the service stopped with an error: {status}"
    );

    // Removed, and removing again is no failure.
    let out = machine.off();
    assert!(out.status.success(), "service off: {}", text(&out));
    assert!(!machine.sc("query").0, "the service is still there");
    let out = machine.off();
    assert!(out.status.success(), "a second off: {}", text(&out));
    assert!(text(&out).contains("isn't there"), "{}", text(&out));
}

/// A wrong password: the service isn't made, and the message says so.
#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_password_leaves_no_service() {
    if !allowed() {
        return;
    }
    let world = world().await;
    let machine = Machine::new(&world).await;
    let out = machine.on("Not-the-password-1!");
    assert!(
        !out.status.success(),
        "service on accepted a wrong password"
    );
    let said = text(&out);
    // The logon check's own message, before any service is made (the start's
    // says "the password is wrong, or ...").
    assert!(said.contains("the password is wrong for"), "{said}");
    assert!(!machine.sc("query").0, "the service was left behind");
    assert!(
        !machine.logs().contains("Not-the-password-1!"),
        "the password is in a log"
    );
}

/// A group isn't an account: `--account Users` is refused before the right is
/// granted to everyone in it, and no service is made.
#[tokio::test(flavor = "multi_thread")]
async fn a_group_is_not_an_account_for_the_service() {
    if !allowed() {
        return;
    }
    let world = world().await;
    let machine = Machine::new(&world).await;
    let out = machine.theca(
        &["service", "on", "--account", "Users"],
        Some("Whatever-1!\r\n"),
    );
    let said = text(&out);
    let left = windows("sc", &["query", "theca-Users"]).0;
    let _ = windows("sc", &["delete", "theca-Users"]);
    assert!(!out.status.success(), "service on took a group: {said}");
    assert!(said.contains("isn't a user account"), "{said}");
    assert!(!left, "a service was made for a group");
}

/// A state another process holds (the app, `theca run`): the service ends as
/// it starts, `service on` says so and leaves nothing behind, rather than
/// printing success for a service that stops a moment later.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_state_fails_service_on() {
    if !allowed() {
        return;
    }
    let world = world().await;
    let machine = Machine::new(&world).await;
    let mut holder = Command::new(machine.exe())
        .arg("--state")
        .arg(machine.state())
        .arg("run")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("the other process holds the state", || {
        theca::lock(&machine.state()).is_err()
    });
    let out = machine.on(&machine.password);
    let said = text(&out);
    let left = machine.sc("query").0;
    let _ = holder.kill();
    let _ = holder.wait();
    assert!(
        !out.status.success(),
        "service on succeeded on a held state: {said}"
    );
    assert!(said.contains("stopped as it started"), "{said}");
    assert!(!left, "the service was left behind");
}

/// An administrator's service has the full administrator token, so it may run
/// only a binary that only administrators can replace: from the folder the
/// account may write (`ProgramData` here) `service on` refuses and makes
/// nothing; from Program Files it registers the service, which runs. (A
/// standard account, as in the other tests, gets its service from anywhere.)
#[tokio::test(flavor = "multi_thread")]
async fn an_administrators_service_runs_only_a_binary_in_program_files() {
    if !allowed() {
        return;
    }
    let world = world().await;
    let machine = Machine::new(&world).await;
    let (ok, said) = windows(
        "net",
        &["localgroup", "Administrators", &machine.user, "/add"],
    );
    assert!(ok, "net localgroup Administrators /add: {said}");

    // Even when the environment claims this folder is Program Files (a program
    // of the account's could set it in its user registry): the folders come
    // from the operating system.
    let folder = machine.dir.to_str().unwrap();
    let out = machine.theca_env(
        &machine.exe(),
        &["service", "on", "--account", &machine.user],
        Some(&format!("{}\r\n", machine.password)),
        &[
            ("ProgramFiles", folder),
            ("ProgramFiles(x86)", folder),
            ("ProgramW6432", folder),
            ("SystemRoot", folder),
        ],
    );
    let said = text(&out);
    assert!(
        !out.status.success(),
        "service on took a binary the administrator can overwrite: {said}"
    );
    assert!(said.contains("can overwrite"), "{said}");
    assert!(
        !machine.sc("query").0,
        "a service was made for a binary the account can overwrite"
    );

    std::fs::create_dir_all(&machine.program_files).unwrap();
    let exe = machine.program_files.join("theca.exe");
    std::fs::copy(machine.exe(), &exe).unwrap();
    let out = machine.theca_from(
        &exe,
        &["service", "on", "--account", &machine.user],
        Some(&format!("{}\r\n", machine.password)),
    );
    assert!(
        out.status.success(),
        "service on from Program Files: {}",
        text(&out)
    );
    let (_, status) = machine.sc("query");
    assert!(status.contains("RUNNING"), "{status}");
    let out = machine.theca_from(&exe, &["service", "off", "--account", &machine.user], None);
    assert!(out.status.success(), "service off: {}", text(&out));
}
