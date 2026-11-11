// A .appex is an MH_EXECUTE whose entry point is Foundation's _NSExtensionMain,
// linked application-extension-safe (what Xcode does for an app extension).
// Only for this bin: through RUSTFLAGS the flags would hit every build script.
fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo::rustc-link-arg-bin=theca-drive=-Wl,-e,_NSExtensionMain");
        println!("cargo::rustc-link-arg-bin=theca-drive=-Wl,-application_extension");
    }
}
