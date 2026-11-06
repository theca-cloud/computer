#!/usr/bin/env bash
# This runner's packages, into dist/, each checked before it's kept: installed
# or unpacked, then `theca --version` must print this version (the tag's, on
# a tag). Used by .github/workflows/release.yml, one runner per OS and
# architecture:
#   ci/package.sh <macos|windows|linux> <arm64|x86_64>
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
case $os in
macos) bundles=dmg ;;
windows) bundles=nsis ;;
linux) bundles=appimage,deb ;;
esac
ui/node_modules/.bin/tauri build --bundles "$bundles" --config '{"build":{"beforeBuildCommand":""}}'

# Headless: no GUI library linked (ADR 0004).
cargo build --release --target-dir target/headless
mkdir "$tmp/$headless"
cp LICENSE README.md "$tmp/$headless/"

case $os in
macos)
    cp "$bundle"/dmg/Theca_"$version"_*.dmg "dist/$name.dmg"
    mnt=$tmp/dmg
    hdiutil attach -nobrowse -readonly -mountpoint "$mnt" "dist/$name.dmg" >/dev/null
    check "$mnt/Theca.app/Contents/MacOS/theca"
    hdiutil detach "$mnt" >/dev/null
    ;;
windows)
    cp "$bundle"/nsis/Theca_"$version"_*-setup.exe "dist/$name.exe"
    # NSIS, silently, waited for: per user, under %LOCALAPPDATA%.
    powershell -NoProfile -Command \
        "Start-Process -Wait -FilePath '$(cygpath -w "dist/$name.exe")' -ArgumentList '/S'"
    installed=$(find "$(cygpath "$LOCALAPPDATA")" -maxdepth 3 -ipath '*theca*/theca.exe' | head -1)
    [ -n "$installed" ] || { echo "the installer put no theca.exe under %LOCALAPPDATA%" >&2; exit 1; }
    check "$installed"
    ;;
linux)
    cp "$bundle"/appimage/Theca_"$version"_*.AppImage "dist/$name.AppImage"
    cp "$bundle"/deb/Theca_"$version"_*.deb "dist/$name.deb"
    # No FUSE on the runner: the AppImage unpacks itself instead.
    APPIMAGE_EXTRACT_AND_RUN=1 check "dist/$name.AppImage"
    sudo dpkg -i "dist/$name.deb" >/dev/null
    check /usr/bin/theca
    ;;
esac

# The headless archive: a folder of its name, unpacked again to be checked.
check_dir=$tmp/unpacked
mkdir "$check_dir"
if [ "$os" = windows ]; then
    cp target/headless/release/theca.exe "$tmp/$headless/"
    (cd "$tmp" && 7z a -tzip -bso0 "$dist/$headless.zip" "$headless")
    7z x -bso0 -o"$check_dir" "dist/$headless.zip"
    check "$check_dir/$headless/theca.exe"
else
    cp target/headless/release/theca "$tmp/$headless/"
    tar -czf "dist/$headless.tar.gz" -C "$tmp" "$headless"
    tar -xzf "dist/$headless.tar.gz" -C "$check_dir"
    check "$check_dir/$headless/theca"
fi
ls -l dist
