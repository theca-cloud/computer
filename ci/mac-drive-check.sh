#!/usr/bin/env bash
# Before a release, on a developer's Mac (not CI: a runner never quarantines
# what it builds): the packaged app downloaded as a browser leaves it
# (quarantined), opened through Gatekeeper, its drive extension loaded, a
# drive added for a test library, switched on by hand, a file opened in it,
# a folder kept and not anymore through Finder's actions (the library needs
# a file at its top and a folder holding a file).
# Each step prints PASS|FAIL <step>: <detail>, into summary.txt too.
#   ci/mac-drive-check.sh <Theca .dmg, or its URL> <test library> [instance]
# A test instance or a test library: the drive is removed at the end. With
# no account in the app yet (a Mac it never ran on), `instance` is logged in
# to once the app opens (`theca login`, approved in the browser).
set -u
dmg=${1:-}
library=${2:-}
instance=${3:-}
if [ -z "$dmg" ] || [ -z "$library" ]; then
    echo "usage: ci/mac-drive-check.sh <Theca .dmg, or its URL> <test library> [instance]" >&2
    exit 2
fi
work=$(mktemp -d)
summary=$PWD/summary.txt
: >"$summary"
app=/Applications/Theca.app
theca=$app/Contents/MacOS/theca

result() { echo "$1 $2: $3" | tee -a "$summary"; }
ask() {
    echo
    echo ">>> $1"
    read -r -p ">>> press Enter when done " _
}

case $dmg in
http*://*)
    curl -sSfL -o "$work/Theca.dmg" "$dmg" || { result FAIL download "$dmg"; exit 1; }
    dmg=$work/Theca.dmg
    ;;
esac
# What Safari sets on a download: Gatekeeper checks it on first open.
xattr -w com.apple.quarantine "0081;$(printf %x "$(date +%s)");Safari;" "$dmg"
result PASS "quarantined like a download" "$(xattr -p com.apple.quarantine "$dmg")"

mnt=$work/mnt
hdiutil attach -nobrowse -readonly -mountpoint "$mnt" "$dmg" >/dev/null ||
    { result FAIL "dmg mounted" "$dmg"; exit 1; }
if [ -e "$app" ]; then
    echo "Theca.app is already in /Applications: it is replaced."
    osascript -e 'quit app "Theca"' 2>/dev/null
    rm -rf "$app"
fi
# Through Finder, as a user drags it: Gatekeeper translocates (runs from a
# temporary copy, its File Provider disabled) an app moved by anything else.
osascript -e "tell application \"Finder\" to duplicate (POSIX file \"$mnt/Theca.app\" as alias) to (POSIX file \"/Applications\" as alias) with replacing" >/dev/null ||
    { result FAIL "installed" "Finder didn't copy it (allow Terminal to control Finder)"; exit 1; }
hdiutil detach "$mnt" >/dev/null
if xattr -p com.apple.quarantine "$app" >/dev/null 2>&1; then
    result PASS "installed, quarantine kept" "$(xattr -p com.apple.quarantine "$app")"
else
    result FAIL "installed, quarantine kept" "no com.apple.quarantine on the app"
fi

open "$app" 2>/dev/null
ask "macOS refuses an unsigned app: System Settings → Privacy & Security → Open Anyway (then open Theca again if it closed)"
if ! pgrep -x theca >/dev/null; then
    result FAIL "opened through Gatekeeper" "no theca process"
elif pgrep -fl theca | grep -q AppTranslocation; then
    result FAIL "opened through Gatekeeper" "translocated (run from a temporary copy): its drive can't work"
else
    result PASS "opened through Gatekeeper" "theca running from /Applications"
fi

if pluginkit -m -i rs.theca.computer.drive 2>/dev/null | grep -q rs.theca.computer.drive; then
    result PASS "extension registered" "$(pluginkit -m -i rs.theca.computer.drive | tr -s ' ')"
else
    result FAIL "extension registered" "pluginkit doesn't list rs.theca.computer.drive"
fi

if [ -z "$("$theca" accounts 2>/dev/null)" ]; then
    if [ -z "$instance" ]; then
        result FAIL "logged in" "no account in the app: give the instance as the third argument"
        exit 1
    fi
    echo ">>> Logging in to $instance: open the page it prints and allow it"
    if "$theca" login "$instance"; then
        result PASS "logged in" "$instance"
    else
        result FAIL "logged in" "theca login $instance"
        exit 1
    fi
fi

if added=$("$theca" add "$library" --drive 2>&1); then
    result PASS "drive added" "$(echo "$added" | head -1)"
else
    result FAIL "drive added" "$added"
    exit 1
fi
storage=$HOME/Library/CloudStorage
before=$(ls -A "$storage" 2>/dev/null)
ask "Switch the drive on: System Settings → General → Login Items & Extensions → File Providers → Theca"
if "$theca" status 2>&1 | grep -q "waiting to be switched on"; then
    result FAIL "switched on" "theca status still says waiting"
else
    result PASS "switched on" "theca status no longer waiting"
fi

# The folder macOS made: the one new in ~/Library/CloudStorage.
folder=
for _ in $(seq 30); do
    new=$(comm -13 <(echo "$before") <(ls -A "$storage" 2>/dev/null))
    [ -n "$new" ] && folder=$storage/$(echo "$new" | head -1) && break
    sleep 1
done
if [ -n "$folder" ] && ls "$folder" >/dev/null 2>&1; then
    result PASS "listed in Finder's folder" "$folder: $(find "$folder" -mindepth 1 -maxdepth 1 -exec basename {} \; | head -5 | tr '\n' ' ')"
else
    result FAIL "listed in Finder's folder" "nothing new in $storage after 30 s"
fi
file=$(find "$folder" -maxdepth 1 -type f 2>/dev/null | head -1)
if [ -n "$file" ] && head -c 64 "$file" >/dev/null 2>&1; then
    result PASS "a file opened (fetched)" "$file"
else
    result FAIL "a file opened (fetched)" "${file:-no file in the library}"
fi

# Finder's actions (no script can click Finder's menu): a folder kept, its
# files downloaded without being read (SF_DATALESS gone), then not kept.
dataless() { [ $(( 0x$(stat -f %Xf "$1") & 0x40000000 )) -ne 0 ]; }
confirm() { read -r -p ">>> $1 [y/n] " answer; [ "$answer" = y ]; }
dir=$(find "$folder" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | head -1)
inside=$(find "$dir" -type f -maxdepth 1 2>/dev/null | head -1)
if [ -z "$inside" ]; then
    result FAIL "Keep Downloaded" "no folder with a file in the library: add one"
elif ! dataless "$inside"; then
    result FAIL "Keep Downloaded" "$inside was downloaded already: use a library whose folder wasn't opened"
else
    ask "In Finder, right-click $dir → Keep Downloaded"
    for _ in $(seq 30); do dataless "$inside" || break; sleep 1; done
    if dataless "$inside"; then
        result FAIL "Keep Downloaded" "$inside still not downloaded after 30 s"
    else
        result PASS "Keep Downloaded" "$inside downloaded, unread"
    fi
    if confirm "Right-click $dir again: is Don't Keep Downloaded offered, and Keep Downloaded not?"; then
        ask "Click Don't Keep Downloaded, then Remove Download"
        for _ in $(seq 30); do dataless "$inside" && break; sleep 1; done
        if dataless "$inside"; then
            result PASS "Don't Keep Downloaded, Remove Download" "$inside dataless again"
        else
            result FAIL "Don't Keep Downloaded, Remove Download" "$inside still downloaded"
        fi
    else
        result FAIL "Don't Keep Downloaded" "the menu didn't offer it"
    fi
fi

if removed=$("$theca" remove "$library" 2>&1); then
    result PASS "drive removed" "$library"
else
    result FAIL "drive removed" "theca remove $library: $removed"
fi
echo
echo "=== summary (paste it back):"
cat "$summary"
