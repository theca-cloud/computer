@../docs/CLAUDE.md

# This repo: `computer`

Theca on computers (desktops, laptops, servers): one crate and binary, `theca` (ADR 0004): the CLI and daemon; the Tauri app behind the `gui` feature. Today the sync client; later the drive too. Built on `theca-client` (from `core`).

- `src/plan.rs` is reconciliation, pure: base, local scan and remote states in, actions out. Every rule of `decisions.md` (Sync) has a unit test there.
- **`core` is pinned to a release tag** (`Cargo.toml`, from its GitHub mirror), and `scripts/ci.sh` builds the test server from those same sources (found through `cargo metadata`). To work against an unreleased `core` during a sprint, put in an untracked `.cargo/config.toml` (not `Cargo.toml`, which then stays committable): `[patch."https://github.com/theca-cloud/core"]` with `theca-client = { path = "../core/crates/client" }` and `theca-common = { path = "../core/crates/common" }`. `Cargo.lock` then records the patch: commit it only once the release is pinned. The test server comes from those sources too.
- **`ops::MIN_SERVER`** is the oldest server (its `core`, and the CE shipping it) the client works with: raise it when the client starts using an API a server gained, never with each `core` release; `USING.md` says the same.
- `src/state/` is the only SQL: one SQLite file, forward-only since 1.0. A schema change is `SCHEMA` and a new step in `src/state/migrations/` (never an edited one: `scripts/ci.sh` refuses it); `tests/upgrade.rs` reads every release's state back and compares the schemas.
- App passwords live in the OS keychain (`keychain.rs`). Tests never touch a developer's: `common::keychain()` takes the in-memory store unless `THECA_TEST_KEYCHAIN=os` (GitHub's macOS and Windows runs); a test starting a child `theca` uses `device_without_keychain` (on macOS, another binary reading the entry prompts and hangs).
- The scenarios run in parallel in one process, each against a server of its own, on every platform (a port from `common::free_port`, never `bind(0)` alone). `converge` means the server matches the folders, not that the engines' rounds ended: the engine's state (notices, entries) is read with `eventually`.
- `scripts/ci.sh` runs on Windows too (Git Bash, on GitHub's runner): paths from Cargo come as `C:\\…`, Git Bash's `pwd` gives `/d/…` (`pwd -W` for a Windows one), binaries end in `.exe`.
- **A test run alone needs the test server** at `target/debug/theca-server` (a `cargo clean` removes it): `scripts/ci.sh test` builds it, or `SQLX_OFFLINE=true cargo build --manifest-path <core's sources> --target-dir target -p theca-core` (the path as `scripts/ci.sh` finds it), or `THECA_SERVER`. `tests/crash.rs` runs only with `--features crash-points`.
- A new test of a guard is checked failing with the guard removed, before it counts: sprint 11 had three that passed without theirs.
- A quick `cargo check --all-targets` skips `gui.rs`: check `--features gui,custom-protocol` too (sprint 13 broke the app build that way). A change to what the GUI sends the page (`PairView`, `Update`) is a change to `ui/src/lib/app.svelte.ts` and its components: grep the page for the old field or string.
- A flag read from the environment (`env = "…"`) takes `value_parser = clap::builder::FalseyValueParser::new()`: the default bool parser refuses `X=1`. Run a new flag or variable once from a shell, as `USING.md` documents it.
- `cargo mutants` copies the tree to `$TMPDIR` and builds it there (a second `target/`, GBs): delete its `cargo-mutants-…` copy and `mutants.out/` once read. With the local patch above, the copy can't find `../core`: run it `--in-place` (it restores the file; check `git status`). A day of test runs grows `target/` past 10 GB: `cargo clean` past ~5 GB.
- Tests mounting a disk image (macOS, `hdiutil`) hold it in a guard that detaches on drop (`Mounted`, tests/safety.rs): a failing test left images attached.
- `tauri build` (`ci/package.sh`) rewrites `tauri-build`'s line in `Cargo.toml` (`features = []`): harmless, not committed.

## Workflow differences

- **CI in two systems** (ADR 0005): Woodpecker checks Linux on every push; GitHub Actions tests macOS, Windows and Linux arm64 from `dev`, and builds the packages and the release on a tag. Check GitHub with `gh run list -R theca-cloud/computer` (`ci/README.md`), not only `woodpecker-cli`.
- **`dev` is mirrored to GitHub** (public) once Woodpecker's checks pass; feature branches stay on Gitea.
- **`scripts/release.sh`** also checks GitHub's latest `dev` test run; the release itself (packages, GitHub release) happens on GitHub after the tag is pushed. No Gitea release.

Build, run and test commands: see the README.

@README.md
