//! Headless updates for real: a manifest and an archive served over HTTP,
//! signed with a key of the test's own, installed over a copy of this build's
//! `theca`, which must still run.

mod common;

use std::collections::HashMap;
#[cfg(windows)]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::serve;

use theca::state::State;
use theca::update::{self, Checked, Installed, Mode, UpdateError};

const MARKER: &[u8] = b"\0the new release";

struct Key(minisign::KeyPair);

impl Key {
    fn new() -> Key {
        Key(minisign::KeyPair::generate_unencrypted_keypair().unwrap())
    }
    fn public(&self) -> String {
        b64(&self.0.pk.to_box().unwrap().into_string())
    }
    /// As `tauri signer sign` signs a package of the release announced
    /// (9.0.0): its trusted comment names the file.
    fn sign(&self, bytes: &[u8]) -> String {
        self.sign_as(bytes, "theca-headless-9.0.0-linux-x86_64.tar.gz")
    }
    /// Signed as the package `file`.
    fn sign_as(&self, bytes: &[u8], file: &str) -> String {
        let trusted = format!("timestamp:1794500000\tfile:{file}");
        let sig = minisign::sign(Some(&self.0.pk), &self.0.sk, bytes, Some(&trusted), None);
        b64(&sig.unwrap().into_string())
    }
}

fn b64(text: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(text)
}

/// The headless archive `release.yml` publishes for `target`, holding
/// `binary`.
fn archive(target: &str, binary: &[u8]) -> Vec<u8> {
    let folder = format!(
        "theca-headless-9.0.0-{}",
        target.trim_end_matches("-headless")
    );
    #[cfg(not(windows))]
    {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (name, content) in [("README.md", &b"read me"[..]), ("theca", binary)] {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o755);
            tar.append_data(&mut header, format!("{folder}/{name}"), content)
                .unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }
    #[cfg(windows)]
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, content) in [("README.md", &b"read me"[..]), ("theca.exe", binary)] {
            zip.start_file(
                format!("{folder}/{name}"),
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zip.write_all(content).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }
}

/// A copy of this build's `theca` installed in a folder of its own, and a
/// release of it (the same binary, `MARKER` appended) served with its
/// manifest, signed as `sign` says.
struct Release {
    _dir: tempfile::TempDir,
    exe: PathBuf,
    installed: Installed,
    original: Vec<u8>,
    manifest: String,
    requests: Arc<AtomicUsize>,
}

impl Release {
    /// Headless: the binary, released in its archive.
    fn new(sign: impl FnOnce(&[u8]) -> String) -> Release {
        let binary = std::fs::read(env!("CARGO_BIN_EXE_theca")).unwrap();
        Release::of(Installed::Headless, &binary, sign)
    }

    /// Headless, a small file standing in for the binary (the debug build
    /// is hundreds of MB): for the tests that don't run it.
    fn small(sign: impl FnOnce(&[u8]) -> String) -> Release {
        Release::of(Installed::Headless, b"a binary", sign)
    }

    /// `small`, needing a server on `min_server` or later.
    fn needing(min_server: &str, sign: impl FnOnce(&[u8]) -> String) -> Release {
        Release::with(Installed::Headless, b"a binary", min_server, sign)
    }

    /// `kind` of install of `binary` (the `.AppImage`'s package is the file
    /// itself).
    fn of(
        kind: fn(PathBuf) -> Installed,
        binary: &[u8],
        sign: impl FnOnce(&[u8]) -> String,
    ) -> Release {
        Release::with(kind, binary, "1.6.0", sign)
    }

    fn with(
        kind: fn(PathBuf) -> Installed,
        binary: &[u8],
        min_server: &str,
        sign: impl FnOnce(&[u8]) -> String,
    ) -> Release {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir
            .path()
            .join(format!("theca{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&exe, binary).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let installed = kind(exe.clone());
        let target = installed.target();
        let original = std::fs::read(&exe).unwrap();
        let new = [&original[..], MARKER].concat();
        let archive = match installed {
            Installed::Headless(_) => archive(&target, &new),
            Installed::AppImage(_) => new,
        };
        let signature = sign(&archive);
        let (base, requests) = serve(|base| {
            let manifest = serde_json::json!({
                "version": "9.0.0",
                "notes": "The new release",
                "pub_date": "2026-11-20T10:00:00Z",
                "min_server": min_server,
                "platforms": {
                    target: {
                        "url": format!("{base}/theca.archive"),
                        "signature": signature,
                    }
                }
            });
            HashMap::from([
                ("/latest.json", manifest.to_string().into_bytes()),
                ("/theca.archive", archive),
            ])
        });
        Release {
            _dir: dir,
            exe,
            installed,
            original,
            manifest: format!("{base}/latest.json"),
            requests,
        }
    }

    /// Nothing changed: the binary as it was, alone in its folder.
    fn unchanged(&self) {
        assert!(
            std::fs::read(&self.exe).unwrap() == self.original,
            "the binary changed"
        );
        assert_eq!(
            files(self.exe.parent().unwrap()),
            std::slice::from_ref(&self.exe)
        );
    }
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    files
}

/// One account's server, on `version`, as `update::servers` gives it.
fn server(version: &str) -> Vec<(String, Option<String>)> {
    vec![("https://cloud.example".into(), Some(version.into()))]
}

/// The release found, its archive verified, the binary replaced, and the new
/// one runs.
#[tokio::test]
async fn headless_updates_itself() {
    let key = Key::new();
    let release = Release::new(|a| key.sign(a));
    let manifest = update::check(&release.manifest).await.unwrap().unwrap();
    assert_eq!(manifest.version, "9.0.0");
    update::install(
        &manifest,
        &key.public(),
        &server("1.6.0"),
        &release.installed,
    )
    .await
    .unwrap();
    let installed = std::fs::read(&release.exe).unwrap();
    assert!(installed.ends_with(MARKER), "the binary wasn't replaced");
    // On Windows the old one is renamed aside (it may be running), removed
    // at the next update.
    let mut expected = vec![release.exe.clone()];
    if cfg!(windows) {
        expected.push(release.exe.with_extension("exe.old"));
    }
    assert_eq!(files(release.exe.parent().unwrap()), expected);
    let out = Command::new(&release.exe)
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success(), "the new binary doesn't run");
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("theca "));
}

async fn refused(
    release: &Release,
    key: &Key,
    servers: &[(String, Option<String>)],
) -> UpdateError {
    let manifest = update::check(&release.manifest).await.unwrap().unwrap();
    let error = update::install(&manifest, &key.public(), servers, &release.installed)
        .await
        .unwrap_err();
    release.unchanged();
    error
}

#[tokio::test]
async fn a_bad_signature_changes_nothing() {
    let key = Key::new();
    let release = Release::small(|_| key.sign(b"another archive"));
    assert_eq!(
        refused(&release, &key, &server("1.6.0")).await,
        UpdateError::BadSignature
    );
}

#[tokio::test]
async fn another_keys_signature_changes_nothing() {
    let key = Key::new();
    let other = Key::new();
    let release = Release::small(|a| other.sign(a));
    assert_eq!(
        refused(&release, &key, &server("1.6.0")).await,
        UpdateError::OtherKey
    );
}

#[tokio::test]
async fn a_min_server_too_new_changes_nothing() {
    let key = Key::new();
    let release = Release::small(|a| key.sign(a));
    let error = refused(&release, &key, &server("1.5.0")).await;
    assert!(matches!(error, UpdateError::ServerTooOld { .. }), "{error}");
}

/// A folder its user can't write (an admin's install): said, nothing changed.
#[cfg(unix)]
#[tokio::test]
async fn a_binary_it_cant_write_changes_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let key = Key::new();
    let release = Release::small(|a| key.sign(a));
    let dir = release.exe.parent().unwrap().to_owned();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Root writes anyway (Woodpecker's containers): GitHub's runners aren't root.
    if std::fs::write(dir.join("probe"), "").is_ok() {
        eprintln!("skipped: this user writes a read-only folder (root)");
        std::fs::remove_file(dir.join("probe")).unwrap();
        return;
    }
    let error = refused(&release, &key, &server("1.6.0")).await;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The manifest only: the package isn't downloaded to be refused.
    assert_eq!(
        release.requests.load(Ordering::SeqCst),
        1,
        "the package downloaded"
    );
    let said = error.to_string();
    assert!(matches!(error, UpdateError::CantWrite { .. }), "{said}");
    assert!(said.contains(&release.exe.display().to_string()), "{said}");
    assert!(said.contains("nothing changed"), "{said}");
}

/// A link to the binary (`/usr/local/bin/theca`): the binary replaced, the
/// link kept.
#[cfg(unix)]
#[tokio::test]
async fn a_link_to_the_binary_stays_a_link() {
    let key = Key::new();
    let release = Release::small(|a| key.sign(a));
    let links = tempfile::tempdir().unwrap();
    let link = links.path().join("theca");
    std::os::unix::fs::symlink(&release.exe, &link).unwrap();
    let manifest = update::check(&release.manifest).await.unwrap().unwrap();
    update::install(
        &manifest,
        &key.public(),
        &server("1.6.0"),
        &Installed::Headless(link.clone()),
    )
    .await
    .unwrap();
    assert!(
        link.symlink_metadata().unwrap().is_symlink(),
        "the link was replaced"
    );
    assert!(std::fs::read(&release.exe).unwrap().ends_with(MARKER));
}

/// The `.AppImage`: the file `linux-<arch>` names, verified, renamed over
/// `$APPIMAGE` (no unpacking, no Tauri, no display: the new one runs with
/// none), and started again from there, with the arguments given.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_appimage_updates_itself_without_a_display() {
    let key = Key::new();
    let binary = std::fs::read(env!("CARGO_BIN_EXE_theca")).unwrap();
    let release = Release::of(Installed::AppImage, &binary, |a| key.sign(a));
    assert_eq!(
        release.installed.target(),
        format!("linux-{}", std::env::consts::ARCH)
    );
    let manifest = update::check(&release.manifest).await.unwrap().unwrap();
    update::install(
        &manifest,
        &key.public(),
        &server("1.6.0"),
        &release.installed,
    )
    .await
    .unwrap();
    assert!(std::fs::read(&release.exe).unwrap().ends_with(MARKER));
    let out = Command::new(&release.exe)
        .arg("--version")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap();
    assert!(out.status.success(), "the new .AppImage doesn't run");
    let restart = release
        .installed
        .restart_command(["run".into(), "--state".into()]);
    assert_eq!(restart.get_program(), release.exe.as_os_str());
    assert!(
        restart.get_args().eq(["run", "--state"]),
        "not its arguments"
    );
}

/// The state of a computer with no account, in `mode`.
fn state_in(mode: Mode) -> (tempfile::TempDir, State) {
    let dir = tempfile::tempdir().unwrap();
    let state = State::open(&dir.path().join("state.sqlite")).unwrap();
    update::set_mode(&state, mode).unwrap();
    (dir, state)
}

/// One scheduled check in `mode`, installing headless as `theca run` does.
async fn tick(release: &Release, key: &Key, mode: Mode) -> Checked {
    let (_dir, state) = state_in(mode);
    tick_on(&state, release, key).await
}

/// One scheduled check on `state`, installing headless as `theca run` does.
async fn tick_on(state: &State, release: &Release, key: &Key) -> Checked {
    let public = key.public();
    update::tick(state, &release.manifest, |manifest, servers| async move {
        update::install(&manifest, &public, &servers, &release.installed).await
    })
    .await
}

/// `auto`: a newer release found and installed.
#[tokio::test]
async fn auto_installs() {
    let key = Key::new();
    let release = Release::small(|a| key.sign(a));
    let checked = tick(&release, &key, Mode::Auto).await;
    assert!(
        matches!(&checked, Checked::Installed(m) if m.version == "9.0.0"),
        "{checked:?}"
    );
    assert!(std::fs::read(&release.exe).unwrap().ends_with(MARKER));
}

/// `ask`: offered, nothing installed.
#[tokio::test]
async fn ask_offers_and_doesnt_install() {
    let key = Key::new();
    let release = Release::small(|a| key.sign(a));
    let checked = tick(&release, &key, Mode::Ask).await;
    assert!(
        matches!(&checked, Checked::Offered { manifest, why: None } if manifest.version == "9.0.0"),
        "{checked:?}"
    );
    assert!(release.requests.load(Ordering::SeqCst) > 0, "never asked");
    release.unchanged();
}

/// `off`: the manifest never asked for.
#[tokio::test]
async fn off_doesnt_check() {
    let key = Key::new();
    let release = Release::small(|a| key.sign(a));
    let checked = tick(&release, &key, Mode::Off).await;
    assert!(matches!(checked, Checked::Off), "{checked:?}");
    assert_eq!(release.requests.load(Ordering::SeqCst), 0, "asked anyway");
    release.unchanged();
}

/// `auto` that can't install it (another key's signature): offered, with why.
#[tokio::test]
async fn auto_offers_what_it_cant_install() {
    let key = Key::new();
    let release = Release::small(|_| Key::new().sign(b"another"));
    let checked = tick(&release, &key, Mode::Auto).await;
    assert!(
        matches!(
            &checked,
            Checked::Offered {
                why: Some(UpdateError::OtherKey),
                ..
            }
        ),
        "{checked:?}"
    );
    release.unchanged();
}

/// `theca` on `state`, its exit code and what it printed.
fn theca(state: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_theca"))
        .arg("--state")
        .arg(state)
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Without `self-update` (the .deb): both commands say who updates this
/// package, and fail.
#[cfg(not(feature = "self-update"))]
#[test]
fn without_the_feature_the_commands_name_the_channel() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.sqlite");
    let channel = if cfg!(target_os = "linux") {
        "updated by your package manager (apt, dnf or pacman)"
    } else {
        "doesn't update itself"
    };
    for args in [&["update"][..], &["updates"], &["updates", "auto"]] {
        let (code, _, err) = theca(&state, args);
        assert_eq!(code, i32::from(theca::exit::ERROR), "{args:?}: {err}");
        assert!(err.contains(channel), "{args:?}: {err}");
    }
}

/// `theca status` says a newer release, asked for then; `off` asks nothing.
#[cfg(feature = "self-update")]
#[test]
fn with_the_feature_status_shows_a_version_found() {
    let release = Release::small(|a| Key::new().sign(a));
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.sqlite");
    let (code, out, err) = theca(&state, &["--manifest", &release.manifest, "status"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.starts_with("Theca 9.0.0 is available: theca update"),
        "{out}"
    );
    let (code, out, _) = theca(&state, &["updates", "off"]);
    assert_eq!((code, out.as_str()), (0, "Updates: off\n"));
    let (_, out, _) = theca(&state, &["updates"]);
    assert_eq!(out, "Updates: off\n");
    // A mode that isn't one: clap's usage error, naming the modes.
    let (code, _, err) = theca(&state, &["updates", "weekly"]);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("\"weekly\" isn't one of auto, ask, off"),
        "{err}"
    );
    let (_, out, _) = theca(&state, &["updates"]);
    assert_eq!(out, "Updates: off\n", "a mode that isn't one stored");
    let asked = release.requests.load(Ordering::SeqCst);
    let (_, out, _) = theca(&state, &["--manifest", &release.manifest, "status"]);
    assert!(!out.contains("is available"), "{out}");
    assert_eq!(release.requests.load(Ordering::SeqCst), asked, "off asked");
}

/// `ci/manifest.py` writes what the client reads: each self-updating
/// package `ci/package.sh` names, under the platform key `Installed` asks
/// for, its URL in the release and its signature; `min_server` from
/// `ops::MIN_SERVER`; the packages without a signature (.dmg, .deb) left out.
#[cfg(unix)]
#[test]
fn the_release_manifest_is_what_the_client_reads() {
    let key = Key::new();
    let dist = tempfile::tempdir().unwrap();
    let packages = [
        ("theca-9.0.0-macos-arm64.app.tar.gz", "darwin-aarch64"),
        ("theca-9.0.0-macos-x86_64.app.tar.gz", "darwin-x86_64"),
        ("theca-9.0.0-windows-x86_64.exe", "windows-x86_64"),
        ("theca-9.0.0-linux-x86_64.AppImage", "linux-x86_64"),
        ("theca-9.0.0-linux-arm64.AppImage", "linux-aarch64"),
        (
            "theca-headless-9.0.0-windows-x86_64.zip",
            "windows-x86_64-headless",
        ),
        (
            "theca-headless-9.0.0-linux-x86_64.tar.gz",
            "linux-x86_64-headless",
        ),
        (
            "theca-headless-9.0.0-linux-arm64.tar.gz",
            "linux-aarch64-headless",
        ),
    ];
    for (file, _) in packages {
        std::fs::write(dist.path().join(file), file).unwrap();
        let sig = dist.path().join(format!("{file}.sig"));
        std::fs::write(sig, key.sign(file.as_bytes())).unwrap();
    }
    for unsigned in [
        "theca-9.0.0-macos-arm64.dmg",
        "theca-9.0.0-linux-x86_64.deb",
    ] {
        std::fs::write(dist.path().join(unsigned), "x").unwrap();
    }
    let notes = dist.path().join("notes.md");
    std::fs::write(&notes, "\n- Self-update\n").unwrap();
    let base = "https://github.com/theca-cloud/computer/releases/download/9.0.0";
    let script = |dist: &Path| {
        Command::new("python3")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/ci/manifest.py"))
            .args(["9.0.0", base])
            .arg(dist)
            .arg(&notes)
            .output()
            .unwrap()
    };
    let out = script(dist.path());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let manifest = update::Manifest::parse(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(manifest.version, "9.0.0");
    assert_eq!(
        manifest.min_server.as_deref(),
        Some(theca::ops::MIN_SERVER.0)
    );
    assert_eq!(manifest.platforms.len(), packages.len());
    for (file, target) in packages {
        let package = manifest.package(target).unwrap();
        assert_eq!(package.url, format!("{base}/{file}"));
        let signed =
            update::verify_release(&key.public(), &package.signature, file.as_bytes(), "9.0.0");
        assert_eq!(signed, Ok(()), "{target}");
    }
    // The names `Installed` asks for are among them.
    let here = Installed::Headless(PathBuf::new()).target();
    if matches!(std::env::consts::ARCH, "x86_64" | "aarch64") && !cfg!(target_os = "macos") {
        assert!(manifest.package(&here).is_ok(), "{here}");
    }
    // A signature of something that isn't a package: refused, nothing printed.
    std::fs::write(dist.path().join("notes.md.sig"), "x").unwrap();
    let out = script(dist.path());
    assert!(!out.status.success() && out.stdout.is_empty());
}

/// `theca status` doesn't wait on a manifest that doesn't come (a server
/// offline, a firewall): 3 s at most, then its status as ever.
#[cfg(feature = "self-update")]
#[test]
fn with_the_feature_status_doesnt_wait_on_the_manifest() {
    // Connections accepted, counted, never answered.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/latest.json", silent.local_addr().unwrap());
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = asked.clone();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in silent.incoming() {
            counted.fetch_add(1, Ordering::SeqCst);
            held.push(stream);
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.sqlite");
    let started = std::time::Instant::now();
    let (code, out, err) = theca(&state, &["--manifest", &url, "status"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("No account"), "{out}");
    let took = started.elapsed();
    assert!(
        took < std::time::Duration::from_secs(10),
        "status took {took:?}"
    );
    assert!(
        asked.load(Ordering::SeqCst) > 0,
        "the manifest never asked for"
    );
}

/// `min_server` against an account's real server (asked its version, as
/// `auto` asks it): a release needing a newer one refused, saying which
/// server and its version; one needing exactly it, or older, installed.
#[tokio::test]
async fn min_server_is_checked_against_each_accounts_server() {
    let w = common::world().await;
    let (_dir, state_path, account) = w.logged_in("laptop").await;
    let state = State::open(&state_path).unwrap();
    update::set_mode(&state, Mode::Auto).unwrap();
    let server = theca_client::Client::new(&account.instance)
        .unwrap()
        .check_version()
        .await
        .unwrap();
    let key = Key::new();
    let above = Release::needing("99.0.0", |a| key.sign(a));
    let checked = tick_on(&state, &above, &key).await;
    let Checked::Offered {
        why:
            Some(UpdateError::ServerTooOld {
                instance,
                server: said,
                ..
            }),
        ..
    } = checked
    else {
        panic!("not refused for the server: {checked:?}");
    };
    assert_eq!((instance, said), (account.instance.clone(), server.clone()));
    above.unchanged();
    let at = Release::needing(&server, |a| key.sign(a));
    let checked = tick_on(&state, &at, &key).await;
    assert!(matches!(checked, Checked::Installed(_)), "{checked:?}");
    let below = Release::needing("0.1.0", |a| key.sign(a));
    let checked = tick_on(&state, &below, &key).await;
    assert!(matches!(checked, Checked::Installed(_)), "{checked:?}");
}

/// An older release, signed by the right key, announced as newer (whoever
/// could change the manifest could do that): refused, nothing changed.
#[tokio::test]
async fn an_older_release_announced_as_newer_is_refused() {
    let key = Key::new();
    let release = Release::small(|a| key.sign_as(a, "theca-headless-1.2.0-linux-x86_64.tar.gz"));
    let error = refused(&release, &key, &server("1.6.0")).await;
    assert_eq!(
        error,
        UpdateError::NotThisVersion {
            version: "9.0.0".into(),
            signed: "theca-headless-1.2.0-linux-x86_64.tar.gz".into(),
        }
    );
}
