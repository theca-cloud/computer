#!/usr/bin/env bash
# This runner's packages, into dist/, each checked before it's kept: installed
# or unpacked, then `theca --version` must print this version (the tag's, on
# a tag). Used by .github/workflows/release.yml, one runner per OS and
# architecture:
#   ci/package.sh <macos|windows|linux> <arm64|x86_64>
# Every package updates itself (the `self-update` feature) but the .deb, which
# apt updates (ADR 0006): each is signed for the updater (a .sig beside it,
# which ci/manifest.py lists in latest.json), with TAURI_SIGNING_PRIVATE_KEY
# and its _PASSWORD (release.yml's secrets; a run without them, by hand,
# signs with a throwaway key; a tag never). THECA_TEST_UPDATER_PUBKEY, a test
# key's public half, replaces ci/updater.pub in the builds: refused on a tag.
set -euo pipefail
cd "$(dirname "$0")/.."

os=${1:-}
arch=${2:-}
case $os/$arch in
macos/arm64 | macos/x86_64 | windows/x86_64 | linux/arm64 | linux/x86_64) ;;
*)
    echo "usage: ci/package.sh <macos|windows|linux> <arm64|x86_64>" >&2
    exit 2
    ;;
esac
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
if [ "${GITHUB_REF_TYPE:-}" = tag ] && [ "$GITHUB_REF_NAME" != "$version" ]; then
    echo "the tag $GITHUB_REF_NAME isn't Cargo.toml's version $version" >&2
    exit 1
fi
if [ "${GITHUB_REF_TYPE:-}" = tag ] && [ -n "${THECA_TEST_UPDATER_PUBKEY:-}" ]; then
    echo "THECA_TEST_UPDATER_PUBKEY is set: a release embeds ci/updater.pub only" >&2
    exit 1
fi
name=theca-$version-$os-$arch
headless=theca-headless-$version-$os-$arch
bundle=target/release/bundle
dist=$PWD/dist
# A runner's temp folder, or one of our own when run by hand.
tmp=${RUNNER_TEMP:-$(mktemp -d)}
rm -rf "$dist" && mkdir "$dist"

check() {
    out=$("$@" --version | tr -d '\r')
    [ "$out" = "theca $version" ] || { echo "$1 --version: '$out', not 'theca $version'" >&2; exit 1; }
    echo "checked: $1 prints $out"
}

# With a window: the page first, then Tauri's bundles (the gui build). Tauri
# would run beforeBuildCommand from ui/, where `npm --prefix ui` finds nothing.
npm --prefix ui ci
npm --prefix ui run build
# The public half of the key that signs, for Tauri's bundler (it reads it
# from the configuration when it signs; tauri.conf.json has none).
signing_pub=$(cat ci/updater.pub)
if [ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]; then
    if [ "${GITHUB_REF_TYPE:-}" = tag ]; then
        echo "no TAURI_SIGNING_PRIVATE_KEY: a release is signed with the release key" >&2
        exit 1
    fi
    ui/node_modules/.bin/tauri signer generate --ci -p "" -w "$tmp/throwaway.key" >/dev/null
    TAURI_SIGNING_PRIVATE_KEY=$(cat "$tmp/throwaway.key")
    signing_pub=$(cat "$tmp/throwaway.key.pub")
    TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""
    export TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PASSWORD
    echo "no TAURI_SIGNING_PRIVATE_KEY: signed with a throwaway key (no install takes these)" >&2
fi
# A file's .sig beside it, as Tauri signs its own packages.
sign() {
    ui/node_modules/.bin/tauri signer sign "$1" >/dev/null
}
case $os in
# The .app too: the updater's .app.tar.gz is made from it.
macos) bundles=app,dmg ;;
windows) bundles=nsis ;;
linux) bundles=appimage ;;
esac
# macOS's drive: its extension built and signed first, for Tauri to copy
# into the app (tauri.conf.json, bundle.macOS.files) without signing it with
# the app's entitlements.
if [ "$os" = macos ]; then
    cargo build --release -p theca-fileprovider --bin theca-drive
    ci/mac-app.sh appex target/release/theca-drive target/appex/ThecaDrive.appex
fi
# $1: the configuration merged into tauri.conf.json.
tauri_build() {
    config=$1
    shift
    ui/node_modules/.bin/tauri build --config "$config" "$@"
}
tauri_build "{\"build\":{\"beforeBuildCommand\":\"\"},\"bundle\":{\"createUpdaterArtifacts\":true},\"plugins\":{\"updater\":{\"pubkey\":\"$signing_pub\"}}}" \
    --bundles "$bundles" --features self-update
# Linux's app a second time, without self-update: the .deb.
if [ "$os" = linux ]; then
    tauri_build '{"build":{"beforeBuildCommand":""}}' --bundles deb
fi

# Headless: no GUI library linked (ADR 0004). Not on macOS since 1.2.0: a
# Mac's CLI is the app's own `theca` (its drive needs the app's extension).
if [ "$os" != macos ]; then
    cargo build --release --features self-update --target-dir target/headless
    mkdir "$tmp/$headless"
    cp LICENSE README.md USING.md "$tmp/$headless/"
fi

# The uninstaller removes every drive first (ci/installer.nsh), but for an
# upgrade: a drive put in the installed app's state (its folder holding a
# file not uploaded) stays through an uninstall run in place, as an
# installer replacing the app runs it; once installed again, a user's
# uninstall (a copy from the temp folder) removes it, its file moved out to
# "(not uploaded)". $1: theca.exe as installed, $2: the installer.
uninstall_removes_drives() {
    "$1" status >/dev/null
    state=$(cygpath -w "$APPDATA/theca/theca/data/state.sqlite")
    home=$(cygpath -w "$USERPROFILE")
    drive="$home\theca-ci\Docs"
    mkdir -p "$(cygpath "$drive")"
    echo "not uploaded" >"$(cygpath "$drive")/edit.txt"
    python - "$state" "$home\theca-ci" "$drive" <<'PY'
import sqlite3, sys, uuid
state, default, drive = sys.argv[1:]
db = sqlite3.connect(state)
db.execute("INSERT INTO account (instance, uuid, email, device, password, default_folder)"
           " VALUES ('https://ci.invalid', ?, 'ci@ci.invalid', 'ci', 'x', ?)", (str(uuid.uuid4()), default))
db.execute("INSERT INTO pair (account, library, root, library_name, path, mode)"
           " VALUES (1, ?, ?, 'Docs', ?, 'drive')", (str(uuid.uuid4()), str(uuid.uuid4()), drive))
db.commit()
PY
    # In place (`_?=`), as "Uninstall before installing" runs it: the drive
    # stays.
    folder=$(cygpath -w "$(dirname "$1")")
    powershell -NoProfile -Command \
        "Start-Process -Wait -FilePath '$folder\uninstall.exe' -ArgumentList '/S','_?=$folder'"
    [ -e "$(cygpath "$drive")/edit.txt" ] || { echo "an upgrade's uninstall removed the drive" >&2; exit 1; }
    powershell -NoProfile -Command \
        "Start-Process -Wait -FilePath '$(cygpath -w "$2")' -ArgumentList '/S'"
    # As a user runs it: NSIS relaunches it from a copy, which nothing waits
    # for.
    powershell -NoProfile -Command \
        "Start-Process -Wait -FilePath '$folder\uninstall.exe' -ArgumentList '/S'"
    for _ in $(seq 120); do
        [ ! -e "$(cygpath "$drive")" ] && [ ! -e "$1" ] && break
        sleep 1
    done
    [ ! -e "$(cygpath "$drive")" ] || { echo "the uninstaller left the drive's folder" >&2; exit 1; }
    grep -q "not uploaded" "$(cygpath "$home")/theca-ci/Docs (not uploaded)/edit.txt" ||
        { echo "the uninstaller didn't move the drive's edit out" >&2; exit 1; }
}

case $os in
macos)
    cp "$bundle"/dmg/Theca_"$version"_*.dmg "dist/$name.dmg"
    # What the updater replaces the .app with.
    cp "$bundle"/macos/Theca.app.tar.gz "dist/$name.app.tar.gz"
    cp "$bundle"/macos/Theca.app.tar.gz.sig "dist/$name.app.tar.gz.sig"
    mnt=$tmp/dmg
    hdiutil attach -nobrowse -readonly -mountpoint "$mnt" "dist/$name.dmg" >/dev/null
    check "$mnt/Theca.app/Contents/MacOS/theca"
    # The drive's extension inside, both signatures intact.
    codesign --verify --strict "$mnt/Theca.app/Contents/PlugIns/ThecaDrive.appex"
    codesign --verify --strict "$mnt/Theca.app"
    # Still its own entitlements (a --deep signing of the app would drop its
    # sandbox, and PlugInKit refuse it).
    codesign --display --entitlements - "$mnt/Theca.app/Contents/PlugIns/ThecaDrive.appex" 2>&1 |
        grep -q com.apple.security.app-sandbox ||
        { echo "the drive's extension lost its sandbox entitlement" >&2; exit 1; }
    echo "checked: the drive's extension is in the app, signed, sandboxed"
    hdiutil detach "$mnt" >/dev/null
    ;;
windows)
    cp "$bundle"/nsis/Theca_"$version"_*-setup.exe "dist/$name.exe"
    cp "$bundle"/nsis/Theca_"$version"_*-setup.exe.sig "dist/$name.exe.sig"
    # NSIS, silently, waited for: per user, under %LOCALAPPDATA%.
    powershell -NoProfile -Command \
        "Start-Process -Wait -FilePath '$(cygpath -w "dist/$name.exe")' -ArgumentList '/S'"
    installed=$(find "$(cygpath "$LOCALAPPDATA")" -maxdepth 3 -ipath '*theca*/theca.exe' | head -1)
    [ -n "$installed" ] || { echo "the installer put no theca.exe under %LOCALAPPDATA%" >&2; exit 1; }
    check "$installed"
    uninstall_removes_drives "$installed" "dist/$name.exe"
    ;;
linux)
    cp "$bundle"/appimage/Theca_"$version"_*.AppImage "dist/$name.AppImage"
    cp "$bundle"/appimage/Theca_"$version"_*.AppImage.sig "dist/$name.AppImage.sig"
    cp "$bundle"/deb/Theca_"$version"_*.deb "dist/$name.deb"
    # No FUSE on the runner: the AppImage unpacks itself instead.
    APPIMAGE_EXTRACT_AND_RUN=1 check "dist/$name.AppImage"
    # Drives need fuse3: declared, and installed with it (apt, as users do).
    dpkg-deb -f "dist/$name.deb" Depends | grep -q fuse3 ||
        { echo "the .deb doesn't depend on fuse3" >&2; exit 1; }
    sudo apt-get install -yq "./dist/$name.deb" >/dev/null
    check /usr/bin/theca
    ;;
esac

# The headless archive: a folder of its name, unpacked again to be checked.
check_dir=$tmp/unpacked
mkdir "$check_dir"
if [ "$os" = macos ]; then
    :
elif [ "$os" = windows ]; then
    cp target/headless/release/theca.exe "$tmp/$headless/"
    (cd "$tmp" && 7z a -tzip -bso0 "$dist/$headless.zip" "$headless")
    sign "dist/$headless.zip"
    7z x -bso0 -o"$check_dir" "dist/$headless.zip"
    check "$check_dir/$headless/theca.exe"
else
    cp target/headless/release/theca "$tmp/$headless/"
    tar -czf "dist/$headless.tar.gz" -C "$tmp" "$headless"
    sign "dist/$headless.tar.gz"
    tar -xzf "dist/$headless.tar.gz" -C "$check_dir"
    check "$check_dir/$headless/theca"
fi
ls -l dist
# The update itself, A to B, with a test key (no release needed).
ci/update-check.sh "$os" "$arch"
