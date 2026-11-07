# Theca for computers

The Theca app for macOS, Windows and Linux: local folders kept identical to Theca libraries, both ways. A tray app with a small window, or a CLI and daemon (`theca`) for servers.

## Install

Download it from the [releases](https://github.com/theca-cloud/computer/releases), for your system and processor (`arm64`: Apple Silicon, ARM servers and Raspberry Pis; `x86_64`: Intel and AMD):

| | The app | Headless (servers) |
|---|---|---|
| macOS | `theca-<version>-macos-<arch>.dmg`: open it, drag Theca to Applications | `theca-headless-<version>-macos-<arch>.tar.gz` |
| Windows | `theca-<version>-windows-x86_64.exe`: installs for you alone, no admin | `theca-headless-<version>-windows-x86_64.zip` |
| Linux | `theca-<version>-linux-<arch>.deb` (Debian, Ubuntu: `sudo apt install ./theca-….deb`), or the `.AppImage` anywhere else (`chmod +x`, then run it) | `theca-headless-<version>-linux-<arch>.tar.gz` |

The app also answers the CLI (`theca --help`). Headless, unpack the archive and put `theca` on your `PATH` (`sudo install theca-headless-*/theca /usr/local/bin/`), then `theca login`. Linux builds run on Ubuntu 22.04 and Debian 12 or newer.

The app needs a Theca server on the community edition 1.4.4 or later (`core` 1.4.0): it says so when logging in to an older one.

### Opening an unsigned app

The apps aren't signed by Apple or Microsoft yet, so the first time:

- **macOS** says Apple could not verify Theca. Open **System Settings → Privacy & Security**, scroll to Security, click **Open Anyway** next to Theca, and confirm.
- **Windows** SmartScreen says "Windows protected your PC": click **More info**, then **Run anyway**.

### Checking a download

Each release lists the SHA-256 of every file in `SHA256SUMS`: `shasum -a 256 -c SHA256SUMS --ignore-missing` (macOS), `sha256sum -c SHA256SUMS --ignore-missing` (Linux), `Get-FileHash <file>` (Windows, compared by eye).

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
