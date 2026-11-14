//! The harness's own promises: what the tests start ends with them, however
//! they end (`tests/common/child.sh`, `common::Tended`). No drive here: plain
//! commands, on every Unix.
#![cfg(unix)]

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use common::*;

/// `child.sh <silence> <grace> - <command>`, its input a pipe held here.
fn script(silence: u32, grace: u32, command: &[&str]) -> (Child, ChildStdin) {
    let mut child = Command::new("bash")
        .arg(child_script())
        .args([&silence.to_string(), &grace.to_string(), "-"])
        .args(command)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let pipe = child.stdin.take().unwrap();
    (child, pipe)
}

/// The script's status once it ends, `limit` at most (killed past it).
fn ended(child: &mut Child, limit: Duration) -> ExitStatus {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            // Its command first: the script killed alone would leave it
            // running, holding this run's output open.
            let _ = Command::new("pkill")
                .args(["-KILL", "-P", &child.id().to_string()])
                .status();
            let _ = child.kill();
            panic!("still running after {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The script's command (not its watcher): its one child that isn't a shell.
fn command_of(script: u32, name: &str) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let found = Command::new("pgrep")
            .args(["-P", &script.to_string(), "-x", name])
            .output()
            .unwrap();
        if let Some(pid) = String::from_utf8_lossy(&found.stdout)
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "the script never started {name}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The pipe closed (the test process ended, or stopped its child): the
/// command gets SIGTERM, and the script exits as it did.
#[test]
fn a_closed_pipe_stops_the_command() {
    let (mut child, pipe) = script(10, 5, &["sleep", "600"]);
    let sleep = command_of(child.id(), "sleep");
    assert!(running(sleep));
    drop(pipe);
    let status = ended(&mut child, Duration::from_secs(3));
    assert_eq!(status.code(), Some(143), "SIGTERM's default");
    assert!(!running(sleep));
}

/// Silence with the pipe still open (a killed test process whose stuck thread
/// keeps every file open): the command is stopped once the limit passed.
#[test]
fn silence_stops_the_command() {
    let (mut child, pipe) = script(2, 5, &["sleep", "600"]);
    let sleep = command_of(child.id(), "sleep");
    let started = Instant::now();
    let status = ended(&mut child, Duration::from_secs(6));
    assert_eq!(status.code(), Some(143));
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "before its limit"
    );
    assert!(!running(sleep));
    drop(pipe);
}

/// A line now and then keeps the command alive past the silence limit.
#[test]
fn beats_keep_the_command_alive() {
    let (mut child, mut pipe) = script(2, 5, &["sleep", "600"]);
    let sleep = command_of(child.id(), "sleep");
    for _ in 0..10 {
        pipe.write_all(b"\n").unwrap();
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(child.try_wait().unwrap().is_none(), "stopped while beating");
    assert!(running(sleep));
    drop(pipe);
    assert_eq!(ended(&mut child, Duration::from_secs(3)).code(), Some(143));
}

/// A command ignoring SIGTERM is killed once the grace passed; with no grace,
/// at once.
#[test]
fn a_command_ignoring_sigterm_is_killed_after_the_grace() {
    let stubborn = r#"trap "" TERM; echo ready; while :; do sleep 1; done"#;
    let mut command = Command::new("bash");
    command
        .arg(child_script())
        .args(["10", "2", "-", "bash", "-c", stubborn])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = command.spawn().unwrap();
    // Its trap is set once it says so.
    let mut ready = [0u8; 6];
    std::io::Read::read_exact(child.stdout.as_mut().unwrap(), &mut ready).unwrap();
    let started = Instant::now();
    drop(child.stdin.take());
    let status = ended(&mut child, Duration::from_secs(6));
    assert_eq!(status.code(), Some(137), "SIGKILL");
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "before the grace"
    );

    let (mut child, pipe) = script(10, 0, &["sleep", "600"]);
    command_of(child.id(), "sleep");
    drop(pipe);
    assert_eq!(ended(&mut child, Duration::from_secs(2)).code(), Some(137));
}

/// A command ending by itself: the script exits at once, with its status.
#[test]
fn a_command_ending_by_itself_ends_the_script() {
    let (mut child, pipe) = script(10, 5, &["sh", "-c", "exit 7"]);
    assert_eq!(ended(&mut child, Duration::from_secs(2)).code(), Some(7));
    drop(pipe);
}

/// Polls `check` for `limit` at most.
fn within(limit: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while !check() {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// This process's heartbeat keeps a tended command alive past the silence
/// limit (10 s); stopped, it says how it ended.
#[test]
fn a_tended_command_lives_until_it_is_stopped() {
    let mut tended = Tended::start(Path::new("sleep"), 5, None, |sleep| {
        sleep.arg("600");
    });
    let sleep = command_of(tended.id(), "sleep");
    std::thread::sleep(Duration::from_secs(13));
    assert_eq!(tended.ended(), None, "stopped while this process lives");
    assert!(running(sleep));
    assert_eq!(tended.stop(), Ended::Exited(143));
    assert!(!running(sleep));
}

/// A tended command that ignores SIGTERM: `stop` says it was killed. One
/// ending by itself: its code, without being stopped.
#[test]
fn stopping_says_how_a_command_ended() {
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("ready");
    let stubborn = r#"trap "" TERM; touch "$1"; while :; do sleep 1; done"#;
    let mut tended = Tended::start(Path::new("bash"), 1, None, |bash| {
        bash.args(["-c", stubborn, "bash"]).arg(&ready);
    });
    assert!(
        within(Duration::from_secs(5), || ready.exists()),
        "its trap"
    );
    assert_eq!(tended.stop(), Ended::Killed);

    let mut tended = Tended::start(Path::new("sh"), 5, None, |sh| {
        sh.args(["-c", "exit 7"]);
    });
    assert!(within(Duration::from_secs(2), || tended.ended().is_some()));
    assert_eq!(tended.ended(), Some(Ended::Exited(7)));
}

/// What a test meant to leave nothing of, cleared whatever it found:
/// processes killed, a server's database dropped.
struct Leftovers {
    processes: Vec<u32>,
    database: Option<String>,
}

impl Drop for Leftovers {
    fn drop(&mut self) {
        for pid in &self.processes {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .stderr(Stdio::null())
                .status();
        }
        if let Some(database) = &self.database {
            drop_database(database);
        }
    }
}

/// Where a victim says what it started.
const VICTIM: &str = "THECA_TEST_VICTIM";

/// This test binary run on one ignored test, a victim: what it wrote to the
/// file it was given, once it has.
fn victim(test: &str, said: &Path) -> (Child, String) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", test, "--nocapture"])
        .env(VICTIM, said)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    // Ready, or ended before it was (its own panic says why, above).
    within(Duration::from_secs(60), || {
        said.exists() || child.try_wait().is_ok_and(|ended| ended.is_some())
    });
    if !said.exists() {
        let _ = child.kill();
        panic!("the victim never got ready");
    }
    (child, std::fs::read_to_string(said).unwrap())
}

/// A victim's word, written whole or not at all.
fn say(said: &std::ffi::OsStr, what: &str) {
    let said = Path::new(said);
    let part = said.with_extension("part");
    std::fs::write(&part, what).unwrap();
    std::fs::rename(part, said).unwrap();
}

/// Run by `a_killed_test_leaves_no_server` only: a test process holding a
/// server, which says where it is and waits to be killed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "the victim of a_killed_test_leaves_no_server"]
async fn victim_with_a_server() {
    let Some(said) = std::env::var_os(VICTIM) else {
        return;
    };
    let server = server().await;
    say(
        &said,
        &format!("{}\n{}", server.script_pid(), server.database),
    );
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// A test process killed with SIGKILL (no destructor runs) leaves no server:
/// its script sees the pipe close, and kills it.
#[test]
fn a_killed_test_leaves_no_server() {
    let dir = tempfile::tempdir().unwrap();
    let (mut victim, said) = victim("victim_with_a_server", &dir.path().join("said"));
    let (script, database) = said.split_once('\n').unwrap();
    let script: u32 = script.parse().unwrap();
    let server = command_of(script, "theca-server");
    let _cleared = Leftovers {
        processes: vec![server],
        database: Some(database.to_owned()),
    };
    assert!(
        running(server) && running(script),
        "nothing to leave behind"
    );
    victim.kill().unwrap();
    victim.wait().unwrap();
    assert!(
        within(Duration::from_secs(15), || !running(server)
            && !running(script)),
        "left behind: the server {}, its script {}",
        running(server),
        running(script)
    );
}

/// A child engine that didn't stop by itself (frozen, it can't take SIGTERM,
/// and its script kills it after the grace) fails its test at `stop`.
#[tokio::test(flavor = "multi_thread")]
#[should_panic(expected = "the engine stops")]
async fn a_daemon_that_had_to_be_killed_fails_its_stop() {
    let w = world().await;
    let mut a = w.device_in_a_child("a", true).await;
    let theca = command_of(a.script_pid().unwrap(), "theca");
    let frozen = Command::new("kill")
        .args(["-STOP", &theca.to_string()])
        .status()
        .unwrap();
    assert!(frozen.success());
    a.stop().await;
}

/// A drive beside a sync device, on a world whose wire then carries nothing:
/// the drive lists `a.txt`, not downloaded, and a read of it waits on the
/// daemon's fetch for as long as the wire stalls.
#[cfg(target_os = "linux")]
async fn a_drive_with_a_stalled_file() -> (World, std::sync::Arc<Wire>, Device, Device) {
    let (w, wire) = world_cut().await;
    let s = w.device("sync").await;
    s.write("a.txt", "hello");
    w.converge(&[&s]).await;
    let d = w.drive("drive").await;
    let placeholder = d.backing().join("a.txt");
    eventually("a placeholder", || async {
        theca::drive::is_placeholder(&placeholder)
    })
    .await;
    wire.budget.store(0, std::sync::atomic::Ordering::SeqCst);
    (w, wire, s, d)
}

/// Run by `a_killed_drive_test_leaves_nothing` only: a test process with a
/// drive, one of its threads waiting on a read of it, which says where
/// things are and waits to be killed.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "the victim of a_killed_drive_test_leaves_nothing"]
async fn victim_with_a_drive() {
    let Some(said) = std::env::var_os(VICTIM) else {
        return;
    };
    let (w, _wire, _s, d) = a_drive_with_a_stalled_file().await;
    let file = d.folder.join("a.txt");
    let (done, read) = std::sync::mpsc::channel();
    std::thread::spawn(move || done.send(std::fs::read(file)));
    // In the daemon's hands by now, its fetch stalled: else the test's kill
    // would close every pipe, and prove nothing of the silence limit.
    assert!(
        read.recv_timeout(Duration::from_secs(2)).is_err(),
        "the read wasn't stalled"
    );
    let said_now = format!(
        "{}\n{}\n{}\n{}",
        d.script_pid().unwrap(),
        w.server.script_pid(),
        d.folder.display(),
        w.server.database
    );
    say(&said, &said_now);
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// A test process killed with SIGKILL while one of its threads waits on its
/// drive: that thread keeps the process, and every file it had open, until
/// the drive answers or its daemon ends. The daemon's script hears the
/// heartbeat stop, and nothing is left: no daemon, no server, no mount, and
/// the killed process itself gone.
#[cfg(target_os = "linux")]
#[test]
fn a_killed_drive_test_leaves_nothing() {
    if !fuse() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (mut victim, said) = victim("victim_with_a_drive", &dir.path().join("said"));
    let said: Vec<&str> = said.lines().collect();
    let [daemon_script, server_script] = [said[0], said[1]].map(|p| p.parse::<u32>().unwrap());
    let folder = Path::new(said[2]);
    let daemon = command_of(daemon_script, "theca");
    let server = command_of(server_script, "theca-server");
    // Killing the daemon is also what frees the victim, should the script
    // not have.
    let _cleared = Leftovers {
        processes: vec![daemon, server],
        database: Some(said[3].to_owned()),
    };
    let mounted = || theca::drive::fuse::mounted(folder);
    assert!(
        running(daemon) && running(server) && mounted(),
        "nothing to leave behind"
    );
    victim.kill().unwrap();
    let all = [daemon, server, daemon_script, server_script];
    let left = || all.iter().filter(|p| running(**p)).count();
    assert!(
        within(Duration::from_secs(45), || left() == 0 && !mounted()),
        "left behind: {} of the daemon, the server and their scripts; mounted: {}",
        left(),
        mounted()
    );
    assert!(
        within(Duration::from_secs(5), || victim
            .try_wait()
            .is_ok_and(|ended| ended.is_some())),
        "the killed test still waits on its drive"
    );
}

/// A drive's daemon killed while a read waits on it: the read fails, at once
/// (the kernel aborts the connection of a server gone), instead of hanging.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_killed_mid_read_fails_the_read() {
    if !fuse() {
        return;
    }
    let (_w, wire, _s, d) = a_drive_with_a_stalled_file().await;
    let daemon = command_of(d.script_pid().unwrap(), "theca");
    let file = d.folder.join("a.txt");
    let (done, read) = std::sync::mpsc::channel();
    std::thread::spawn(move || done.send(std::fs::read(file)));
    assert!(
        read.recv_timeout(Duration::from_secs(2)).is_err(),
        "the read wasn't stalled"
    );
    let mounted = || theca::drive::fuse::mounted(&d.folder);
    assert!(running(daemon) && mounted());
    let killed = Command::new("kill")
        .args(["-KILL", &daemon.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let ended = read
        .recv_timeout(Duration::from_secs(5))
        .expect("the read still waits on a dead daemon");
    assert!(ended.is_err(), "read {ended:?} from a dead daemon");
    assert!(within(Duration::from_secs(2), || !running(daemon)));
    // Killed, it couldn't unmount: its script detached the dead mount.
    assert!(
        within(Duration::from_secs(5), || !mounted()),
        "the dead daemon's mount left"
    );
    wire.budget
        .store(usize::MAX, std::sync::atomic::Ordering::SeqCst);
}

/// A drive that answers nothing fails the test that reads it once the limit
/// passed, inside `eventually` too; the call left waiting ends with the
/// drive's engine, at the test's cleanup.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn a_drive_that_never_answers_fails_at_its_deadline() {
    use std::sync::atomic::{AtomicBool, Ordering};
    if !fuse() {
        return;
    }
    let (_w, _wire, _s, d) = a_drive_with_a_stalled_file().await;
    let daemon = command_of(d.script_pid().unwrap(), "theca");
    let folder = d.folder.clone();
    let limit = Duration::from_secs(3);
    let returned = std::sync::Arc::new(AtomicBool::new(false));
    let read = {
        let (file, returned) = (folder.join("a.txt"), returned.clone());
        move || {
            let read = std::fs::read(file);
            returned.store(true, Ordering::SeqCst);
            read.is_ok()
        }
    };
    let started = Instant::now();
    let failed = tokio::spawn(in_drive_within(limit, "a.txt read", read))
        .await
        .expect_err("a stalled read returned");
    let (why, took) = (panic_message(failed), started.elapsed());
    assert!(why.contains("the drive didn't answer"), "{why}");
    assert!(took >= limit && took < limit * 2, "{took:?}");

    // Inside `eventually`, whose own 30 s can't pass while its check waits.
    let file = folder.join("a.txt");
    let started = Instant::now();
    let failed = tokio::spawn(async move {
        eventually("a.txt read", || {
            let file = file.clone();
            in_drive_within(limit, "a.txt read", move || std::fs::read(file).is_ok())
        })
        .await;
    })
    .await
    .expect_err("a stalled read returned");
    let (why, took) = (panic_message(failed), started.elapsed());
    assert!(why.contains("the drive didn't answer"), "{why}");
    assert!(took < limit * 2, "{took:?}");

    // The read was stalled for real, and ends once its drive's engine has.
    assert!(!returned.load(Ordering::SeqCst), "the read wasn't stalled");
    assert!(running(daemon) && theca::drive::fuse::mounted(&folder));
    drop(d);
    assert!(
        within(Duration::from_secs(5), || returned.load(Ordering::SeqCst)),
        "the read still waits, its drive's engine stopped"
    );
    assert!(!running(daemon) && !theca::drive::fuse::mounted(&folder));
}
