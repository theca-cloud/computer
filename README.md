# Theca for computers

The Theca app for macOS, Windows and Linux: local folders kept identical to Theca libraries, both ways, or libraries as drives (all shown, a file downloaded when opened). A tray app with a small window, or a CLI and daemon (`theca`) for servers.

Installing it, the app password and the keychain, drives, headless use (commands, exit codes, environment variables, where it keeps its state): **[USING.md](USING.md)**, also on [theca.rs/computer](https://theca.rs/computer).

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

CI runs on Linux on our Woodpecker, and on macOS, Windows and Linux arm64 on GitHub Actions, from `dev` once Linux passed ([ci/README.md](ci/README.md)).

## Releasing

`scripts/release.sh` from `dev` (it refuses while GitHub's latest test run on `dev` is red), then `git push origin prod dev <version>`. Woodpecker mirrors the tag to GitHub, which builds every package, checks each one (`ci/package.sh`: installed or unpacked, `theca --version`), and publishes the release once they all passed. A run that fails: re-run its failed jobs if it was flaky; if it was a real bug, fix it on `dev`, and the next patch release ships it. A tag is never moved: that version stays a tag without a release.

Questions, need help, or running Theca and want to say so? Come to [Discord](https://discord.gg/JBTfacSm4G).

License: AGPL-3.0.
