//! Self-update (sprint Self-update; wiki: Sync client): the app and headless
//! replacing their own package with a newer release, in the packages whose
//! channel doesn't update them (ADR 0006).

use crate::state::{self, State};

/// What the client does about a newer release by itself (`updates` in the
/// state's settings). A version found is offered in every mode, `off` too
/// when asked (Check for updates, `theca update`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Checks, downloads, installs and restarts.
    Auto,
    /// Checks and offers.
    Ask,
    /// Never checks by itself.
    Off,
}

const KEY: &str = "updates";

/// The key every release's packages and manifest are signed with (minisign,
/// as Tauri writes it: the base64 of the `.pub` file), for the app and
/// headless alike. A test build takes a test key's instead, at build time;
/// `ci/package.sh` refuses that on a tag.
#[cfg(feature = "self-update")]
pub const PUBLIC_KEY: &str = match option_env!("THECA_TEST_UPDATER_PUBKEY") {
    Some(key) => key,
    None => include_str!("../ci/updater.pub"),
}
.trim_ascii();

impl Mode {
    /// `auto` for the app; `ask` headless, whose user usually can't write the
    /// binary and whose admin chooses when a server changes.
    pub const DEFAULT: Mode = if cfg!(feature = "gui") {
        Mode::Auto
    } else {
        Mode::Ask
    };

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Ask => "ask",
            Mode::Off => "off",
        }
    }

    const ALL: [Mode; 3] = [Mode::Auto, Mode::Ask, Mode::Off];

    /// The mode `value` names (stored, or given to `theca updates` or the
    /// app), or what to say about it: the one place that reads one.
    pub fn parse(value: &str) -> Result<Mode, String> {
        Mode::ALL
            .into_iter()
            .find(|m| m.as_str() == value)
            .ok_or_else(|| {
                let names: Vec<_> = Mode::ALL.iter().map(|m| m.as_str()).collect();
                format!("{value:?} isn't one of {}", names.join(", "))
            })
    }
}

/// The mode, as stored (`Mode::parse`). Missing is the default; a value
/// that isn't a mode too, and said, never an error.
pub fn mode(state: &State) -> state::Result<Mode> {
    Ok(match state.setting(KEY)? {
        None => Mode::DEFAULT,
        Some(value) => Mode::parse(&value).unwrap_or_else(|why| {
            tracing::warn!("updates: {why}: the default");
            Mode::DEFAULT
        }),
    })
}

pub fn set_mode(state: &State, mode: Mode) -> state::Result<()> {
    state.set_setting(KEY, mode.as_str())
}

/// What `theca update`, `theca updates` and the app's update commands say
/// in a package that doesn't update itself: who does.
#[cfg(not(feature = "self-update"))]
pub const NOT_SELF_UPDATING: &str = if cfg!(target_os = "linux") {
    "this package is updated by your package manager (apt, dnf or pacman), not by Theca itself"
} else {
    "this build of Theca doesn't update itself: install a release from https://theca.rs/computer"
};

/// Why a release isn't installed. Each says what to do, if anything.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UpdateError {
    #[error("the release has no package for {0}")]
    NoPackage(String),
    #[error("the package isn't signed by Theca's release key")]
    OtherKey,
    #[error("the package doesn't match its signature")]
    BadSignature,
    #[error(
        "the package signed is {signed:?}, not Theca {version}'s: an older release announced as newer"
    )]
    NotThisVersion { version: String, signed: String },
    #[error("Theca {version} needs a server on {min} or later: {instance} runs {server}")]
    ServerTooOld {
        version: String,
        min: String,
        instance: String,
        server: String,
    },
    #[error(
        "Theca {version} needs a server on {min} or later: {instance} couldn't be asked its version"
    )]
    ServerUnknown {
        version: String,
        min: String,
        instance: String,
    },
    #[error("the update couldn't be downloaded: {0}")]
    Download(String),
    #[error("the client state couldn't be read: {0}")]
    State(String),
    #[error("Tauri's updater: {0}")]
    Updater(String),
    #[error("the update's archive doesn't hold {0}")]
    Archive(String),
    #[error(
        "{path} can't be replaced ({reason}): nothing changed; run `theca update` as a user who can write it"
    )]
    CantWrite { path: String, reason: String },
    #[error("this app wasn't started from its package (an .AppImage): update it by hand")]
    NotInstalled,
    #[error("an update is already being installed")]
    Busy,
}

/// A release's `latest.json`: Tauri's updater format, plus the headless
/// archives (`<os>-<arch>-headless`) and `min_server`. It only grows (every
/// installed client reads the newest): fields unknown here are ignored.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Manifest {
    pub version: String,
    pub platforms: std::collections::HashMap<String, Package>,
    /// The oldest server (its `core`, as `/api/version` gives it) this
    /// release works with.
    #[serde(default)]
    pub min_server: Option<String>,
}

/// One package of a release: where it is, and its minisign signature (the
/// base64 of the `.sig` file, as Tauri writes it).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Package {
    pub url: String,
    pub signature: String,
}

impl Manifest {
    /// The manifest in `json`; one that doesn't parse, or whose versions
    /// aren't versions, is no update, and said.
    pub fn parse(json: &str) -> Option<Manifest> {
        let parsed = serde_json::from_str::<Manifest>(json)
            .map_err(|e| e.to_string())
            .and_then(|m| {
                let versions = std::iter::once(&m.version).chain(&m.min_server);
                match versions
                    .map(|v| semver::Version::parse(v))
                    .find_map(Result::err)
                {
                    Some(e) => Err(format!("a version isn't one: {e}")),
                    None => Ok(m),
                }
            });
        parsed
            .inspect_err(|error| {
                tracing::warn!(error, "the update manifest doesn't parse: no update")
            })
            .ok()
    }

    /// The release's page, its notes.
    pub fn notes_url(&self) -> String {
        format!(
            "https://github.com/theca-cloud/computer/releases/tag/{}",
            self.version
        )
    }

    /// Newer than the running client's `current` version.
    pub fn is_newer_than(&self, current: &str) -> bool {
        let version = |v: &str| semver::Version::parse(v).ok();
        match (version(&self.version), version(current)) {
            (Some(release), Some(current)) => release > current,
            _ => false,
        }
    }

    /// The package for `target` (`darwin-aarch64`, `linux-x86_64-headless`...).
    pub fn package(&self, target: &str) -> Result<&Package, UpdateError> {
        self.platforms
            .get(target)
            .ok_or_else(|| UpdateError::NoPackage(target.to_owned()))
    }

    /// Every account's server (its URL, and its version: none when it
    /// couldn't be asked) runs `min_server` or later: an update never cuts a
    /// client off a server. One that can't be asked refuses it too, until
    /// the next check.
    pub fn check_servers(&self, servers: &[(String, Option<String>)]) -> Result<(), UpdateError> {
        let Some(min) = &self.min_server else {
            return Ok(());
        };
        let parsed = semver::Version::parse(min).ok();
        for (instance, server) in servers {
            let Some(server) = server else {
                return Err(UpdateError::ServerUnknown {
                    version: self.version.clone(),
                    min: min.clone(),
                    instance: instance.to_owned(),
                });
            };
            let old = semver::Version::parse(server)
                .ok()
                .zip(parsed.as_ref())
                .is_none_or(|(server, min)| &server < min);
            if old {
                return Err(UpdateError::ServerTooOld {
                    version: self.version.clone(),
                    min: min.clone(),
                    instance: instance.to_owned(),
                    server: server.to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// The text Tauri's base64 holds (a `.pub` or `.sig` file).
fn base64_text(b64: &str) -> Option<String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    String::from_utf8(bytes).ok()
}

fn public_key(key: &str) -> Option<minisign_verify::PublicKey> {
    minisign_verify::PublicKey::decode(&base64_text(key)?).ok()
}

/// `bytes` are what `signature` signed with `key` (the base64 of a minisign
/// `.pub` file, as `PUBLIC_KEY`), and the signature's trusted comment
/// (signed with the rest) names a file of `version` (`file:theca-headless-1.3.0-…`,
/// `file:Theca_1.3.0_amd64.AppImage`, as Tauri's signer writes it): the
/// manifest isn't signed, so an older signed package announced as newer
/// would otherwise be taken (a rollback).
pub fn verify_release(
    key: &str,
    signature: &str,
    bytes: &[u8],
    version: &str,
) -> Result<(), UpdateError> {
    let signature = verified(key, signature, bytes)?;
    let file = signature
        .trusted_comment()
        .split('\t')
        .find_map(|field| field.strip_prefix("file:"))
        .unwrap_or("");
    // The name as the release writes it, the version followed by what can
    // only come after a whole one: a headless archive's OS
    // (`theca-headless-2.0.0-rc.1-…` isn't 2.0.0's), an `.AppImage`'s `_`
    // (Tauri writes a prerelease's version whole: `Theca_2.0.0-rc.1_…`).
    let headless = file
        .strip_prefix(&format!("theca-headless-{version}-"))
        .is_some_and(|rest| {
            // No headless macOS archive since 1.2.0 (a Mac's CLI is the app's).
            ["linux-", "windows-"].iter().any(|os| rest.starts_with(os))
        });
    let appimage = file.starts_with(&format!("Theca_{version}_"));
    if headless || appimage {
        Ok(())
    } else {
        Err(UpdateError::NotThisVersion {
            version: version.to_owned(),
            signed: file.to_owned(),
        })
    }
}

fn verified(
    key: &str,
    signature: &str,
    bytes: &[u8],
) -> Result<minisign_verify::Signature, UpdateError> {
    let key = public_key(key).ok_or(UpdateError::OtherKey)?;
    let signature = base64_text(signature)
        .and_then(|s| minisign_verify::Signature::decode(&s).ok())
        .ok_or(UpdateError::BadSignature)?;
    // Prehashed only, as `tauri signer` signs: no legacy signature.
    key.verify(bytes, &signature, false).map_err(|e| match e {
        minisign_verify::Error::UnexpectedKeyId => UpdateError::OtherKey,
        _ => UpdateError::BadSignature,
    })?;
    Ok(signature)
}

/// Where every release's manifest is: the latest GitHub release's.
pub const MANIFEST: &str =
    "https://github.com/theca-cloud/computer/releases/latest/download/latest.json";

/// What a self-update replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    /// The headless binary (where a link to it leads).
    Headless(std::path::PathBuf),
    /// The `.AppImage` the app runs from (`$APPIMAGE`: the running binary
    /// is inside its mount), updated by this code, not Tauri's: Tauri's needs
    /// a display.
    AppImage(std::path::PathBuf),
}

impl Installed {
    /// This process's, if this code updates it: headless, or the app from an
    /// `.AppImage` (Tauri's updater updates the app on macOS and Windows).
    pub fn this() -> Option<Installed> {
        if !cfg!(feature = "gui") {
            std::env::current_exe().ok().map(Installed::Headless)
        } else if cfg!(target_os = "linux") {
            std::env::var_os("APPIMAGE").map(|p| Installed::AppImage(p.into()))
        } else {
            None
        }
    }

    pub fn path(&self) -> &std::path::Path {
        match self {
            Installed::Headless(path) | Installed::AppImage(path) => path,
        }
    }

    /// Its entry in a manifest's `platforms`: `<os>-<arch>` as Tauri names
    /// them (the `.AppImage` is `linux-<arch>`), `-headless` for headless.
    pub fn target(&self) -> String {
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            os => os,
        };
        let headless = match self {
            Installed::Headless(_) => "-headless",
            Installed::AppImage(_) => "",
        };
        format!("{os}-{}{headless}", std::env::consts::ARCH)
    }

    /// The new release started with `args`: from the installed file (never
    /// `current_exe`, inside an `.AppImage`'s mount).
    pub fn restart_command(
        &self,
        args: impl IntoIterator<Item = std::ffi::OsString>,
    ) -> std::process::Command {
        let mut command = std::process::Command::new(self.path());
        command.args(args);
        command
    }

    /// The new release in this process's place, with its arguments: returns
    /// only if it can't.
    #[cfg(unix)]
    pub fn restart(&self) -> std::io::Error {
        use std::os::unix::process::CommandExt;
        self.restart_command(std::env::args_os().skip(1)).exec()
    }

    /// The new release started with this process's arguments, and this
    /// process exits; returns only if it can't.
    #[cfg(windows)]
    pub fn restart(&self) -> std::io::Error {
        match self.restart_command(std::env::args_os().skip(1)).spawn() {
            Ok(_) => std::process::exit(0),
            Err(e) => e,
        }
    }
}

/// The release at `url` (`MANIFEST`, or `theca update --manifest`'s), if
/// it's newer than this client; one that doesn't parse is none.
pub async fn check(url: &str) -> Result<Option<Manifest>, UpdateError> {
    let json = fetch(url).await?;
    Ok(Manifest::parse(&String::from_utf8_lossy(&json))
        .filter(|m| m.is_newer_than(env!("CARGO_PKG_VERSION"))))
}

async fn fetch(url: &str) -> Result<Vec<u8>, UpdateError> {
    let error = |e: reqwest::Error| UpdateError::Download(e.to_string());
    // theca-client's TLS provider, installed as it does.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(error)?;
    let response = client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(error)?;
    Ok(response.bytes().await.map_err(error)?.to_vec())
}

/// `installed` replaced by `manifest`'s package, verified with `key`
/// (`PUBLIC_KEY`, or a test's), once every account's server
/// (`check_servers`) runs `min_server`. Refused, nothing changes.
pub async fn install(
    manifest: &Manifest,
    key: &str,
    servers: &[(String, Option<String>)],
    installed: &Installed,
) -> Result<(), UpdateError> {
    manifest.check_servers(servers)?;
    let package = manifest.package(&installed.target())?;
    // The file itself, not a link to it (`/usr/local/bin/theca`); its folder
    // written to before anything is downloaded (`auto` would download the
    // package at every check, to end here).
    let path = installed.path();
    let path = std::fs::canonicalize(path).map_err(|e| cant_write(path, e))?;
    writable(&path)?;
    let download = fetch(&package.url).await?;
    verify_release(key, &package.signature, &download, &manifest.version)?;
    let binary = match installed {
        Installed::Headless(_) => unpack(&download)?,
        // Tauri's `.AppImage` package is the file itself.
        Installed::AppImage(_) => download,
    };
    replace(&path, &binary)
}

/// A file can be made beside `exe` (`replace` makes one there).
fn writable(exe: &std::path::Path) -> Result<(), UpdateError> {
    let dir = exe
        .parent()
        .ok_or_else(|| cant_write(exe, std::io::Error::other("no folder")))?;
    tempfile::Builder::new()
        .prefix(".theca-update")
        .tempfile_in(dir)
        .map(drop)
        .map_err(|e| cant_write(exe, e))
}

/// How often the app and `theca run` check, after their start.
pub const EVERY: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// What a check came to.
#[derive(Debug)]
pub enum Checked {
    /// `off`: nothing asked.
    Off,
    /// No newer release.
    Latest,
    /// A newer release, offered: in `ask`, or in `auto` when it couldn't be
    /// installed (why).
    Offered {
        manifest: Manifest,
        why: Option<UpdateError>,
    },
    /// Installed: to start in this process's place.
    Installed(Manifest),
    /// The manifest couldn't be read: tried again at the next check.
    Failed(UpdateError),
}

/// One check, as the state's mode says, against the manifest at `url`;
/// in `auto`, a newer release is installed by `install` (given it and every
/// account's server: `install`, or Tauri's updater in the app).
pub async fn tick<F, Fut>(state: &State, url: &str, install: F) -> Checked
where
    F: FnOnce(Manifest, Vec<(String, Option<String>)>) -> Fut,
    Fut: std::future::Future<Output = Result<(), UpdateError>>,
{
    let mode = match mode(state) {
        Ok(mode) => mode,
        Err(e) => return Checked::Failed(UpdateError::State(e.to_string())),
    };
    if mode == Mode::Off {
        return Checked::Off;
    }
    let manifest = match check(url).await {
        Ok(Some(manifest)) => manifest,
        Ok(None) => return Checked::Latest,
        Err(e) => {
            tracing::warn!("no update check: {e}");
            return Checked::Failed(e);
        }
    };
    tracing::info!(version = manifest.version, "a newer release");
    if mode == Mode::Ask {
        return Checked::Offered {
            manifest,
            why: None,
        };
    }
    match install(manifest.clone(), servers(state).await).await {
        Ok(()) => Checked::Installed(manifest),
        Err(e) => {
            tracing::warn!(version = manifest.version, "not installed: {e}");
            Checked::Offered {
                manifest,
                why: Some(e),
            }
        }
    }
}

/// Every account's server and its version (none: it couldn't be asked).
pub async fn servers(state: &State) -> Vec<(String, Option<String>)> {
    let mut servers = Vec::new();
    for account in state.accounts().unwrap_or_default() {
        let version = match theca_client::Client::new(&account.instance) {
            Ok(client) => client.check_version().await.ok(),
            Err(_) => None,
        };
        servers.push((account.instance, version));
    }
    servers
}

/// `tick` at once, then every `EVERY`, until it says to stop (installed) or
/// `shutdown`.
pub async fn schedule<F, Fut>(mut tick: F, mut shutdown: tokio::sync::watch::Receiver<bool>)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::ops::ControlFlow<()>>,
{
    loop {
        if tick().await.is_break() {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(EVERY) => {}
            _ = shutdown.changed() => return,
        }
    }
}

/// The binary in a headless archive: `<folder>/theca` in a `.tar.gz`.
#[cfg(not(windows))]
fn unpack(archive: &[u8]) -> Result<Vec<u8>, UpdateError> {
    use std::io::Read;
    let error = |e: std::io::Error| UpdateError::Archive(format!("theca ({e})"));
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries().map_err(error)? {
        let mut entry = entry.map_err(error)?;
        let path = entry.path().map_err(error)?.into_owned();
        if path.components().count() == 2 && path.file_name() == Some("theca".as_ref()) {
            let mut binary = Vec::new();
            entry.read_to_end(&mut binary).map_err(error)?;
            return Ok(binary);
        }
    }
    Err(UpdateError::Archive("theca".into()))
}

/// The binary in a headless archive: `<folder>/theca.exe` in a `.zip`.
#[cfg(windows)]
fn unpack(archive: &[u8]) -> Result<Vec<u8>, UpdateError> {
    use std::io::Read;
    let error = |e: &dyn std::fmt::Display| UpdateError::Archive(format!("theca.exe ({e})"));
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).map_err(|e| error(&e))?;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).map_err(|e| error(&e))?;
        let Some(path) = file.enclosed_name() else {
            continue;
        };
        if path.components().count() == 2 && path.file_name() == Some("theca.exe".as_ref()) {
            let mut binary = Vec::new();
            file.read_to_end(&mut binary).map_err(|e| error(&e))?;
            return Ok(binary);
        }
    }
    Err(UpdateError::Archive("theca.exe".into()))
}

fn cant_write(path: &std::path::Path, e: std::io::Error) -> UpdateError {
    UpdateError::CantWrite {
        path: path.display().to_string(),
        reason: e.to_string(),
    }
}

/// Where the running binary goes aside on Windows (it can be renamed, not
/// replaced): `theca.exe.old`, or, while an older one set aside still runs
/// from there (it can't be removed), the first `theca.exe.old.<n>` free.
#[cfg(any(windows, test))]
fn aside(exe: &std::path::Path) -> std::path::PathBuf {
    let first = exe.with_extension("exe.old");
    let names: Vec<_> = std::iter::once(first.clone())
        .chain((1..100).map(|n| first.with_extension(format!("old.{n}"))))
        .collect();
    // Every one set aside before that nothing runs from any more: removed.
    for name in names.iter().filter(|n| n.exists()) {
        let _ = std::fs::remove_file(name);
    }
    names
        .into_iter()
        .find(|n| !n.exists())
        // None free: the rename fails, said as a binary it can't write.
        .unwrap_or(first)
}

/// `binary` written beside `exe`, then renamed over it: never half a binary.
/// On Windows the running one is renamed aside first (`aside`: removed at the
/// next update, once nothing runs from it), and back if the new one can't
/// take its place.
fn replace(exe: &std::path::Path, binary: &[u8]) -> Result<(), UpdateError> {
    use std::io::Write;
    let cant = |e| cant_write(exe, e);
    let dir = exe
        .parent()
        .ok_or_else(|| cant(std::io::Error::other("no folder")))?;
    let mut new = tempfile::Builder::new()
        .prefix(".theca-update")
        .tempfile_in(dir)
        .map_err(cant)?;
    new.write_all(binary).map_err(cant)?;
    let permissions = std::fs::metadata(exe).map_err(cant)?.permissions();
    new.as_file().set_permissions(permissions).map_err(cant)?;
    new.as_file().sync_all().map_err(cant)?;
    #[cfg(windows)]
    {
        let aside = aside(exe);
        std::fs::rename(exe, &aside).map_err(cant)?;
        if let Err(e) = new.persist(exe) {
            let _ = std::fs::rename(&aside, exe);
            return Err(cant(e.error));
        }
    }
    #[cfg(not(windows))]
    new.persist(exe).map_err(|e| cant(e.error))?;
    tracing::info!(path = %exe.display(), "updated");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Missing, unknown, invalid: the default; each mode read back as stored.
    #[test]
    fn the_mode_falls_back_to_its_default() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open(&dir.path().join("state.sqlite")).unwrap();
        assert_eq!(mode(&state).unwrap(), Mode::DEFAULT);
        for m in [Mode::Off, Mode::Auto, Mode::Ask, Mode::Off] {
            set_mode(&state, m).unwrap();
            assert_eq!(mode(&state).unwrap(), m);
        }
        for bad in ["weekly", "Auto", " ask", ""] {
            state.set_setting(KEY, bad).unwrap();
            let log = logged(|| assert_eq!(mode(&state).unwrap(), Mode::DEFAULT, "{bad:?}"));
            assert!(log.contains("isn't one of auto, ask, off"), "{log}");
        }
    }

    /// The signature alone, its trusted comment not looked at.
    fn verify(key: &str, signature: &str, bytes: &[u8]) -> Result<(), UpdateError> {
        verified(key, signature, bytes).map(drop)
    }

    /// A key pair of the test's own, its public half as `PUBLIC_KEY` is.
    struct Key(minisign::KeyPair);

    impl Key {
        fn new() -> Key {
            Key(minisign::KeyPair::generate_unencrypted_keypair().unwrap())
        }
        fn public(&self) -> String {
            b64(&self.0.pk.to_box().unwrap().into_string())
        }
        /// As `tauri signer sign` writes a `.sig`.
        fn sign(&self, bytes: &[u8]) -> String {
            let sig = minisign::sign(Some(&self.0.pk), &self.0.sk, bytes, None, None).unwrap();
            b64(&sig.into_string())
        }
        /// As Tauri's signer signs the package `file`.
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

    /// A manifest as `release.yml` writes it, `extra` added to it and to its
    /// package.
    fn manifest(signature: &str, extra: &str) -> String {
        format!(
            r#"{{
              "version": "1.3.0",
              "notes": "See the release",
              "pub_date": "2026-11-20T10:00:00Z",
              "min_server": "1.6.0",
              "platforms": {{
                "linux-x86_64-headless": {{
                  "url": "https://example.invalid/theca.tar.gz",
                  "signature": "{signature}"{extra}
                }}
              }}{extra}
            }}"#
        )
    }

    const PACKAGE: &[u8] = b"the package's bytes";

    /// A release's manifest read, its package found and verified.
    #[test]
    fn a_good_manifest_is_read_and_verified() {
        let key = Key::new();
        let m = Manifest::parse(&manifest(&key.sign(PACKAGE), "")).unwrap();
        assert!(m.is_newer_than("1.2.0"));
        assert!(!m.is_newer_than("1.3.0") && !m.is_newer_than("1.4.0"));
        let package = m.package("linux-x86_64-headless").unwrap();
        assert_eq!(package.url, "https://example.invalid/theca.tar.gz");
        assert_eq!(verify(&key.public(), &package.signature, PACKAGE), Ok(()));
        assert_eq!(
            m.package("windows-x86_64-headless").unwrap_err(),
            UpdateError::NoPackage("windows-x86_64-headless".into())
        );
    }

    /// Other bytes, a signature that isn't one, another key's: refused.
    #[test]
    fn a_bad_signature_is_refused() {
        let key = Key::new();
        let signature = key.sign(PACKAGE);
        assert_eq!(
            verify(&key.public(), &signature, b"other bytes"),
            Err(UpdateError::BadSignature)
        );
        assert_eq!(
            verify(&key.public(), &b64("not a signature"), PACKAGE),
            Err(UpdateError::BadSignature)
        );
        assert_eq!(
            verify(&key.public(), &Key::new().sign(PACKAGE), PACKAGE),
            Err(UpdateError::OtherKey)
        );
    }

    /// A release's package names its version, whole: 1.2.1's isn't 1.2.10's,
    /// nor one that names none.
    #[test]
    fn a_package_signed_for_another_version_is_refused() {
        let key = Key::new();
        let ok = |file: &str, version: &str| {
            verify_release(&key.public(), &key.sign_as(PACKAGE, file), PACKAGE, version)
        };
        assert_eq!(
            ok("theca-headless-1.2.1-linux-x86_64.tar.gz", "1.2.1"),
            Ok(())
        );
        assert_eq!(ok("Theca_1.2.1_amd64.AppImage", "1.2.1"), Ok(()));
        assert_eq!(
            ok("theca-headless-1.2.1-windows-x86_64.zip", "1.2.1"),
            Ok(())
        );
        for (file, version) in [
            ("theca-headless-1.2.10-linux-x86_64.tar.gz", "1.2.1"),
            ("theca-headless-1.2.1-linux-x86_64.tar.gz", "1.2.10"),
            ("Theca_11.2.1_amd64.AppImage", "1.2.1"),
            ("theca.tar.gz", "1.2.1"),
            // A prerelease's package, for its final version.
            ("theca-headless-2.0.0-rc.1-linux-x86_64.tar.gz", "2.0.0"),
            ("Theca_2.0.0-rc.1_amd64.AppImage", "2.0.0"),
            // A headless macOS archive, which no release has.
            ("theca-headless-1.2.1-macos-arm64.tar.gz", "1.2.1"),
            // The version elsewhere in the name.
            ("old-theca-headless-1.2.1-linux-x86_64.tar.gz", "1.2.1"),
        ] {
            assert!(
                matches!(ok(file, version), Err(UpdateError::NotThisVersion { .. })),
                "{file} taken for {version}"
            );
        }
        let unnamed = verify_release(&key.public(), &key.sign(PACKAGE), PACKAGE, "1.2.1");
        assert!(matches!(unnamed, Err(UpdateError::NotThisVersion { .. })));
    }

    /// The release key embedded in a self-updating build is one.
    #[cfg(feature = "self-update")]
    #[test]
    fn the_embedded_key_decodes() {
        assert!(public_key(PUBLIC_KEY).is_some());
        assert!(public_key(&b64("not a key")).is_none());
    }

    /// The servers' list as `servers` gives it.
    fn owned(list: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
        list.iter()
            .map(|(i, v)| (i.to_string(), v.map(str::to_owned)))
            .collect()
    }

    /// Each account's server at `min_server` or later; one older, or that
    /// couldn't be asked, refuses the release, saying why.
    #[test]
    fn a_server_older_than_min_server_refuses_it() {
        let m = Manifest::parse(&manifest("sig", "")).unwrap();
        let a = "https://a.example";
        let b = "https://b.example";
        assert_eq!(
            m.check_servers(&owned(&[(a, Some("1.6.0")), (b, Some("2.0.0"))])),
            Ok(())
        );
        let too_old = m
            .check_servers(&owned(&[(a, Some("1.6.0")), (b, Some("1.5.9"))]))
            .unwrap_err();
        assert_eq!(
            too_old.to_string(),
            "Theca 1.3.0 needs a server on 1.6.0 or later: https://b.example runs 1.5.9"
        );
        assert!(matches!(
            m.check_servers(&owned(&[(a, Some("1.6.0-rc"))])),
            Err(UpdateError::ServerTooOld { .. })
        ));
        assert!(matches!(
            m.check_servers(&owned(&[(a, Some("not a version"))])),
            Err(UpdateError::ServerTooOld { .. })
        ));
        assert_eq!(
            m.check_servers(&owned(&[(a, None)]))
                .unwrap_err()
                .to_string(),
            "Theca 1.3.0 needs a server on 1.6.0 or later: https://a.example couldn't be asked its version"
        );
        let without = Manifest {
            min_server: None,
            ..m
        };
        assert_eq!(without.check_servers(&owned(&[(a, Some("0.1.0"))])), Ok(()));
    }

    /// A field this client doesn't know, in the manifest or a package: still
    /// read (the manifest only grows).
    #[test]
    fn a_field_added_is_still_read() {
        let key = Key::new();
        let m = Manifest::parse(&manifest(
            &key.sign(PACKAGE),
            r#", "delta": {"from": "1.2.0"}"#,
        ))
        .unwrap();
        let package = m.package("linux-x86_64-headless").unwrap();
        assert_eq!(verify(&key.public(), &package.signature, PACKAGE), Ok(()));
    }

    /// Not JSON, a field missing, a version that isn't one: no update, and
    /// said in the log.
    #[test]
    fn a_manifest_that_doesnt_parse_is_no_update() {
        for bad in [
            "not json".to_owned(),
            r#"{"version": "1.3.0"}"#.to_owned(),
            manifest("sig", "").replace("\"1.3.0\"", "\"next\""),
            manifest("sig", "").replace("\"1.6.0\"", "\"soon\""),
        ] {
            let log = logged(|| assert!(Manifest::parse(&bad).is_none(), "{bad}"));
            assert!(log.contains("the update manifest doesn't parse"), "{log}");
        }
    }

    /// What `f` logs.
    fn logged(f: impl FnOnce()) -> String {
        use std::sync::{Arc, Mutex};
        #[derive(Clone)]
        struct Buffer(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buffer {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buffer = Buffer(Arc::default());
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = buffer.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    /// The old binary set aside: `theca.exe.old`, replaced; one that can't be
    /// removed (still running, on Windows; a folder here) kept, the next
    /// name taken.
    #[test]
    fn the_old_binary_goes_where_it_can() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("theca.exe");
        let old = dir.path().join("theca.exe.old");
        assert_eq!(aside(&exe), old);
        std::fs::write(&old, "an older one, not running").unwrap();
        assert_eq!(aside(&exe), old);
        assert!(!old.exists(), "the older one removed");
        std::fs::create_dir(&old).unwrap();
        std::fs::write(old.join("held"), "").unwrap();
        // One left from an update before, nothing running from it any more.
        let earlier = dir.path().join("theca.exe.old.2");
        std::fs::write(&earlier, "set aside, then left").unwrap();
        assert_eq!(aside(&exe), dir.path().join("theca.exe.old.1"));
        assert!(old.join("held").exists(), "the one it can't remove kept");
        assert!(!earlier.exists(), "the one left from before removed");
    }

    /// The app installs by itself; headless asks.
    #[test]
    fn the_default_depends_on_the_build() {
        let expected = if cfg!(feature = "gui") {
            Mode::Auto
        } else {
            Mode::Ask
        };
        assert_eq!(Mode::DEFAULT, expected);
    }
}
