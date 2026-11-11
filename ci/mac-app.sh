#!/usr/bin/env bash
# macOS's drive extension bundled and signed ad hoc, inside out (no --deep:
# it would give the extension the app's entitlements).
#   ci/mac-app.sh appex <theca-drive binary> <ThecaDrive.appex>
#   ci/mac-app.sh app <theca binary> <theca-drive binary> <Theca.app>
#     a minimal Theca.app (the tests'): the theca binary, the extension in
#     PlugIns/, as a Mac's install has them
set -euo pipefail
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)

appex() {
    local binary=$1 appex=$2
    rm -rf "$appex"
    mkdir -p "$appex/Contents/MacOS"
    cp fileprovider/mac/Info.plist "$appex/Contents/Info.plist"
    plutil -replace CFBundleShortVersionString -string "$version" "$appex/Contents/Info.plist"
    plutil -replace CFBundleVersion -string "$version" "$appex/Contents/Info.plist"
    cp "$binary" "$appex/Contents/MacOS/theca-drive"
    codesign --force --sign - --entitlements fileprovider/mac/drive.entitlements "$appex"
    codesign --verify --strict "$appex"
}

case ${1:-} in
appex) appex "$2" "$3" ;;
app)
    app=$4
    rm -rf "$app"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/PlugIns"
    cat >"$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key><string>rs.theca.computer</string>
	<key>CFBundleName</key><string>Theca</string>
	<key>CFBundleExecutable</key><string>theca</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
	<key>CFBundleShortVersionString</key><string>$version</string>
	<key>CFBundleVersion</key><string>$version</string>
	<key>LSMinimumSystemVersion</key><string>13.0</string>
	<key>LSUIElement</key><true/>
</dict>
</plist>
PLIST
    cp "$2" "$app/Contents/MacOS/theca"
    appex "$3" "$app/Contents/PlugIns/ThecaDrive.appex"
    codesign --force --sign - "$app"
    codesign --verify --strict "$app"
    ;;
*)
    echo "usage: ci/mac-app.sh appex <theca-drive> <out.appex> | app <theca> <theca-drive> <out.app>" >&2
    exit 2
    ;;
esac
