//! Theca's File Provider extension, started by the system (its entry point
//! is Foundation's `_NSExtensionMain`, build.rs; `main` only runs if that
//! flag is lost).

#[cfg(target_os = "macos")]
fn main() {
    theca_fileprovider::extension_main();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("theca-drive is macOS's File Provider extension");
}
