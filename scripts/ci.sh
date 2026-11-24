#!/usr/bin/env sh
# Every CI check, run natively: `scripts/ci.sh` runs them all; `scripts/ci.sh
# <step>` runs one, as the Woodpecker steps do:
#   lint  released migrations untouched, formatting, clippy (warnings are
#         errors), the app's page, the app built
#   test  every test (the end-to-end ones need a Postgres, scripts/dev.sh's or
#         DATABASE_URL's, and the server, built from the pinned core);
#         `test <filter>`: only those whose names match. Built first, then
#         each run given THECA_TEST_TIMEOUT (20m by default) where coreutils'
#         `timeout` is
#   app   the app driven through its window (Linux: tauri-driver, under a
#         virtual screen; the CI image has them)
#   deny  licenses, advisories, sources (Rust and the page's npm packages)
#   service  (not in `all`: it needs Docker and a privileged container, so
#         GitHub's runner and a developer's machine, not Woodpecker) the
#         theca-cli package installed on a systemd machine, the daemon run as
#         `theca@<user>` (tests/service.rs); THECA_TEST_DEB names a package
#         already built, else it is built here (ci/cli-package.sh). Needs the
#         test Postgres, as `test` does.
set -eu
cd "$(dirname "$0")/.."

step=${1:-all}
filter=${2:-}
case "$step" in
    all | lint | test | app | deny | service) ;;
    *)
        echo "usage: scripts/ci.sh [lint|test [<filter>]|app|deny|service]" >&2
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

# `cargo test <args>`, built first (`--no-run`: the build isn't timed), then
# run within THECA_TEST_TIMEOUT, so no test hangs a machine: `timeout` kills
# the whole run past it (what the tests started under tests/common/child.sh
# ends with them). Where `timeout` isn't (macOS), the job's limit stands.
tests() {
    # Each step checked: set -e is off in a function called left of `||`.
    cargo test --no-run "$@" || return
    if ! command -v timeout >/dev/null; then
        cargo test "$@"
        return
    fi
    # `timeout` puts the run in a group of its own, which a terminal's Ctrl-C
    # doesn't reach, and a command started with `&` ignores SIGINT: each
    # signal is passed on, and the wait taken up again after it.
    timeout -k 30 "${THECA_TEST_TIMEOUT:-20m}" cargo test "$@" &
    run=$!
    trap 'kill -INT "$run" 2>/dev/null' INT
    trap 'kill -TERM "$run" 2>/dev/null' TERM
    # Waited for once whatever happened (a run already ended has its status
    # kept for this wait); again while a trapped signal cut the wait short
    # (above 128, the run still there).
    if wait "$run"; then status=0; else status=$?; fi
    while [ "$status" -gt 128 ] && kill -0 "$run" 2>/dev/null; do
        if wait "$run"; then status=0; else status=$?; fi
    done
    trap - INT TERM
    case "$status" in
        124) echo "cargo test $*: stopped after ${THECA_TEST_TIMEOUT:-20m} (THECA_TEST_TIMEOUT)" >&2 ;;
        137) echo "cargo test $*: killed (SIGKILL: THECA_TEST_TIMEOUT's last resort, or another)" >&2 ;;
    esac
    return "$status"
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
    # Windows' service (empty elsewhere: linted for real on the Windows runner).
    cargo clippy -p theca-winservice --all-targets -- -D warnings
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
    # `|| exit`: set -e doesn't stop at a function's failure on the left of
    # `||`; `tests` returns its first failing step's status itself.
    if [ -n "$filter" ]; then
        tests --features crash-points -- "$filter" || exit
        tests -p theca-fileprovider -- "$filter" || exit
    else
        tests --features crash-points || exit
        tests -p theca-fileprovider || exit
        # The updater's own tests in a self-updating build (the embedded key,
        # the commands).
        tests --features self-update --lib update:: || exit
        tests --features self-update --test update with_the_feature || exit
    fi
fi
if [ "$step" = service ]; then
    server
    if [ -z "${THECA_TEST_DEB:-}" ]; then
        version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
        case $(uname -m) in
        x86_64) arch=x86_64 ;;
        aarch64 | arm64) arch=arm64 ;;
        esac
        ci/cli-package.sh "$version" "$arch" target/service-check theca-cli deb
        THECA_TEST_DEB=$PWD/target/service-check/theca-cli.deb
    fi
    export THECA_TEST_DEB
    # Required here: a missing Docker or package fails, not skips.
    THECA_TEST_SERVICE=1 tests --test service || exit
fi
# Only Linux has a WebDriver for WebKit: elsewhere, `all` skips it, and the app
# is checked by hand (the sprint's checklist).
if [ "$step" = app ] || { [ "$step" = all ] && [ "$(uname)" = Linux ]; }; then
    server
    npm --prefix ui run build
    dbus-run-session -- xvfb-run -a cargo test --features gui,custom-protocol --test app
    # The app's own unit tests (the tray's summary).
    cargo test --features gui,custom-protocol,self-update --lib -- gui:: autostart::
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
    # build time, has no fixed release. And, by id, the ALLOWED advisories: a
    # moderate or worse with no fixed release, in build tooling that only ever
    # handles this repository's own files, so nobody supplies the input it
    # fails on; each goes once the audit passes without it (theca/computer#68).
    #   GHSA-fc5q-39xc-vjpf: picomatch <= 4.0.7, a ReDoS through repeated
    #   extglobs (vite, svelte-check, paraglide glob our own paths); no patched
    #   version listed on 2026-11-18.
    ALLOWED="GHSA-fc5q-39xc-vjpf"
    export ALLOWED
    audit=$(npm --prefix ui audit --audit-level=moderate --json || true)
    # Fails closed: an audit that gave no JSON (no network) fails the parse.
    printf '%s' "$audit" | node -e '
        let json = "";
        process.stdin.on("data", (chunk) => (json += chunk)).on("end", () => {
            const allowed = process.env.ALLOWED.split(" ");
            const found = new Set();
            for (const v of Object.values(JSON.parse(json).vulnerabilities)) {
                for (const via of v.via) {
                    if (typeof via !== "object") continue;
                    if (!["moderate", "high", "critical"].includes(via.severity)) continue;
                    if (allowed.some((id) => via.url.endsWith(id))) continue;
                    found.add(`${via.url} (${via.name}, ${via.severity})`);
                }
            }
            if (found.size) {
                console.error("npm audit: " + [...found].join(", "));
                process.exit(1);
            }
        });
    '
    unset ALLOWED
fi
echo "CI checks passed ($step)"
