//! The log files (sprint 17), as a user's computer has them: `theca run`
//! and a refused command writing the day's file in the state's `logs`
//! folder, each line with its context, never the app password.

mod common;

use std::process::{Command, Stdio};

use common::*;
use theca::state::Password;

/// Every file of the folder, read (gzipped ones aren't made in a test).
fn logs(folder: &std::path::Path) -> String {
    std::fs::read_dir(folder)
        .map(|dir| {
            dir.flatten()
                .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// An upload's line carries its account, pair and round; a refused `theca
/// add` is there too, with its command; the app password is in no file.
#[tokio::test(flavor = "multi_thread")]
async fn theca_run_logs_with_its_context() {
    let w = world().await;
    let mut d = w.device_without_keychain("logs").await;
    d.stop().await;
    let Password::InState(password) = d.account.password.clone() else {
        panic!("the app password in the state");
    };
    let folder = d.state_path.parent().unwrap().join("logs");
    let theca = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_theca"));
        // The level the settings say, not the test run's.
        c.arg("--state").arg(&d.state_path).env_remove("RUST_LOG");
        c
    };
    let refused = theca().args(["add", "No such library"]).output().unwrap();
    assert!(!refused.status.success());

    // A finished day, left since yesterday: `theca run` prunes at its start.
    let yesterday = (chrono::Utc::now() - chrono::Duration::days(1)).format("%Y-%m-%d");
    let finished = folder.join(format!("theca.{yesterday}.log"));
    std::fs::write(&finished, "yesterday\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&finished)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
        .unwrap();

    d.write("a.txt", "hello");
    let mut run = theca()
        .arg("run")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    eventually("the upload in the log", || async {
        logs(&folder).contains(r#"uploaded path="a.txt""#)
    })
    .await;
    eventually("a finished day gzipped", || async {
        !finished.exists() && finished.with_extension("log.gz").exists()
    })
    .await;
    let _ = run.kill();
    let _ = run.wait();

    let all = logs(&folder);
    let upload = all
        .lines()
        .find(|l| l.contains(r#"uploaded path="a.txt""#))
        .unwrap();
    for context in [
        r#"command{name="run"}"#,
        &format!("account{{id={}", d.account.id),
        "pair{id=",
        ":round:",
    ] {
        assert!(upload.contains(context), "{context} in {upload}");
    }
    assert!(
        all.lines()
            .any(|l| l.contains(r#"command{name="add"}"#) && l.contains("No such library")),
        "{all}"
    );
    assert!(!all.contains(&password), "the app password in the log");
}

/// A token given to `login` (an argument, the environment) is in no log,
/// the login refused or not.
#[tokio::test(flavor = "multi_thread")]
async fn a_token_is_never_logged() {
    let w = world().await;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.sqlite");
    let token = "not-a-real-app-password-4f2c";
    for env in [false, true] {
        let mut c = Command::new(env!("CARGO_BIN_EXE_theca"));
        c.arg("--state")
            .arg(&state)
            .env_remove("RUST_LOG")
            .args(["login", &w.server.url]);
        if env {
            c.env("THECA_TOKEN", token);
        } else {
            c.args(["--token", token]);
        }
        let out = c.output().unwrap();
        assert!(!out.status.success(), "a made-up token refused");
    }
    let all = logs(&dir.path().join("logs"));
    assert!(all.contains(r#"command{name="login"}"#), "{all}");
    assert!(!all.contains(token), "{all}");
}

/// A state migrated by a command (1.1.0's): the migration's line is in the
/// file, logging having started before the state opened.
#[test]
fn a_migration_is_logged() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.sqlite");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/upgrade/1.1.0/state.sqlite"
        ),
        &state,
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_theca"))
        .arg("--state")
        .arg(&state)
        .env_remove("RUST_LOG")
        .arg("accounts")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let all = logs(&dir.path().join("logs"));
    assert!(all.contains("the state migrated from version"), "{all}");
}
