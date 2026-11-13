#!/usr/bin/env sh
# Every CI check, run natively: `scripts/ci.sh` runs them all; `scripts/ci.sh
# <step>` runs one, as the Woodpecker steps do:
#   lint  released migrations untouched, formatting, clippy (warnings are
#         errors), the app's page, the app built
#   test  every test (the end-to-end ones need a Postgres, scripts/dev.sh's or
#         DATABASE_URL's, and the server, built from the pinned core);
#         `test <filter>`: only those whose names match
#   app   the app driven through its window (Linux: tauri-driver, under a
#         virtual screen; the CI image has them)
#   deny  licenses, advisories, sources (Rust and the page's npm packages)
set -eu
cd "$(dirname "$0")/.."

step=${1:-all}
filter=${2:-}
case "$step" in
    all | lint | test | app | deny) ;;
    *)
        echo "usage: scripts/ci.sh [lint|test [<filter>]|app|deny]" >&2
        exit 2
        ;;
esac

# The test server: built from ../core (offline: its committed .sqlx/, not its
# dev database) unless THECA_SERVER names one, wherever the build goes.
server() {
    [ -n "${THECA_SERVER:-}" ] && return
    # core's sources, wherever Cargo has them: a sibling checkout during a
    # sprint (a path dependency), Cargo's git checkout of the pinned tag after.
    # Windows writes it with escaped backslashes: turned into slashes.
    core=$(cargo metadata --format-version 1 | tr ',' '\n' | sed 's|\\\\|/|g' |
        sed -n 's|^"manifest_path":"\(.*\)/crates/client/Cargo.toml"$|\1|p' | head -1)
    [ -n "$core" ] || { echo "core's sources not found (theca-client's manifest)" >&2; exit 1; }
    # Into this repo's target/ (not into Cargo's git checkout, in ~/.cargo).
    target=${CARGO_TARGET_DIR:-$PWD/target}
    SQLX_OFFLINE=true cargo build --manifest-path "$core/Cargo.toml" --target-dir "$target" -p theca-core
    # Absolute: the tests start it from a directory of their own. A Windows
    # path on Windows (Git Bash's pwd gives /d/...), and its .exe.
    THECA_SERVER="$(cd "$target" && { pwd -W 2>/dev/null || pwd; })/debug/theca-server"
    [ -f "$THECA_SERVER.exe" ] && THECA_SERVER="$THECA_SERVER.exe"
    export THECA_SERVER
}

# The last release's tag (none yet: nothing to guard).
last_release() {
    git fetch -q --tags origin 2>/dev/null || true
    git tag --list | grep -E '^[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -1
}

# What a released client state depends on is never edited nor deleted
# (wiki: Sync client, The client state): its migration steps, and the release states the
# upgrade test reads back. Against the working tree, so an uncommitted edit
# fails too; renames count as a deletion.
released_guard() {
    last=$(last_release)
    [ -n "$last" ] || return 0
    edited=$(git diff --name-only --no-renames --diff-filter=MD "$last" -- \
        'src/state/migrations/v*.rs' tests/upgrade)
    if [ -n "$edited" ]; then
        echo "released migration steps or states edited or deleted since $last: add a new step instead" >&2
        echo "$edited" >&2
        exit 1
    fi
}

# A release never trusts a test key: ci/package.sh, run as on a tag with the
# test key's variable set, refuses it before building anything.
test_key_guard() {
    version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
    status=0
    out=$(GITHUB_REF_TYPE=tag GITHUB_REF_NAME="$version" THECA_TEST_UPDATER_PUBKEY=test \
        ci/package.sh linux x86_64 2>&1) || status=$?
    if [ "$status" != 1 ] || ! echo "$out" | grep -q THECA_TEST_UPDATER_PUBKEY; then
        echo "ci/package.sh on a tag didn't refuse the test key (exit $status):" >&2
        echo "$out" >&2
        exit 1
    fi
}

if [ "$step" = all ] || [ "$step" = lint ]; then
    released_guard
    test_key_guard
    cargo fmt --all --check
    cargo clippy --features crash-points --all-targets -- -D warnings
    # macOS's File Provider extension and calls (a stub elsewhere: its
    # protocol is still checked).
    cargo clippy -p theca-fileprovider --all-targets -- -D warnings
    # The app: its page (formatting, lint, types), built, then embedded in the
    # `gui` build, linted too (needs WebKitGTK on Linux).
    [ -d ui/node_modules ] || npm --prefix ui ci
    npm --prefix ui run lint
    npm --prefix ui run check
    npm --prefix ui run build
    # With self-update, as every app package but the .deb.
    cargo clippy --features gui,custom-protocol,self-update --all-targets -- -D warnings
fi
if [ "$step" = all ] || [ "$step" = test ]; then
    # The end-to-end tests run the real server: built from the pinned core unless
    # THECA_SERVER names one.
    # The engines' debug lines, each device's named: printed for a failed test
    # only (captured), so a failure seen once on CI says what happened.
    export RUST_LOG="${RUST_LOG:-theca=debug}"
    server
    # The crash tests' named points (tests/crash.rs) built in: inert unless
    # THECA_CRASH_AT names one.
    if [ -n "$filter" ]; then
        cargo test --features crash-points -- "$filter"
        cargo test -p theca-fileprovider -- "$filter"
    else
        cargo test --features crash-points
        cargo test -p theca-fileprovider
        # The updater's own tests in a self-updating build (the embedded key,
        # the commands).
        cargo test --features self-update --lib update::
        cargo test --features self-update --test update with_the_feature
    fi
fi
# Only Linux has a WebDriver for WebKit: elsewhere, `all` skips it, and the app
# is checked by hand (the sprint's checklist).
if [ "$step" = app ] || { [ "$step" = all ] && [ "$(uname)" = Linux ]; }; then
    server
    npm --prefix ui run build
    dbus-run-session -- xvfb-run -a cargo test --features gui,custom-protocol --test app
    # The app's own unit tests (the tray's summary).
    cargo test --features gui,custom-protocol,self-update --lib gui::
    # The app with self-update, built with a key made for this run (never
    # stored): its tests sign releases with it, as release.yml signs.
    key_dir=$(mktemp -d)
    # Removed however the tests end.
    trap 'rm -rf "$key_dir"' EXIT
    ui/node_modules/.bin/tauri signer generate --ci -p "" -w "$key_dir/updater.key" >/dev/null
    THECA_TEST_UPDATER_PUBKEY=$(cat "$key_dir/updater.key.pub") \
        THECA_TEST_UPDATER_KEY="$key_dir/updater.key" \
        dbus-run-session -- xvfb-run -a \
        cargo test --features gui,custom-protocol,self-update --test app self_update
fi
if [ "$step" = all ] || [ "$step" = deny ]; then
    cargo deny --all-features check
    # Every dependency: the page bundles its devDependencies (--omit=dev would
    # audit nothing). Lows let through: deepmerge's, in vite-plugin-svelte at
    # build time, has no fixed release.
    npm --prefix ui audit --audit-level=moderate
fi
echo "CI checks passed ($step)"
