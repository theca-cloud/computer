//! The daemon as a Linux service: `theca-cli`'s `.deb` installed on a machine
//! that runs systemd (a Debian 12 container, `--privileged`, systemd as its
//! PID 1) and syncing against the real test server, which the container
//! reaches on the host's network. What a user does with `theca@<user>`: enable
//! it, let it sync, upgrade the package, run `theca run` by hand beside it,
//! remove the package.
//!
//! Needs Docker and the package (`THECA_TEST_DEB`): `scripts/ci.sh service`
//! builds it and runs this, and `THECA_TEST_SERVICE=1` (release.yml, that
//! script) makes a missing one a failure, not a skip. The machine is Debian 12;
//! `THECA_TEST_SERVICE_IMAGE` names another base for a package built on a
//! system newer than the release's Ubuntu 22.04 (its glibc is newer than
//! Debian 12's).

#![cfg(target_os = "linux")]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use theca_common::api;
use uuid::Uuid;

/// The user the service runs as, and her library's name.
const USER: &str = "tester";
const UNIT: &str = "theca@tester.service";
const FOLDER: &str = "/home/tester/theca/Docs";

/// The package, when this machine can run the check (or fails when it must).
fn package() -> Option<PathBuf> {
    let required = std::env::var("THECA_TEST_SERVICE").is_ok_and(|v| !v.is_empty() && v != "0");
    let deb = std::env::var_os("THECA_TEST_DEB").map(PathBuf::from);
    let docker = Command::new("docker")
        .arg("info")
        .output()
        .is_ok_and(|o| o.status.success());
    match deb {
        Some(deb) if docker => Some(deb),
        _ => {
            assert!(
                !required,
                "THECA_TEST_SERVICE: no Docker, or no THECA_TEST_DEB"
            );
            eprintln!("skipped: no Docker, or no THECA_TEST_DEB (scripts/ci.sh service)");
            None
        }
    }
}

/// A Debian 12 booted with systemd, named for this run (Docker is shared by
/// every session on the machine): its container and its image go with it.
struct Machine {
    name: String,
    image: String,
}

impl Machine {
    fn start(deb: &Path) -> Self {
        let id = Uuid::now_v7().simple().to_string();
        let machine = Self {
            name: format!("theca-service-{id}"),
            image: format!("theca-service-{id}"),
        };
        // The packages systemd needs; the apt lists stay, for the install.
        let base = std::env::var("THECA_TEST_SERVICE_IMAGE").unwrap_or_else(|_| "debian:12".into());
        let dockerfile = format!(
            "FROM {base}\n\
            ENV DEBIAN_FRONTEND=noninteractive\n\
            RUN apt-get update && apt-get install -y --no-install-recommends \
            systemd systemd-sysv ca-certificates procps\n"
        );
        let mut build = Command::new("docker")
            .args(["build", "-q", "-t", &machine.image, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut build.stdin.take().unwrap(), dockerfile.as_bytes()).unwrap();
        assert!(build.wait().unwrap().success(), "docker build failed");
        let started = Command::new("docker")
            .args(["run", "-d", "--name", &machine.name])
            .args(["--privileged", "--cgroupns=host", "--network", "host"])
            .args(["-v", "/sys/fs/cgroup:/sys/fs/cgroup:rw"])
            .args([&machine.image, "/sbin/init"])
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(started.success(), "docker run failed");
        // Booted: `degraded` is what a container reaches (a unit that can't
        // run in one).
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let (_, state) = machine.run("root", "systemctl is-system-running");
            if state.trim() == "running" || state.trim() == "degraded" {
                break;
            }
            assert!(Instant::now() < deadline, "systemd never booted: {state}");
            std::thread::sleep(Duration::from_millis(500));
        }
        let copied = Command::new("docker")
            .arg("cp")
            .arg(deb)
            .arg(format!("{}:/tmp/theca-cli.deb", machine.name))
            .status()
            .unwrap();
        assert!(copied.success(), "docker cp failed");
        machine
    }

    /// `script` in the machine as `user`, with `env`: its success, and what
    /// it printed (both streams).
    fn run_with(&self, user: &str, env: &[(&str, &str)], script: &str) -> (bool, String) {
        let mut command = Command::new("docker");
        let home = if user == "root" {
            "/root".to_owned()
        } else {
            format!("/home/{user}")
        };
        command.args(["exec", "-u", user, "-e", &format!("HOME={home}")]);
        for (key, value) in env {
            command.args(["-e", &format!("{key}={value}")]);
        }
        let out = command
            .args([&self.name, "sh", "-c", script])
            .output()
            .unwrap();
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), said)
    }

    fn run(&self, user: &str, script: &str) -> (bool, String) {
        self.run_with(user, &[], script)
    }

    /// `script` as root, which must succeed.
    fn root(&self, script: &str) -> String {
        let (ok, said) = self.run("root", script);
        assert!(ok, "{script}: {said}");
        said
    }

    fn active(&self) -> bool {
        self.run("root", &format!("systemctl is-active --quiet {UNIT}"))
            .0
    }

    /// The unit's main process (0: none).
    fn main_pid(&self) -> u32 {
        self.root(&format!("systemctl show -p MainPID --value {UNIT}"))
            .trim()
            .parse()
            .unwrap()
    }

    /// How many times systemd restarted it by itself.
    fn restarts(&self) -> u32 {
        self.root(&format!("systemctl show -p NRestarts --value {UNIT}"))
            .trim()
            .parse()
            .unwrap()
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .stdout(Stdio::null())
            .status();
        let _ = Command::new("docker")
            .args(["rmi", &self.image])
            .stdout(Stdio::null())
            .status();
    }
}

/// One file syncs each way through the service: made in the folder, it
/// reaches the library, and made in the library, it reaches the folder.
async fn syncs(world: &World, machine: &Machine, name: &str) {
    let (made, said) = machine.run(
        USER,
        &format!("echo from the machine > {FOLDER}/{name}-here.txt"),
    );
    assert!(made, "{said}");
    eventually("a file made in the folder reaches the library", || async {
        let tree = world.remote(&world.library).await;
        tree.get(&format!("{name}-here.txt")).is_some_and(|c| {
            c.as_deref()
                .is_some_and(|c| c.starts_with(b"from the machine"))
        })
    })
    .await;
    world
        .upload(
            &world.library,
            &format!("{name}-there.txt"),
            b"from the library",
        )
        .await;
    eventually("a file made in the library reaches the folder", || async {
        let (ok, said) = machine.run(USER, &format!("cat {FOLDER}/{name}-there.txt"));
        ok && said.trim() == "from the library"
    })
    .await;
}

/// `theca-cli` from its package: the unit runs as the user and syncs; an
/// upgrade restarts it; a `theca run` beside it is refused, and one that was
/// refused starts once the other ends; removing the package stops it.
#[tokio::test(flavor = "multi_thread")]
async fn the_daemon_runs_as_a_service_through_its_package() {
    let Some(deb) = package() else { return };
    let world = world().await;
    let machine = Machine::start(&deb);

    // Installed, with its unit.
    machine.root("apt-get install -y /tmp/theca-cli.deb");
    machine.root("test -f /usr/lib/systemd/system/theca@.service");
    machine.root(&format!("useradd -m {USER}"));

    // The user logs in (a server's way: the app password in her state file)
    // and adds her library, as the unit's `theca run` will find them.
    let created = world
        .admin
        .create_app_password(&api::CreateAppPassword {
            name: USER.into(),
            organization: None,
            library: None,
            read_only: false,
        })
        .await
        .unwrap();
    let (ok, said) = machine.run_with(
        USER,
        &[("THECA_TOKEN", &created.password)],
        &format!(
            "theca login {} --no-keyring --name {USER} --folder /home/{USER}/theca",
            world.server.url
        ),
    );
    assert!(ok, "login: {said}");
    let (ok, said) = machine.run(USER, "theca add Docs");
    assert!(ok, "add: {said}");

    // Enabled: it runs as her, with her home (her state is found), and syncs.
    machine.root(&format!("systemctl enable --now {UNIT}"));
    eventually("the unit is active", || async { machine.active() }).await;
    let pid = machine.main_pid();
    assert_ne!(pid, 0);
    let owner = machine.root(&format!("ps -o user= -p {pid}"));
    assert_eq!(owner.trim(), USER, "the daemon runs as the user");
    syncs(&world, &machine, "first").await;

    // A second `theca run` by the same user is refused (the state's lock);
    // `timeout` ends one that wasn't, which would run for ever.
    let (ok, said) = machine.run(USER, "timeout 20 theca run");
    assert!(!ok, "a second theca run was not refused");
    assert!(said.contains("already running"), "{said}");
    assert!(machine.active(), "the refused run ended the unit");

    // A `theca run` that fails to start is tried again: the state locked by
    // another process (the app, say), the unit fails, and starts by itself
    // once the lock is free.
    machine.root(&format!("systemctl stop {UNIT}"));
    machine.run_with(
        USER,
        &[],
        "nohup sh -c 'echo $$ > /tmp/holder.pid; exec theca run' >/dev/null 2>&1 &",
    );
    eventually("the other process is started", || async {
        machine.run("root", "test -s /tmp/holder.pid").0
    })
    .await;
    // It takes the state's lock a moment after it starts.
    std::thread::sleep(Duration::from_secs(2));
    machine.root(&format!("systemctl start --no-block {UNIT}"));
    eventually("the unit fails to start and is tried again", || async {
        machine.restarts() >= 1
    })
    .await;
    machine.root("kill $(cat /tmp/holder.pid)");
    eventually("the unit starts once the lock is free", || async {
        machine.active() && machine.main_pid() != 0
    })
    .await;

    // Upgraded: a package of a higher version, from this one; the daemon is
    // restarted (another process), and goes on syncing.
    machine.root(
        "dpkg-deb -R /tmp/theca-cli.deb /tmp/upgraded && \
         sed -i 's/^Version: .*/Version: 99.0.0/' /tmp/upgraded/DEBIAN/control && \
         dpkg-deb -b /tmp/upgraded /tmp/theca-cli-99.deb >/dev/null",
    );
    let before = machine.main_pid();
    machine.root("apt-get install -y /tmp/theca-cli-99.deb");
    eventually("the daemon is restarted by the upgrade", || async {
        let now = machine.main_pid();
        machine.active() && now != 0 && now != before
    })
    .await;
    syncs(&world, &machine, "second").await;

    // Removed: the daemon stops, and the unit goes with the package.
    machine.root("dpkg -r theca-cli");
    assert!(!machine.active(), "the unit still runs after the removal");
    assert!(
        !machine.run("root", "pgrep -x theca").0,
        "a theca process is left after the removal"
    );
    assert!(
        !machine
            .run("root", "test -e /usr/lib/systemd/system/theca@.service")
            .0,
        "the unit file is left after the removal"
    );
}
