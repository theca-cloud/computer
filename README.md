# Theca for computers

The Theca app for macOS, Windows and Linux: local folders kept identical to Theca libraries, both ways. A tray app with a small window, or a CLI and daemon (`theca`) for servers.

## Development

Prerequisites: Rust (pinned in `rust-toolchain.toml`), Node, Docker and `cargo install --locked cargo-deny`. The end-to-end tests run `core`'s server, built from the pinned release's sources (`scripts/ci.sh` does it). On Linux, the app needs WebKitGTK 4.1 and appindicator (Tauri's prerequisites).

```sh
scripts/dev.sh                 # the tests' Postgres (port in .env), idempotent
npm --prefix ui ci             # the app's page, once
cargo run -- --help            # the CLI (no app in this build)
npm --prefix ui run build && cargo run --features gui,custom-protocol   # the app
scripts/ci.sh [lint|test|app|deny]   # every CI check, or one step (app: Linux only)
cargo fmt && npm --prefix ui run format   # fix formatting
```

CI runs on Linux, macOS and Windows, one Woodpecker workflow each. The macOS and Windows agents are started by hand while working on the client (the commands at the top of [ci/README.md](ci/README.md), with how they and the shared test server are set up).

License: AGPL-3.0.
