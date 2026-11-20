#!/usr/bin/env bash
# The installed app as a user sees it, in pictures (theca/computer#96): this
# run's package for the OS, installed as a user would, logged in to the public
# demo, then three screenshots into shots/:
#   app.png      the app's window, its libraries up to date
#   folders.png  the file manager on the synced folders
#   drive.png    the file manager on a library as a drive
# Used by .github/workflows/release.yml's `screenshots` job, after the package
# job, on every run: the agent looks at them at /release (CLAUDE.md), nothing
# here compares pictures. A failure (the demo down, a runner's quirk) holds
# nothing back: no job needs this one.
#   ci/screenshots.sh <macos|windows|linux> <arm64|x86_64>
# The packages are read from dist/ (the package job's artifact).
set -euo pipefail
cd "$(dirname "$0")/.."

os=${1:-}
arch=${2:-}
case $os in macos | windows | linux) ;; *)
    echo "usage: ci/screenshots.sh <macos|windows|linux> <arm64|x86_64>" >&2
    exit 2
    ;;
esac
# A path as Windows programs read it (a Git Bash one reads as an option).
export MSYS2_ARG_CONV_EXCL='*'

demo=https://demo.theca.rs
# The demo account theca.rs shows everyone: public, nothing to keep secret.
demo_email=demo@theca.rs
demo_password=thecademo
jar=$(mktemp)
# curl is a native Windows program there, and MSYS2_ARG_CONV_EXCL leaves
# /tmp/… as it is: it can't write its cookie, and the app password's request
# has no session (401). Tried on windows-2025.
if [ "$os" = windows ]; then jar=$(cygpath -m "$jar"); fi
app_password_id=
theca=
mkdir -p shots

package() {
    local f
    for f in dist/theca-[0-9]*-"$os-$arch.$1"; do
        if [ -f "$f" ]; then
            echo "$f"
            return
        fi
    done
    echo "no dist/theca-<version>-$os-$arch.$1" >&2
    return 1
}

# The app helper's calls, for Windows (the screen, a picture of it).
win() {
    powershell -NoProfile -File "$(cygpath -w ci/screenshots.ps1)" "$@"
}

# Installed as a user installs it: the NSIS installer silently, the .dmg
# copied into Applications (and registered, which a Mac's Finder does the
# first time: the drive's extension isn't known to macOS before, and `add
# --drive` fails with -2001; test.yml registers the tests' app the same
# way), the .deb by apt (with a desktop first: Xvfb, Xfce for the panel's
# tray and Thunar; Xfce is not what most users run, but a desktop).
install_app() {
    local pkg mnt
    case $os in
    windows)
        pkg=$(package exe)
        powershell -NoProfile -Command "Start-Process -Wait -FilePath '$(cygpath -w "$pkg")' -ArgumentList '/S'"
        theca=$(find "$(cygpath "$LOCALAPPDATA")" -maxdepth 3 -ipath '*theca*/theca.exe' | head -1)
        [ -n "$theca" ] || { echo "the installer put no theca.exe under %LOCALAPPDATA%" >&2; exit 1; }
        win screen
        ;;
    macos)
        pkg=$(package dmg)
        mnt=$(mktemp -d)
        hdiutil attach -nobrowse -readonly -mountpoint "$mnt" "$pkg" >/dev/null
        cp -R "$mnt/Theca.app" /Applications/
        hdiutil detach "$mnt" >/dev/null
        /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f -R -trusted /Applications/Theca.app
        pluginkit -a /Applications/Theca.app/Contents/PlugIns/ThecaDrive.appex
        pluginkit -e use -i rs.theca.computer.drive
        theca=/Applications/Theca.app/Contents/MacOS/theca
        ;;
    linux)
        pkg=$(package deb)
        sudo apt-get update -q
        sudo DEBIAN_FRONTEND=noninteractive apt-get install -yq --no-install-recommends \
            xvfb xfce4 xfce4-panel xfce4-terminal thunar dbus-x11 imagemagick x11-utils \
            xfce4-statusnotifier-plugin adwaita-icon-theme fonts-dejavu
        sudo DEBIAN_FRONTEND=noninteractive apt-get install -yq "./$pkg"
        theca=theca
        Xvfb :99 -screen 0 1920x1080x24 >/dev/null 2>&1 &
        sleep 2
        export DISPLAY=:99
        eval "$(dbus-launch --sh-syntax)"
        nohup startxfce4 >/dev/null 2>&1 &
        sleep 15
        ;;
    esac
    "$theca" --version
}

# The app password of this run: made on the demo, masked in the log, revoked on
# the way out whatever happens.
revoke() {
    [ -n "$app_password_id" ] || return 0
    curl -fsS -b "$jar" -X DELETE "$demo/api/v1/auth/app-passwords/$app_password_id" >/dev/null &&
        echo "the app password is revoked"
}

login() {
    local ap password device keyring=
    curl -fsS -c "$jar" -H 'Content-Type: application/json' \
        -d "{\"email\":\"$demo_email\",\"password\":\"$demo_password\"}" "$demo/api/v1/auth/login" >/dev/null
    ap=$(curl -fsS -b "$jar" -H 'Content-Type: application/json' \
        -d '{"name":"Release screenshots (GitHub runner)"}' "$demo/api/v1/auth/app-passwords")
    password=$(jq -r .password <<<"$ap")
    echo "::add-mask::$password"
    app_password_id=$(jq -r .app_password.id <<<"$ap")
    trap revoke EXIT
    case $os in
    windows) device='Windows PC' ;;
    macos) device='Mac' keyring=--no-keyring ;;
    linux) device='Linux PC' keyring=--no-keyring ;;
    esac
    # $keyring: one word or none, unquoted on purpose.
    # shellcheck disable=SC2086
    THECA_TOKEN=$password "$theca" login "$demo" --name "$device" $keyring
    # The .deb is updated by apt, and refuses (theca updates).
    if [ "$os" != linux ]; then "$theca" updates off; fi
    "$theca" add Documents
    "$theca" add 'My files'
    "$theca" add Photos --drive
}

start_app() {
    case $os in
    windows) powershell -NoProfile -Command "Start-Process -FilePath '$(cygpath -w "$theca")'" ;;
    macos) open -a /Applications/Theca.app ;;
    linux) nohup theca >/dev/null 2>&1 & ;;
    esac
}

# A Mac adds the drive switched off, for the user to switch on in System
# Settings: here the way tests/mac_drive.rs does (fileproviderd's own list, a
# private thing, only for CI).
switch_on_mac() {
    local plist="$HOME/Library/Application Support/FileProvider/rs.theca.computer.drive/Domains.plist"
    local _
    for _ in $(seq 60); do
        [ -f "$plist" ] && break
        sleep 1
    done
    enable_domains "$plist"
    killall -9 fileproviderd || true
    sleep 2
    enable_domains "$plist"
}
enable_domains() {
    python3 - "$1" <<'PY'
import plistlib, sys
with open(sys.argv[1], "rb") as f:
    domains = plistlib.load(f)
for domain in domains.values():
    if isinstance(domain, dict):
        domain["Enabled"] = True
with open(sys.argv[1], "wb") as f:
    plistlib.dump(domains, f)
PY
}

# Every library synced (the folders) or served (the drive): a picture of an app
# stuck syncing, or signed out, passes for nothing.
wait_synced() {
    local out="" _
    for _ in $(seq 120); do
        out=$("$theca" status 2>&1 | tr -d '\r' || true)
        if [ "$(grep -c '  up to date' <<<"$out" || true)" = 3 ]; then
            echo "$out"
            return 0
        fi
        sleep 1
    done
    echo "the three libraries aren't all up to date after 120 s:" >&2
    echo "$out" >&2
    return 1
}

shot() {
    case $os in
    windows) win shot "$(cygpath -w "$PWD/shots/$1.png")" ;;
    macos) screencapture -x "shots/$1.png" ;;
    linux) import -window root "shots/$1.png" ;;
    esac
}

# The file manager on a folder, given a moment to draw it.
show() {
    case $os in
    windows) powershell -NoProfile -Command "Start-Process explorer.exe '$(cygpath -w "$1")'" ;;
    macos) open "$1" ;;
    linux) thunar "$1" >/dev/null 2>&1 & ;;
    esac
    sleep 10
}

install_app
login
start_app
if [ "$os" = macos ]; then switch_on_mac; fi
status=$(wait_synced)
# The drive's folder, from the line `theca status` gives it: `  Photos  <path>  up to date…`.
drive=$(awk -F'  ' '$2 == "Photos" { print $3 }' <<<"$status")
[ -n "$drive" ] || { echo "no Photos folder in theca status:" >&2; echo "$status" >&2; exit 1; }
# `theca status` answers from the state: the window may still be loading.
sleep 10
shot app
show "$HOME/theca"
shot folders
show "$drive"
shot drive
for name in app folders drive; do
    [ -s "shots/$name.png" ] || { echo "no shots/$name.png" >&2; exit 1; }
done
ls -l shots
