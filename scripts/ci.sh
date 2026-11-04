#!/usr/bin/env sh
# Every CI check, run natively: `scripts/ci.sh` runs them all; `scripts/ci.sh
# <step>` runs one, as the Woodpecker steps do:
#   lint  formatting, clippy (warnings are errors), the app's page, the app built
#   test  every test (the end-to-end ones need scripts/dev.sh's Postgres and
#         the server, built from ../core; or, with THECA_TEST_SERVER_URL and
#         its admin's THECA_TEST_ADMIN_EMAIL and _PASSWORD, a shared server)
#   app   the app driven through its window (Linux: tauri-driver, under a
#         virtual screen; the CI image has them)
#   deny  licenses, advisories, sources (Rust and the page's npm packages)
set -eu
cd "$(dirname "$0")/.."

step=${1:-all}
case "$step" in
    all | lint | test | app | deny) ;;
    *)
        echo "usage: scripts/ci.sh [lint|test|app|deny]" >&2
        exit 2
        ;;
esac

# The test server: built from ../core (offline: its committed .sqlx/, not its
# dev database) unless THECA_SERVER names one, wherever the build goes.
server() {
    [ -n "${THECA_SERVER:-}" ] && return
    # core's sources, wherever Cargo has them: a sibling checkout during a
    # sprint (a path dependency), Cargo's git checkout of the pinned tag after.
    core=$(cargo metadata --format-version 1 | tr ',' '\n' |
        sed -n 's|^"manifest_path":"\(.*\)/crates/client/Cargo.toml"$|\1|p' | head -1)
    [ -n "$core" ] || { echo "core's sources not found (theca-client's manifest)" >&2; exit 1; }
    # Into this repo's target/ (not into Cargo's git checkout, in ~/.cargo).
    target=${CARGO_TARGET_DIR:-$PWD/target}
    SQLX_OFFLINE=true cargo build --manifest-path "$core/Cargo.toml" --target-dir "$target" -p theca-core
    # Absolute: the tests start it from a directory of their own.
    THECA_SERVER="$(cd "$target" && pwd)/debug/theca-server"
    export THECA_SERVER
}

if [ "$step" = all ] || [ "$step" = lint ]; then
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    # The app: its page (formatting, lint, types), built, then embedded in the
    # `gui` build, linted too (needs WebKitGTK on Linux).
    [ -d ui/node_modules ] || npm --prefix ui ci
    npm --prefix ui run lint
    npm --prefix ui run check
    npm --prefix ui run build
    cargo clippy --features gui,custom-protocol --all-targets -- -D warnings
fi
if [ "$step" = all ] || [ "$step" = test ]; then
    # The end-to-end tests run the real server: built from ../core unless
    # THECA_SERVER names one.
    # The engines' debug lines, each device's named: printed for a failed test
    # only (captured), so a failure seen once on CI says what happened.
    export RUST_LOG="${RUST_LOG:-theca=debug}"
    if [ -n "${THECA_TEST_SERVER_URL:-}" ]; then
        # A shared server (the Windows step's, running on Linux): it must run
        # the core this repo pins, and its tests run one at a time (each
        # clears the organizations the last one left).
        pinned=$(sed -n '/^name = "theca-client"$/{n;s/^version = "\(.*\)"$/\1/p;}' Cargo.lock)
        served=$(curl -fsS "$THECA_TEST_SERVER_URL/api/version" | sed -n 's/.*"server":"\([^"]*\)".*/\1/p')
        [ "$served" = "$pinned" ] ||
            { echo "the test server runs core $served, this repo pins $pinned: update it" >&2; exit 1; }
        cargo test -- --test-threads=1
    else
        server
        cargo test
    fi
fi
# Only Linux has a WebDriver for WebKit: elsewhere, `all` skips it, and the app
# is checked by hand (the sprint's checklist).
if [ "$step" = app ] || { [ "$step" = all ] && [ "$(uname)" = Linux ]; }; then
    server
    npm --prefix ui run build
    dbus-run-session -- xvfb-run -a cargo test --features gui,custom-protocol --test app
fi
if [ "$step" = all ] || [ "$step" = deny ]; then
    cargo deny --all-features check
    # Every dependency: the page bundles its devDependencies (--omit=dev would
    # audit nothing). Lows let through: deepmerge's, in vite-plugin-svelte at
    # build time, has no fixed release.
    npm --prefix ui audit --audit-level=moderate
fi
echo "CI checks passed ($step)"
