#!/usr/bin/env bash
# The update itself, checked without a release (ADR 0006): A, this commit
# built with a test key, updated to B, the same commit with its version
# bumped, built and signed with that key, through a manifest served here
# (`theca update --manifest`): the app as installed (the .app, the NSIS
# install, the .AppImage) and headless. First B signed by another key is
# served, and A must refuse it and still be A; then B, which A installs.
# Each version read from the installed files, never by starting the app
# while it installs. Run by ci/package.sh after the packages, on every run:
#   ci/update-check.sh <macos|windows|linux> <arm64|x86_64>
set -euo pipefail
cd "$(dirname "$0")/.."

os=$1
arch=$2
version_in() { sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1; }
version=$(version_in)
next=$(echo "$version" | awk -F. '{ print $1 "." $2 "." $3 + 1 }')
tmp=${RUNNER_TEMP:-$(mktemp -d)}/update-check
rm -rf "$tmp"
# What is served: B signed by the test key (good/), and by another (other/).
dist=$tmp/serve/good
other=$tmp/serve/other
mkdir -p "$tmp/a" "$tmp/b" "$dist" "$other" "$tmp/installed"
# Git Bash leaves arguments alone (it rewrote /S into S:/: the installer
# showed its wizard and waited).
export MSYS2_ARG_CONV_EXCL='*'
exe=
py=python3
if [ "$os" = windows ]; then
    exe=.exe
    py=python
fi

# Cargo.toml and Cargo.lock as they were, whatever happens: B bumps the
# version, `tauri build` rewrites a line.
cp Cargo.toml "$tmp/Cargo.toml"
cp Cargo.lock "$tmp/Cargo.lock"
server=
cleanup() {
    cp "$tmp/Cargo.toml" Cargo.toml
    cp "$tmp/Cargo.lock" Cargo.lock
    if [ -n "$server" ]; then kill "$server" 2>/dev/null || true; fi
}
trap cleanup EXIT

tauri=ui/node_modules/.bin/tauri
key() { "$tauri" signer generate --ci -p "" -w "$1" >/dev/null; }
key "$tmp/test.key"
key "$tmp/other.key"
THECA_TEST_UPDATER_PUBKEY=$(cat "$tmp/test.key.pub")
TAURI_SIGNING_PRIVATE_KEY=$(cat "$tmp/test.key")
TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""
export THECA_TEST_UPDATER_PUBKEY TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PASSWORD

case $os in
macos) bundles=app ;;
windows) bundles=nsis ;;
linux) bundles=appimage ;;
esac
bundle=target/release/bundle

# The app and headless at Cargo.toml's version, with the test key, into $1
# (B's updater packages signed).
build() {
    out=$1
    v=$(version_in)
    # The bundler checks its signatures with the signing key's public half.
    # Tauri's updater takes the manifest served here over http only when
    # told (these builds alone: they embed the run's test key, and
    # ci/package.sh refuses that key on a tag).
    "$tauri" build --bundles "$bundles" --features self-update --config \
        "{\"build\":{\"beforeBuildCommand\":\"\"},\"bundle\":{\"createUpdaterArtifacts\":true},\"plugins\":{\"updater\":{\"pubkey\":\"$(cat "$tmp/test.key.pub")\",\"dangerousInsecureTransportProtocol\":true}}}"
    case $os in
    macos)
        cp -R "$bundle/macos/Theca.app" "$out/"
        cp "$bundle/macos/Theca.app.tar.gz" "$out/app.tar.gz"
        cp "$bundle/macos/Theca.app.tar.gz.sig" "$out/app.tar.gz.sig"
        ;;
    windows)
        cp "$bundle"/nsis/Theca_"$v"_*-setup.exe "$out/setup.exe"
        cp "$bundle"/nsis/Theca_"$v"_*-setup.exe.sig "$out/setup.exe.sig"
        ;;
    linux)
        cp "$bundle"/appimage/Theca_"$v"_*.AppImage "$out/Theca.AppImage"
        cp "$bundle"/appimage/Theca_"$v"_*.AppImage.sig "$out/Theca.AppImage.sig"
        ;;
    esac
    if [ "$os" != macos ]; then
        cargo build --release --features self-update --target-dir target/headless
        cp "target/headless/release/theca$exe" "$out/"
    fi
}

build "$tmp/a"
awk -v from="version = \"$version\"" -v to="version = \"$next\"" \
    '!done && $0 == from { print to; done = 1; next } { print }' "$tmp/Cargo.toml" >Cargo.toml
build "$tmp/b"
cp "$tmp/Cargo.toml" Cargo.toml

# B as a release publishes it: package.sh's names, latest.json from them.
name=theca-$next-$os-$arch
case $os in
macos) app=app.tar.gz ext=app.tar.gz ;;
windows) app=setup.exe ext=exe ;;
linux) app=Theca.AppImage ext=AppImage ;;
esac
cp "$tmp/b/$app" "$dist/$name.$ext"
cp "$tmp/b/$app.sig" "$dist/$name.$ext.sig"
if [ "$os" != macos ]; then
    headless=theca-headless-$next-$os-$arch
    mkdir "$tmp/$headless"
    cp "$tmp/b/theca$exe" "$tmp/$headless/"
    if [ "$os" = windows ]; then
        (cd "$tmp" && 7z a -tzip -bso0 "$dist/$headless.zip" "$headless")
        archive=$dist/$headless.zip
    else
        tar -czf "$dist/$headless.tar.gz" -C "$tmp" "$headless"
        archive=$dist/$headless.tar.gz
    fi
    "$tauri" signer sign "$archive" >/dev/null
fi
# The same packages, signed by the other key.
for package in "$dist"/*; do
    case $package in *.sig) continue ;; esac
    cp "$package" "$other/"
    TAURI_SIGNING_PRIVATE_KEY=$(cat "$tmp/other.key") \
        "$tauri" signer sign "$other/$(basename "$package")" >/dev/null
done
port=$("$py" -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
url=http://127.0.0.1:$port
for served in good other; do
    "$py" ci/manifest.py "$next" "$url/$served" "$tmp/serve/$served" >"$tmp/latest.json"
    mv "$tmp/latest.json" "$tmp/serve/$served/latest.json"
done
cat "$dist/latest.json"
"$py" -m http.server "$port" --bind 127.0.0.1 --directory "$tmp/serve" >/dev/null 2>&1 &
server=$!
for _ in $(seq 50); do
    "$py" -c "import urllib.request; urllib.request.urlopen('$url/good/latest.json')" 2>/dev/null && break
    sleep 0.2
done

# `$2 --version` is version $1.
is() {
    out=$("$2" --version | tr -d '\r')
    [ "$out" = "theca $1" ] || { echo "$2 --version: '$out', not 'theca $1'" >&2; exit 1; }
}
is_b() {
    is "$next" "$1"
    echo "updated: $1 is $next"
}
# `theca update` from $1, against the manifest of good/ or other/ ($2).
update() {
    "$1" --state "$tmp/state.sqlite" --manifest "$url/$2/latest.json" update
}
# B signed by the other key: refused for that (our code's words, or Tauri's
# updater's), and A still A; with `installer` (Windows' app), after the time
# an installer it might have started would take to replace it.
refuses() {
    # Its output to a file, never a pipe: an installer or app it would start
    # holds what it inherits, and the check would hang instead of failing.
    if update "$1" other >"$tmp/said" 2>&1; then
        cat "$tmp/said"
        echo "$1 took B signed by another key" >&2
        exit 1
    fi
    said=$(cat "$tmp/said")
    echo "$said"
    case $said in
    *"isn't signed by Theca's release key"* | *"signature was created with a different key"*) ;;
    *)
        echo "$1 failed, but not for the key" >&2
        exit 1
        ;;
    esac
    if [ "${2:-}" = installer ]; then sleep 30; fi
    is "$version" "$1"
    echo "refused: $1 took nothing signed by another key"
}

case $os in
macos)
    cp -R "$tmp/a/Theca.app" "$tmp/installed/"
    refuses "$tmp/installed/Theca.app/Contents/MacOS/theca"
    update "$tmp/installed/Theca.app/Contents/MacOS/theca" good
    is_b "$tmp/installed/Theca.app/Contents/MacOS/theca"
    ;;
windows)
    powershell -NoProfile -Command \
        "Start-Process -Wait -FilePath '$(cygpath -w "$tmp/a/setup.exe")' -ArgumentList '/S'"
    installed=$(find "$(cygpath "$LOCALAPPDATA")" -maxdepth 3 -ipath '*theca*/theca.exe' | head -1)
    [ -n "$installed" ] || { echo "A's installer put no theca.exe under %LOCALAPPDATA%" >&2; exit 1; }
    # Tauri's updater refuses another key's before running any installer.
    refuses "$installed" installer
    # Tauri's updater runs B's installer (quiet) and ends this process.
    update "$installed" good || true
    # B's installer works on: its version resource read until it is B, then
    # the installer awaited, and the app it starts again (/R) stopped.
    for _ in $(seq 300); do
        found=$(powershell -NoProfile -Command \
            "(Get-Item '$(cygpath -w "$installed")').VersionInfo.ProductVersion" 2>/dev/null | tr -d '\r')
        case $found in "$next"*) break ;; esac
        sleep 1
    done
    case $found in "$next"*) ;; *)
        echo "the installed theca.exe is still '$found', not $next" >&2
        exit 1
        ;;
    esac
    for _ in $(seq 120); do
        tasklist /FI "IMAGENAME eq Theca_${next}_x64-setup.exe" | grep -qi setup || break
        sleep 1
    done
    taskkill /F /IM theca.exe >/dev/null 2>&1 || true
    is_b "$installed"
    folder=$(cygpath -w "$(dirname "$installed")")
    powershell -NoProfile -Command \
        "Start-Process -Wait -FilePath '$folder\uninstall.exe' -ArgumentList '/S'"
    ;;
linux)
    # No FUSE on the runner: the .AppImage unpacks itself ($APPIMAGE still
    # names the file).
    cp "$tmp/a/Theca.AppImage" "$tmp/installed/Theca.AppImage"
    APPIMAGE_EXTRACT_AND_RUN=1 refuses "$tmp/installed/Theca.AppImage"
    APPIMAGE_EXTRACT_AND_RUN=1 update "$tmp/installed/Theca.AppImage" good
    APPIMAGE_EXTRACT_AND_RUN=1 is_b "$tmp/installed/Theca.AppImage"
    ;;
esac
if [ "$os" != macos ]; then
    cp "$tmp/a/theca$exe" "$tmp/installed/theca$exe"
    refuses "$tmp/installed/theca$exe"
    update "$tmp/installed/theca$exe" good
    is_b "$tmp/installed/theca$exe"
fi
echo "checked: A updated to B ($version to $next)"
