@../docs/CLAUDE.md

# This repo: `computer`

Theca on computers (desktops, laptops, servers): one crate and binary, `theca` (ADR 0004): the CLI and daemon; the Tauri app behind the `gui` feature. Today the sync client; later the drive too. Built on `theca-client` (from `core`).

- `src/plan.rs` is reconciliation, pure: base, local scan and remote states in, actions out. Every rule of `decisions.md` (Sync) has a unit test there.
- **`core` is pinned to a release tag** (`Cargo.toml`, from its GitHub mirror), and `scripts/ci.sh` builds the test server from those same sources (found through `cargo metadata`). To work against an unreleased `core` during a sprint, add locally, uncommitted: `[patch."https://github.com/theca-cloud/core"]` with `theca-client = { path = "../core/crates/client" }` and `theca-common = { path = "../core/crates/common" }`.
- `src/state.rs` is the only SQL: one SQLite file, the schema rewritten in place until the client's 1.0.
- The scenarios run in parallel in one process, each against a server of its own, on every platform (a port from `common::free_port`, never `bind(0)` alone). `converge` means the server matches the folders, not that the engines' rounds ended: the engine's state (notices, entries) is read with `eventually`.
- `scripts/ci.sh` runs on Windows too (Git Bash, on GitHub's runner): paths from Cargo come as `C:\\…`, Git Bash's `pwd` gives `/d/…` (`pwd -W` for a Windows one), binaries end in `.exe`.
- **A test run alone needs the test server** at `target/debug/theca-server` (a `cargo clean` removes it): `scripts/ci.sh test` builds it, or `SQLX_OFFLINE=true cargo build --manifest-path <core's sources> --target-dir target -p theca-core` (the path as `scripts/ci.sh` finds it), or `THECA_SERVER`. `tests/crash.rs` runs only with `--features crash-points`.
- A new test of a guard is checked failing with the guard removed, before it counts: sprint 11 had three that passed without theirs.
- `cargo mutants` copies the tree to `$TMPDIR` and builds it there (a second `target/`, GBs): delete `$TMPDIR/cargo-mutants-*` and its output once read. A day of test runs grows `target/` past 10 GB: `cargo clean` past ~5 GB.
- `tauri build` (`ci/package.sh`) rewrites `tauri-build`'s line in `Cargo.toml` (`features = []`): harmless, not committed.

## Workflow differences

- **CI in two systems** (ADR 0005): Woodpecker checks Linux on every push; GitHub Actions tests macOS, Windows and Linux arm64 from `dev`, and builds the packages and the release on a tag. Check GitHub with `gh run list -R theca-cloud/computer` (`ci/README.md`), not only `woodpecker-cli`.
- **`dev` is mirrored to GitHub** (public) once Woodpecker's checks pass; feature branches stay on Gitea.
- **`scripts/release.sh`** also checks GitHub's latest `dev` test run; the release itself (packages, GitHub release) happens on GitHub after the tag is pushed. No Gitea release.

Build, run and test commands: see the README.

@README.md
