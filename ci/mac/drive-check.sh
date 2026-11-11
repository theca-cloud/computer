#!/usr/bin/env bash
# Before a release, on a developer's Mac (not CI: a runner never quarantines
# what it builds): the packaged app as a user gets it, its drive end to end.
# It does all it can itself, and asks only what a person must do:
#   - a fresh local server (./server.sh, CE's compose), set up through its
#     API: an admin, a library "DriveTest" with a file at its top and a
#     folder "Keep me" holding one (WebDAV), an app password for the CLI;
#   - the app installed from the .dmg (--as-downloaded: quarantined as a
#     browser leaves a download, as a user gets it);
#   - you: the drive's switch in System Settings (if macOS doesn't remember
#     it), Finder's Always Keep on This Mac and Free Up Space, and whether
#     its menu offers them; with --as-downloaded also the drag into
#     Applications and Gatekeeper's Open Anyway;
#   - checked: not translocated, the extension registered, logged in (no
#     browser: the app password), the drive added, its folder listed, a
#     file fetched when read, a folder kept then not, the drive removed.
# Stops at the first failure the rest depends on (the summary says where).
#   ./drive-check.sh [--as-downloaded] <Theca .dmg, or its URL>
# Run from its folder (ci/mac, or a copy of it): server.sh and
# docker-compose.yml beside it. Each step prints PASS|FAIL <step>: <detail>,
# into summary.txt too.
set -u
here=$(cd "$(dirname "$0")" && pwd)
# --as-downloaded: as a user gets it (quarantined, dragged in, Gatekeeper's
# Open Anyway), once before a release; else the app copied in, its
# quarantine removed: no clicks for those (a quicker turn).
downloaded=false
[ "${1:-}" = --as-downloaded ] && { downloaded=true; shift; }
dmg=${1:-}
if [ -z "$dmg" ]; then
    echo "usage: ./drive-check.sh [--as-downloaded] <Theca .dmg, or its URL>" >&2
    exit 2
fi
instance=http://localhost:${THECA_PORT:-8080}
library=DriveTest
email=check@theca.test
work=$(mktemp -d)
summary=$here/summary.txt
: >"$summary"
app=/Applications/Theca.app
theca=$app/Contents/MacOS/theca
data="$HOME/Library/Application Support/rs.theca.theca"
aside=

result() { echo "$1 $2: $3" | tee -a "$summary"; }
ask() {
    echo
    echo ">>> $1"
    read -r -p ">>> press Enter when done " _
}
confirm() { read -r -p ">>> $1 [y/n] " answer; [ "$answer" = y ]; }
json() { python3 -c "import json,sys; print(json.load(sys.stdin)$1)"; }
# The previous state back, if moved aside for this check.
restore() {
    [ -n "$aside" ] || return 0
    osascript -e 'quit app "Theca"' 2>/dev/null
    sleep 2
    # This check's state, if the app made one, out of the way first.
    if [ -e "$data" ] && ! mv "$data" "$work/state"; then
        echo "!!! Your previous Theca state is still at $aside: move it back to $data by hand."
        return 1
    fi
    if mv "$aside" "$data"; then
        aside=
        echo "Your previous Theca state is back (this check's: $work/state)."
    else
        echo "!!! Your previous Theca state is still at $aside: move it back to $data by hand."
    fi
}
# Interrupted (Ctrl-C at a question): the state goes back all the same.
trap 'restore; exit 130' INT TERM
# This check's logs, kept before the state goes back: the app's, the
# extension's, the system's File Provider lines.
keep_logs() {
    local logs=$here/results/check-$(date +%Y%m%d-%H%M%S)
    mkdir -p "$logs"
    cp -R "$data/logs" "$logs/app" 2>/dev/null
    cp "$HOME/Library/Containers/rs.theca.computer.drive/Data/drive.log" "$logs/" 2>/dev/null
    log show --last 30m --style compact \
        --predicate 'process == "theca-drive" OR process == "fileproviderd" OR process == "theca"' \
        2>/dev/null | grep -i -E 'theca|DriveTest' | tail -2000 >"$logs/system.log"
    cp "$summary" "$logs/"
    echo "The logs: $logs"
}
finish() {
    keep_logs
    restore
    echo
    echo "The server stays up for a look ($instance, $email / $password);"
    echo "./server.sh down removes it, and its data."
    echo
    echo "=== summary (paste it back):"
    cat "$summary"
}
# A failure the next steps depend on: they would only fail or mislead.
stop() {
    result FAIL "$1" "$2"
    echo "STOPPED at: $1" | tee -a "$summary"
    finish
    exit 1
}
password=$(openssl rand -hex 12)

# What it needs here.
for tool in docker python3 curl openssl sqlite3; do
    command -v "$tool" >/dev/null || stop "the tools here" "no $tool (Docker Desktop; xcode-select --install)"
done
docker info >/dev/null 2>&1 || stop "the tools here" "Docker isn't running: start Docker Desktop"

# A fresh server, set up through its API.
"$here/server.sh" down >/dev/null 2>&1
"$here/server.sh" >"$work/server.log" 2>&1 || stop "the server started" "$(tail -3 "$work/server.log")"
token=$(grep -o 'setup?token=[0-9a-f]*' "$work/server.log" | tail -1 | cut -d= -f2)
[ -n "$token" ] || stop "the server started" "no setup token in its log (docker compose logs theca)"
jar=$work/cookies
api() {
    local method=$1 path=$2 body=${3:-}
    curl -sSf -b "$jar" -c "$jar" -X "$method" "$instance/api/v1$path" \
        -H 'content-type: application/json' ${body:+-d "$body"}
}
me=$(curl -sSf -c "$jar" -X POST "$instance/api/v1/setup" \
    -H 'content-type: application/json' -H "x-theca-setup-token: $token" \
    -d "{\"admin_email\":\"$email\",\"admin_password\":\"$password\",\"organization\":\"Check\",\"storages\":[{\"name\":\"main\",\"implementation\":\"filesystem\",\"path\":\"/data/files\"}]}") ||
    stop "the server set up" "POST /api/v1/setup refused"
organization=$(echo "$me" | json "['organizations'][0]['organization']")
id=$(api POST /libraries "{\"organization\":\"$organization\",\"name\":\"$library\"}" | json "['id']") ||
    stop "the test library made" "POST /libraries refused"
dav=$(api POST /auth/app-passwords "{\"name\":\"check: WebDAV\",\"library\":\"$id\"}" | json "['password']") ||
    stop "the test library made" "no app password for WebDAV"
cli=$(api POST /auth/app-passwords "{\"name\":\"check: the app\"}" | json "['password']") ||
    stop "the test library made" "no app password for the app"
put() { curl -sSf -u "$email:$dav" -T - "$instance/dav/$1"; }
echo "at the top" | put top.txt &&
    curl -sSf -u "$email:$dav" -X MKCOL "$instance/dav/Keep%20me" &&
    echo "in a folder" | put "Keep%20me/inside.txt" ||
    stop "the test library made" "WebDAV refused the files"
result PASS "the server set up" "$instance: $library (top.txt, Keep me/inside.txt)"

# Theca's state as it is here: a version from another build may not open,
# and its accounts would mix with this check's. Moved aside, put back after.
if [ -e "$data/state.sqlite" ]; then
    osascript -e 'quit app "Theca"' 2>/dev/null
    sleep 2
    aside="$data.before-check-$(date +%Y%m%d-%H%M%S)"
    mv "$data" "$aside" || stop "a clean state" "couldn't move $data aside"
    result PASS "a clean state" "the previous one aside, back at the end"
fi

case $dmg in
http*://*)
    curl -sSfL -o "$work/Theca.dmg" "$dmg" || stop download "$dmg"
    dmg=$work/Theca.dmg
    ;;
esac
if [ -e "$app" ]; then
    osascript -e 'quit app "Theca"' 2>/dev/null
    sleep 2
    rm -rf "$app" || stop "the old app removed" "rm -rf $app"
fi
if $downloaded; then
    # What Safari sets on a download: Gatekeeper checks it on first open.
    xattr -w com.apple.quarantine "0081;$(printf %x "$(date +%s)");Safari;" "$dmg"
    result PASS "quarantined like a download" "$(xattr -p com.apple.quarantine "$dmg")"
    # By hand, as a user does: Gatekeeper translocates (runs from a
    # temporary copy, its File Provider disabled) a quarantined app moved by
    # anything but a drag in Finder (a script's copy, even through Finder).
    open "$dmg"
    ask "In the window that opened, drag Theca into Applications, then eject the Theca disk"
    [ -d "$app" ] || stop "installed" "no $app"
    if xattr -p com.apple.quarantine "$app" >/dev/null 2>&1; then
        result PASS "installed, quarantine kept" "$(xattr -p com.apple.quarantine "$app")"
    else
        result FAIL "installed, quarantine kept" "no com.apple.quarantine on the app"
    fi
    open "$app" 2>/dev/null
    ask "macOS refuses an unsigned app: System Settings → Privacy & Security → Open Anyway, then open Theca again if it closed"
else
    mnt=$work/mnt
    hdiutil attach -nobrowse -readonly -mountpoint "$mnt" "$dmg" >/dev/null || stop "installed" "the .dmg didn't mount"
    ditto "$mnt/Theca.app" "$app"
    hdiutil detach "$mnt" >/dev/null
    # Unquarantined, nothing translocates it (and no Gatekeeper prompt).
    xattr -dr com.apple.quarantine "$app"
    result PASS "installed" "copied in, its quarantine removed (--as-downloaded: as a user gets it)"
    open "$app" 2>/dev/null
    for _ in $(seq 20); do pgrep -x theca >/dev/null && break; sleep 1; done
fi
pgrep -x theca >/dev/null || stop "opened" "no theca process"
pgrep -fl theca | grep -q AppTranslocation &&
    stop "opened" "translocated (run from a temporary copy, its drive off): was it dragged in with Finder?"
result PASS "opened" "theca running from /Applications"

pluginkit -m -i rs.theca.computer.drive 2>/dev/null | grep -q rs.theca.computer.drive ||
    stop "extension registered" "pluginkit doesn't list rs.theca.computer.drive"
result PASS "extension registered" "$(pluginkit -m -i rs.theca.computer.drive | tr -s ' ')"

# A clean start: earlier checks' drives (one left keeps its folder's name,
# and a new drive of that name is refused).
if [ -n "$(ls -A "$HOME/Library/CloudStorage" 2>/dev/null | grep '^Theca-')" ]; then
    echo "Theca drives are on this Mac already: $(ls "$HOME/Library/CloudStorage" | grep '^Theca-' | tr '\n' ' ')"
    confirm "Remove them all (what isn't uploaded is kept by macOS)?" ||
        stop "a clean start" "Theca drives left on this Mac"
    cleaned=$("$theca" --state "$work/clean.sqlite" remove-drives --all 2>&1) ||
        stop "a clean start" "$cleaned"
    result PASS "a clean start" "$(echo "$cleaned" | tr '\n' ' ')"
fi

said=$(THECA_TOKEN=$cli "$theca" login "$instance" 2>&1) || stop "logged in" "$said"
result PASS "logged in" "$("$theca" accounts | head -1)"

# Before the add: macOS makes the drive's folder then, switched on or not.
storage=$HOME/Library/CloudStorage
before=$(ls -A "$storage" 2>/dev/null)
added=$("$theca" add "$library" --drive 2>&1) || stop "drive added" "$added"
result PASS "drive added" "$(echo "$added" | head -1)"

# macOS may remember Theca's drives switched on: asked only if it says
# otherwise (`theca status` says "switched on" once it is).
on() { "$theca" status 2>&1 | grep -q "a drive, switched on"; }
for _ in $(seq 10); do on && break; sleep 1; done
if on; then
    result PASS "switched on" "already: macOS remembered it"
else
    ask "Switch the drive on: System Settings → General → Login Items & Extensions → File Providers → Theca"
    for _ in $(seq 10); do on && break; sleep 1; done
    on || stop "switched on" "$("$theca" status 2>&1 | grep 'a drive' | head -1)"
    result PASS "switched on" "by hand"
fi

# The folder macOS made: the one new in ~/Library/CloudStorage, listed once
# the app serves the drive (the app notices an account the CLI logged in
# within 30 s).
folder=
for _ in $(seq 90); do
    new=$(comm -13 <(echo "$before") <(ls -A "$storage" 2>/dev/null))
    [ -n "$new" ] && folder=$storage/$(echo "$new" | head -1)
    [ -n "$folder" ] && [ -e "$folder/top.txt" ] && break
    sleep 1
done
[ -n "$folder" ] && [ -e "$folder/top.txt" ] ||
    stop "listed in Finder's folder" "no top.txt in ${folder:-a new folder of $storage} after 90 s"
# A shell's process may not download a drive's files (dataless ones): a
# plain ls or cat is refused ("Operation not permitted") until something
# else downloaded them. Read as an app does, downloads on for this process
# (as tests/mac_drive.rs's theca-materialize does).
fp() {
    python3 - "$@" <<'PY'
import ctypes, os, sys
# sys/resource.h: IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, IOPOL_SCOPE_PROCESS, IOPOL_MATERIALIZE_DATALESS_FILES_ON
ctypes.CDLL(None).setiopolicy_np(3, 0, 2)
what, path = sys.argv[1], sys.argv[2]
try:
    if what == "ls":
        print(" ".join(sorted(os.listdir(path))))
    else:
        print(open(path).read(), end="")
except OSError as e:
    print(e)
    sys.exit(1)
PY
}
# Tried a while: refused the same way while the app isn't serving the drive
# yet, or until macOS's question about Terminal is answered.
listed=false
for n in $(seq 60); do
    listing=$(fp ls "$folder" 2>&1) && listed=true && break
    [ "$n" = 3 ] && echo ">>> Waiting for the drive's listing ($listing). If macOS asks whether Terminal may access files managed by Theca, allow it."
    sleep 1
done
$listed || stop "listed in Finder's folder" "after 60 s: $listing"
result PASS "listed in Finder's folder" "$folder: $listing"

# A placeholder: there, its content not downloaded (SF_DATALESS).
dataless() { [ -e "$1" ] && [ $(( 0x$(stat -f %Xf "$1") & 0x40000000 )) -ne 0 ]; }
# Read as any program reads it: fetched then (tried a while: the app may
# still be starting to serve the drive).
read_back=
for _ in $(seq 30); do
    read_back=$(fp cat "$folder/top.txt" 2>&1) && [ "$read_back" = "at the top" ] && break
    sleep 1
done
if [ "$read_back" = "at the top" ]; then
    result PASS "a file fetched when read" "top.txt"
else
    result FAIL "a file fetched when read" "top.txt: $read_back"
fi

# Finder's actions (no script can click Finder's menu): "Keep me" kept, its
# file downloaded without being read (SF_DATALESS gone), the app's state
# saying so; then not kept, and its download removed.
keep_label="Always Keep on This Mac"
free_label="Free Up Space"
inside="$folder/Keep me/inside.txt"
# The app's record (its state, read as it is written): 1 kept, 0 not.
kept() { sqlite3 "$data/state.sqlite" "SELECT kept FROM entry WHERE name = 'Keep me'" 2>/dev/null; }
# Our item in the menu (macOS adds its own, Download Now, Remove Download).
offered() {
    confirm "Right-click \"Keep me\" in $folder (not the drive itself): does its menu show $1?"
}
if [ ! -e "$inside" ]; then
    result FAIL "$keep_label" "no Keep me/inside.txt in the drive"
elif ! dataless "$inside"; then
    result FAIL "$keep_label" "Keep me/inside.txt downloaded before anything asked for it"
elif ! offered "$keep_label"; then
    result FAIL "$keep_label" "not in the menu"
else
    ask "Click $keep_label"
    for _ in $(seq 60); do [ "$(kept)" = 1 ] && ! dataless "$inside" && break; sleep 1; done
    if [ "$(kept)" != 1 ]; then
        result FAIL "$keep_label" "the app didn't record \"Keep me\" as kept"
    elif dataless "$inside"; then
        result FAIL "$keep_label" "kept, but Keep me/inside.txt still not downloaded after 60 s"
    else
        result PASS "$keep_label" "recorded, Keep me/inside.txt downloaded unread"
    fi
    # Only after a keep recorded: freeing nothing proves nothing.
    if [ "$(kept)" = 1 ]; then
        if ! offered "$free_label"; then
            result FAIL "$free_label" "not in the menu of a kept folder"
        else
            ask "Click $free_label"
            for _ in $(seq 30); do [ "$(kept)" = 0 ] && dataless "$inside" && break; sleep 1; done
            if [ "$(kept)" != 0 ]; then
                result FAIL "$free_label" "the app still records \"Keep me\" as kept"
            elif ! dataless "$inside"; then
                result FAIL "$free_label" "not kept, but Keep me/inside.txt still downloaded"
            else
                result PASS "$free_label" "not kept, Keep me/inside.txt a placeholder again"
            fi
        fi
    fi
fi

if removed=$("$theca" remove "$library" 2>&1); then
    result PASS "drive removed" "$library"
else
    result FAIL "drive removed" "$removed"
fi
finish
