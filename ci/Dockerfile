# check=skip=InvalidDefaultArgInFrom
# CI image for `computer`: the Rust toolchain and tools, and what the app
# (Tauri) needs on Linux to build and to run its smoke tests under a virtual
# screen. Built and pushed by scripts/ci-image.sh; the tag is the Rust version
# (rust-toolchain.toml).
ARG RUST_VERSION
FROM rust:${RUST_VERSION}
ARG TAURI_DRIVER_VERSION=2.1.0

RUN rustup component add rustfmt clippy \
 && cargo install --locked cargo-deny \
 && cargo install --locked tauri-driver --version "$TAURI_DRIVER_VERSION" \
 && rm -rf "$CARGO_HOME/registry" "$CARGO_HOME/git"

# Tauri's Linux prerequisites (WebKitGTK 4.1, the tray's appindicator, SVG
# icons); the page's build (Node); the smoke tests: a virtual screen (Xvfb), a
# D-Bus session (the single-instance plugin uses it), WebKit's WebDriver.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev libssl-dev \
      nodejs npm \
      xvfb xauth dbus dbus-x11 webkit2gtk-driver \
 && rm -rf /var/lib/apt/lists/*
