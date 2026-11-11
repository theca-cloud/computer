#!/usr/bin/env bash
# computer's checks on a developer's Mac, as GitHub's macOS job runs them,
# from a git bundle beside this script (computer.bundle: `git bundle create
# computer.bundle <branch>`, carried over by hand or rc cp), for a quicker
# turn than GitHub's queue: the lint
# and the tests, and with --drive the macOS drive's test too. The results go
# to results/<time>/ beside it.
#
#   ./ci.sh [lint|test|all] [--drive] [<test name filter>]
#   ./ci.sh dmg          the .dmg, as release.yml builds it (ci/package.sh),
#                        into dl/arm64/ for drive-check.sh
#
# Never your keychain: the tests' app passwords stay in memory. --drive
# installs a minimal Theca.app in /Applications (over yours), removes every
# Theca drive on this Mac first (earlier runs' leftovers), switches its test
# drive on by editing fileproviderd's settings and restarting it (killall
# fileproviderd: SeaDrive restarts with it) or, where that edit is refused,
# by your hand in System Settings, and leaves the app there.
set -u
here=$(cd "$(dirname "$0")" && pwd)
repo=$here/computer
bundle=$here/computer.bundle
what=all
drive=false
filter=
while [ $# -gt 0 ]; do
    case $1 in
    lint | test | all | dmg) what=$1 ;;
    --drive) drive=true ;;
    -*) echo "usage: ./ci.sh [lint|test|all|dmg] [--drive] [<test name filter>]" >&2; exit 2 ;;
    *) filter=$1 ;;
    esac
    shift
done
out=$here/results/$(date +%Y%m%d-%H%M%S)
mkdir -p "$out"
summary=$out/summary.txt
say() { echo "$*" | tee -a "$summary"; }

# What it needs.
missing=
need() { command -v "$1" >/dev/null || { say "missing: $1 ($2)"; missing=1; }; }
need git "xcode-select --install"
need cargo "rustup: https://rustup.rs (the toolchain in rust-toolchain.toml comes by itself)"
need node "Node, the version in ui/.nvmrc (brew, mise or nvm)"
need docker "Docker Desktop"
[ -n "$missing" ] && exit 2
docker info >/dev/null 2>&1 || { say "Docker isn't running: start Docker Desktop"; exit 2; }
[ -f "$bundle" ] || { say "no $bundle: git bundle create computer.bundle <branch>, beside this script"; exit 2; }

# The bundle's branch, checked out as it is (nothing of yours kept in the
# clone but target/, node_modules and the Postgres data).
branch=$(git bundle list-heads "$bundle" | awk 'NR == 1 { sub("refs/heads/", "", $2); print $2 }')
[ -d "$repo/.git" ] || git clone -q "$bundle" "$repo"
git -C "$repo" fetch -q "$bundle" "+refs/heads/$branch:refs/remotes/bundle/$branch"
git -C "$repo" checkout -q --detach "bundle/$branch"
git -C "$repo" reset -q --hard "bundle/$branch"
git -C "$repo" clean -qfdx -e target -e ui/node_modules -e .env -e data
cd "$repo" || exit 2
say "commit: $(git log --oneline -1)"

step() {
    local name=$1
    shift
    local log="$out/$name.log"
    echo ">>> $name (log: $log)"
    if "$@" >"$log" 2>&1; then
        say "PASS $name"
    else
        say "FAIL $name (exit $?): $(grep -E 'panicked at|error(\[|:)|FAILED' "$log" | head -5 | tr '\n' ' ')"
        return 1
    fi
}

npm --prefix ui ci >"$out/npm.log" 2>&1 || { say "FAIL npm ci: see npm.log"; exit 1; }
if [ "$what" = dmg ]; then
    step dmg ci/package.sh macos arm64 || exit 1
    mkdir -p "$here/dl/arm64"
    rm -f "$here"/dl/arm64/*.dmg
    cp dist/*.dmg "$here/dl/arm64/" && say "the .dmg: $(ls "$here"/dl/arm64/*.dmg)"
    exit 0
fi
step postgres scripts/dev.sh || exit 1
if [ "$what" != test ]; then
    step lint scripts/ci.sh lint
fi
if [ "$what" != lint ]; then
    export THECA_TEST_KEYCHAIN=mock
    if $drive; then
        export THECA_TEST_DRIVE=1
        export THECA_TEST_MAC_APP=/Applications/Theca.app
        export THECA_TEST_MATERIALIZE=$repo/target/debug/theca-materialize
        export THECA_TEST_LOGS=$out/test-logs
        app() {
            cargo build --bin theca &&
                cargo build -p theca-fileprovider --bins &&
                ci/mac-app.sh app target/debug/theca target/debug/theca-drive /Applications/Theca.app &&
                /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f -R -trusted /Applications/Theca.app &&
                pluginkit -a /Applications/Theca.app/Contents/PlugIns/ThecaDrive.appex &&
                pluginkit -e use -i rs.theca.computer.drive
        }
        osascript -e 'quit app "Theca"' 2>/dev/null
        step "the app with its drive" app || exit 1
        # A clean start: earlier runs' test drives (one left keeps its
        # folder's name, and a new drive of that name is refused).
        /Applications/Theca.app/Contents/MacOS/theca --state "$out/clean.sqlite" remove-drives --all >"$out/clean.log" 2>&1
        echo ">>> If a test drive \"Docs\" shows in System Settings → General → Login Items &"
        echo ">>> Extensions → File Providers → Theca while the tests run, switch it on (2 minutes)."
    fi
    step test scripts/ci.sh test ${filter:+"$filter"}
    grep -E '^test result|^test .* FAILED' "$out/test.log" | sort | uniq -c >>"$summary"
    if $drive && grep -q 'mac_drive.*FAILED\|a_mac_drive_is_the_library ... FAILED' "$out/test.log"; then
        cp "$HOME/Library/Containers/rs.theca.computer.drive/Data/drive.log" "$out/" 2>/dev/null
        log show --last 15m --style compact --predicate 'process == "theca-drive" OR process == "fileproviderd" OR process == "theca"' 2>/dev/null |
            grep -E 'theca|rs\.theca' | tail -5000 >"$out/system.log"
    fi
fi
echo
cat "$summary"
echo
echo "The results: $out"
! grep -q '^FAIL' "$summary"
