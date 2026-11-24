//! Start at login (theca/computer#92): `auto-launch`, called directly rather
//! than through `tauri-plugin-autostart`, which never gives it the app's bundle
//! identifier. Without it, macOS's launch agent names no app, and Login Items
//! lists the signer (the developer's name) instead of Theca. The same file
//! `~/Library/LaunchAgents/Theca.plist` the plugin wrote, now with
//! `AssociatedBundleIdentifiers`.

use std::path::Path;

use auto_launch::{AutoLaunch, AutoLaunchBuilder, MacOSLaunchMode};

/// The app's bundle identifier (`tauri.conf.json`): what macOS names a launch
/// agent after.
const BUNDLE_ID: &str = "rs.theca.computer";

/// What the entry starts the app with: in the tray, no window.
const ARGS: [&str; 1] = ["--background"];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Launch(#[from] auto_launch::Error),
    #[error("the app's own path can't be read: {0}")]
    Path(#[from] std::io::Error),
}

/// The launcher for the running app: `name` is the entry's (`Theca`), the
/// path the AppImage's where there is one (Linux), the binary's otherwise (on
/// a Mac, inside the bundle: a launch agent starts the binary).
pub fn launcher(name: &str, appimage: Option<&Path>) -> Result<AutoLaunch, Error> {
    let path = match appimage {
        Some(appimage) => appimage.to_owned(),
        None => std::env::current_exe()?.canonicalize()?,
    };
    Ok(AutoLaunchBuilder::new()
        .set_app_name(name)
        .set_app_path(&path.to_string_lossy())
        .set_args(&ARGS)
        .set_macos_launch_mode(MacOSLaunchMode::LaunchAgent)
        .set_bundle_identifiers(&[BUNDLE_ID])
        .build()?)
}

/// Turns the entry on or off.
pub fn set(launcher: &AutoLaunch, on: bool) -> Result<(), Error> {
    if on {
        launcher.enable()?;
    } else {
        launcher.disable()?;
    }
    Ok(())
}

/// Written again when it is on: an entry made by 1.3.x has no bundle
/// identifier, and one made before the app moved points at its old place.
/// Left alone when it is off, so one the user turned off stays off.
pub fn refresh(launcher: &AutoLaunch) -> Result<(), Error> {
    if launcher.is_enabled()? {
        launcher.enable()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The entry starts the app in the tray, under the name given, at the
    /// AppImage's path where there is one.
    #[test]
    fn the_entry_starts_the_app_in_the_tray() {
        let launcher = launcher("Theca", Some(Path::new("/opt/Theca.AppImage"))).unwrap();
        assert_eq!(launcher.get_app_name(), "Theca");
        assert_eq!(launcher.get_app_path(), "/opt/Theca.AppImage");
        assert_eq!(launcher.get_args(), ["--background"]);
    }

    /// The entry written names the app: on a Mac, the launch agent's bundle
    /// identifier (what Login Items lists it by: theca/computer#92), on Linux
    /// the desktop entry; started in the tray; one that is off stays off
    /// through `refresh`. The library writes under `$HOME` and this crate
    /// forbids `unsafe` (`set_var`): the test runs its own binary again with a
    /// `HOME` of its own, where the real checks are (`entry_under_home`).
    #[test]
    fn the_entry_names_the_app_and_stays_off_when_off() {
        if std::env::var_os("THECA_TEST_AUTOSTART_HOME").is_some() {
            return entry_under_home();
        }
        let home = tempfile::tempdir().unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "autostart::tests::the_entry_names_the_app_and_stays_off_when_off",
                "--nocapture",
            ])
            .env("HOME", home.path())
            .env("THECA_TEST_AUTOSTART_HOME", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The checks, under the `HOME` the outer test gave.
    fn entry_under_home() {
        let launcher = launcher("Theca", None).unwrap();
        assert!(!launcher.is_enabled().unwrap());
        refresh(&launcher).unwrap();
        assert!(!launcher.is_enabled().unwrap(), "refresh turned it on");
        set(&launcher, true).unwrap();
        assert!(launcher.is_enabled().unwrap());
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        #[cfg(target_os = "macos")]
        let entry = home.join("Library/LaunchAgents/Theca.plist");
        #[cfg(not(target_os = "macos"))]
        let entry = home.join(".config/autostart/Theca.desktop");
        let written = std::fs::read_to_string(&entry).unwrap();
        assert!(written.contains("--background"), "{written}");
        #[cfg(target_os = "macos")]
        assert!(
            written.contains(BUNDLE_ID),
            "no bundle identifier: {written}"
        );
        set(&launcher, false).unwrap();
        refresh(&launcher).unwrap();
        assert!(!launcher.is_enabled().unwrap(), "refresh turned it back on");
        assert!(!entry.exists());
    }
}
