//! What the CLI promises (docs/decisions.md, Sync): its commands and flags,
//! its exit codes (only 0 or not is promised, `USING.md` lists them), and
//! `USING.md` in step with both.

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Output};

use common::*;
use theca::exit;
use theca::state::Stop;

fn theca(state: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_theca"))
        .arg("--state")
        .arg(state)
        .args(args)
        .output()
        .unwrap()
}

fn code(output: &Output) -> u8 {
    output.status.code().expect("exited") as u8
}

/// Invalid arguments (`add` lost `--auto-sync-mb` to `account`), and an
/// error.
#[test]
fn usage_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.sqlite");
    let out = theca(&state, &["add", "Docs", "--auto-sync-mb", "5"]);
    assert_eq!(code(&out), exit::USAGE);
    let out = theca(&state, &["remove-account", "nobody@example.com"]);
    assert_eq!(
        code(&out),
        exit::ERROR,
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `drive-limit`: the default until one is set, then that one; a size it
/// can't read is a usage error, and changes nothing.
#[test]
fn the_drive_limit_is_shown_and_set() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.sqlite");
    let said = |args: &[&str]| {
        let out = theca(&state, args);
        (
            code(&out),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    let (code0, out) = said(&["drive-limit"]);
    assert_eq!(code0, 0);
    assert!(out.contains("(the default"), "{out}");
    assert_eq!(said(&["drive-limit", "1.5GiB"]).0, 0);
    assert_eq!(said(&["drive-limit", "lots"]).0, exit::USAGE);
    let (_, out) = said(&["drive-limit"]);
    assert_eq!(out, "Drives keep up to 1.5 GiB downloaded.\n");
    // As it was given: 20GB reads "20 GB".
    let (_, out) = said(&["drive-limit", "20GB"]);
    assert_eq!(out, "Drives keep up to 20 GB downloaded.\n");
    // Back to the default.
    let (_, out) = said(&["drive-limit", "default"]);
    assert!(out.contains("(the default"), "{out}");
}

/// `logs`: where they are and the defaults; each set; a level or a size it
/// can't read is a usage error; `default` resets; the folder set is where
/// the next command writes.
#[test]
fn the_logs_are_shown_and_set() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.sqlite");
    let said = |args: &[&str]| {
        let out = theca(&state, args);
        (
            code(&out),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    let defaults = format!(
        "Logs: {} (the default)\n  level info, 14 days kept, up to 100 MB in all\n",
        dir.path().join("logs").display()
    );
    let (code0, out) = said(&["logs"]);
    assert_eq!(code0, 0);
    assert!(out.starts_with(&defaults), "{out}");
    let elsewhere = dir.path().join("elsewhere");
    let (_, out) = said(&[
        "logs",
        "--level",
        "debug",
        "--days",
        "30",
        "--max-size",
        "1GB",
        "--path",
        &elsewhere.to_string_lossy(),
    ]);
    assert!(
        out.starts_with(&format!(
            "Logs: {}\n  level debug, 30 days kept, up to 1 GB in all\n",
            elsewhere.display()
        )),
        "{out}"
    );
    assert!(out.contains("next start"), "{out}");
    assert_eq!(said(&["logs", "--level", "loud"]).0, exit::USAGE);
    assert_eq!(said(&["logs", "--max-size", "lots"]).0, exit::USAGE);
    said(&["logs"]);
    assert!(
        std::fs::read_dir(&elsewhere).is_ok_and(|mut d| d.next().is_some()),
        "the next command writes there"
    );
    let (_, out) = said(&[
        "logs",
        "--level",
        "default",
        "--days",
        "default",
        "--max-size",
        "default",
        "--path",
        "default",
    ]);
    assert!(out.starts_with(&defaults), "{out}");
}

/// `status` fails while a pair isn't syncing, and tells a signed-out account
/// apart.
#[tokio::test(flavor = "multi_thread")]
async fn status_says_what_syncs() {
    let w = world().await;
    let mut a = w.device("a").await;
    w.converge(&[&a]).await;
    a.stop().await;
    assert_eq!(code(&theca(&a.state_path, &["status"])), 0);
    let state = a.state();
    state
        .set_stopped(a.pair().id, Some(&Stop::LibraryDeleted))
        .unwrap();
    let out = theca(&a.state_path, &["status"]);
    assert_eq!(code(&out), exit::NOT_SYNCING);
    assert!(String::from_utf8_lossy(&out.stdout).contains("stopped: library deleted"));
    state.set_signed_out(a.account.id).unwrap();
    let out = theca(&a.state_path, &["status"]);
    assert_eq!(code(&out), exit::SIGNED_OUT);
    assert!(String::from_utf8_lossy(&out.stderr).contains("theca login"));
}

/// `THECA_NO_KEYRING` set as scripts set a flag (`1`) keeps a script's login
/// out of the keychain, in a child process as a service starts it.
#[tokio::test(flavor = "multi_thread")]
async fn a_script_logs_in_without_the_keychain() {
    let w = world().await;
    let created = w
        .admin
        .create_app_password(&theca_common::api::CreateAppPassword {
            name: "script".into(),
            organization: None,
            library: None,
            read_only: false,
        })
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.sqlite");
    let out = Command::new(env!("CARGO_BIN_EXE_theca"))
        .arg("--state")
        .arg(&state)
        .args(["login", &w.server.url])
        .env("THECA_TOKEN", &created.password)
        .env("THECA_NO_KEYRING", "1")
        .output()
        .unwrap();
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let accounts = theca::state::State::open(&state)
        .unwrap()
        .accounts()
        .unwrap();
    assert_eq!(
        accounts[0].password,
        theca::state::Password::InState(created.password)
    );
}

/// `theca account` shows an account's settings, and changes them.
#[tokio::test(flavor = "multi_thread")]
async fn account_settings() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.stop().await;
    let email = a.account.email.clone();
    let folder = a.folder.parent().unwrap().join("elsewhere");
    let out = theca(
        &a.state_path,
        &[
            "account",
            &email,
            "--folder",
            &folder.to_string_lossy(),
            "--auto-sync-mb",
            "10",
        ],
    );
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let account = a.state().account(a.account.id).unwrap().unwrap();
    assert_eq!(account.default_folder, folder);
    assert_eq!(account.auto_sync_limit, Some(10 * 1024 * 1024));
    assert!(String::from_utf8_lossy(&out.stdout).contains("10 MiB"));
    let out = theca(&a.state_path, &["account", &email, "--auto-sync-mb", "0"]);
    assert_eq!(code(&out), 0);
    let account = a.state().account(a.account.id).unwrap().unwrap();
    assert_eq!(account.auto_sync_limit, None);
    assert_eq!(account.default_folder, folder);
    assert!(String::from_utf8_lossy(&out.stdout).contains("never synced by themselves"));
}

/// A section of `USING.md`: the first cell of each row of its table.
fn using(section: &str) -> BTreeSet<String> {
    let using =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("USING.md")).unwrap();
    let heading = |l: &str| l.starts_with('#');
    let body: Vec<&str> = using
        .lines()
        .skip_while(|l| !(heading(l) && l.trim_start_matches('#').trim() == section))
        .skip(1)
        .take_while(|l| !heading(l))
        .collect();
    assert!(!body.is_empty(), "USING.md has no {section}");
    body.iter()
        .filter_map(|l| l.strip_prefix("| "))
        .filter_map(|l| l.split(" |").next())
        .map(|cell| cell.trim_matches('`').to_owned())
        .filter(|cell| !cell.is_empty() && cell != "Code" && cell != "Variable")
        .collect()
}

/// `USING.md` lists every exit code there is, and only those.
#[test]
fn using_lists_the_exit_codes() {
    let mut codes: BTreeSet<String> = exit::ALL.iter().map(u8::to_string).collect();
    codes.insert("0".into());
    assert_eq!(using("Exit codes"), codes);
}

/// `USING.md` lists every environment variable the CLI reads, and only those.
#[test]
fn using_lists_the_environment_variables() {
    let help = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_theca"))
            .args(args)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    let main = help(&["--help"]);
    let commands = main
        .split("Commands:\n")
        .nth(1)
        .unwrap()
        .lines()
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_whitespace().next())
        .filter(|c| *c != "help");
    let mut texts = vec![main.clone()];
    texts.extend(commands.map(|c| help(&[c, "--help"])));
    let mut variables: BTreeSet<String> = texts
        .iter()
        .flat_map(|t| t.split("[env: ").skip(1))
        .map(|v| v.split(['=', ']']).next().unwrap().to_owned())
        .collect();
    // The log's filter: tracing's, not clap's.
    variables.insert("RUST_LOG".into());
    assert_eq!(using("Environment variables"), variables);
}
