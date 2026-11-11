//! macOS's drive: what the File Provider extension and the app share.
//!
//! The extension decides nothing: every call the system makes is forwarded
//! to the running app over a Unix socket the app creates in the extension's
//! container (`protocol`), and the app answers from the drive's entries.

pub mod log_file;
pub mod protocol;
pub mod staged;

#[cfg(target_os = "macos")]
mod ext;
#[cfg(target_os = "macos")]
pub mod host;
#[cfg(target_os = "macos")]
mod ipc;

/// The extension's entry, if `_NSExtensionMain` isn't (the link flag lost).
#[cfg(target_os = "macos")]
pub fn extension_main() {
    ext::start();
}
