@../docs/CLAUDE.md

# This repo: `computer`

Theca on computers (desktops, laptops, servers): one crate and binary, `theca` (ADR 0004): the CLI and daemon; the Tauri app behind the `gui` feature. Today the sync client; later the drive too. Built on `theca-client` (from `core`).

- `src/plan.rs` is reconciliation, pure: base, local scan and remote states in, actions out. Every rule of `decisions.md` (Sync) has a unit test there.
- **`core` is pinned to a release tag** (`Cargo.toml`, from its GitHub mirror), and `scripts/ci.sh` builds the test server from those same sources (found through `cargo metadata`). To work against an unreleased `core` during a sprint, add locally, uncommitted: `[patch."https://github.com/theca-cloud/core"]` with `theca-client = { path = "../core/crates/client" }` and `theca-common = { path = "../core/crates/common" }`.
- `src/state.rs` is the only SQL: one SQLite file, the schema rewritten in place until the client's 1.0.
- The scenarios run in parallel in one process (a port from `common::free_port`, never `bind(0)` alone) and, on Windows CI, one at a time against a shared server (`THECA_TEST_SERVER_URL`): a new scenario works in both, or returns early on a shared server, saying why.
- Every workflow in `.woodpecker/` has a `platform` label: our Windows and Mac agents serve every repo of the CI server and take only labelled workflows (their filters, `ci/README.md` step 5); `linux` without one landed on the Windows VM (no services there).

Build, run and test commands: see the README.

@README.md
