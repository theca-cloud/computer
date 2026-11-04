fn main() {
    // The app's build only: the CLI's needs no Tauri (and no tauri.conf.json).
    #[cfg(feature = "gui")]
    tauri_build::build();
}
