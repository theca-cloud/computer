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
# Linux's .rpm files are signed (rpmsign) with LINUX_SIGNING_KEY, the key of
# ci/packages.asc that every package ships; a run by hand signs them with a
# throwaway key, a tag never.
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
cli=theca-cli-$version-$os-$arch
bundle=target/release/bundle
dist=$PWD/dist
# A runner's temp folder, or one of our own when run by hand.
tmp=${RUNNER_TEMP:-$(mktemp -d)}
rm -rf "$dist" && mkdir "$dist"

# The developer's signatures (wiki: Releases): macOS's Developer ID and
# notarization, Windows' Certum certificate. release.yml gives the
# credentials on a version tag only; without them, by hand, the packages are
# ad hoc and unsigned, and their signatures go unchecked; a tag never.
# All or none: a part of them is a mistake, not an unsigned run.
#   APPLE_SIGNING_IDENTITY ("Developer ID Application: <name> (<team>)"),
#   APPLE_API_KEY, APPLE_API_ISSUER, APPLE_API_PRIVATE_KEY (the .p8's text):
#   the App Store Connect key that notarizes;
#   APPLE_CERTIFICATE (the .p12, base64) and APPLE_CERTIFICATE_PASSWORD:
#   imported into a keychain of this run's (on a developer's Mac, left
#   unset: the identity is in their keychain).
# Windows': Certum's Open Source certificate, its key in SimplySign's cloud,
# signed with by ssign (release.yml installs it):
#   CERTUM_EMAIL, the SimplySign login; CERTUM_OTP, its TOTP seed (the
#   otpauth:// URI of SimplySign's QR code).
# Linux's: LINUX_SIGNING_KEY, the private key of ci/packages.asc (armored),
# which signs the .rpm files and the repositories' indexes.
case $os in
macos) credentials="APPLE_SIGNING_IDENTITY APPLE_API_KEY APPLE_API_ISSUER APPLE_API_PRIVATE_KEY" ;;
windows) credentials="CERTUM_EMAIL CERTUM_OTP" ;;
linux) credentials="LINUX_SIGNING_KEY" ;;
*) credentials= ;;
esac
given=
missing=
for v in $credentials; do
    if [ -n "${!v:-}" ]; then given="$given $v"; else missing="$missing $v"; fi
done
signed=false
if [ -n "$credentials" ] && [ -z "$missing" ]; then
    signed=true
elif [ -n "$given" ]; then
    echo "only part of the signing credentials: missing$missing" >&2
    exit 1
elif [ -n "$credentials" ]; then
    if [ "${GITHUB_REF_TYPE:-}" = tag ]; then
        echo "no signing credentials ($credentials): a release is signed by the developer" >&2
        exit 1
    fi
    if [ "$os" = linux ]; then
        echo "no LINUX_SIGNING_KEY: the .rpm files are signed with a throwaway key (a run by hand)" >&2
    else
        echo "no signing credentials: the $os packages are ad hoc and unsigned (a run by hand)" >&2
    fi
fi
if ! $signed; then
    # release.yml passes them empty: Tauri takes a variable that is set, even
    # empty, as an identity (APPLE_SIGNING_IDENTITY="" overrides "-").
    unset APPLE_SIGNING_IDENTITY APPLE_API_KEY APPLE_API_ISSUER APPLE_API_PRIVATE_KEY \
        APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD CERTUM_EMAIL CERTUM_OTP LINUX_SIGNING_KEY
fi
cleanup() {
    rm -f "$tmp"/AuthKey_*.p8 "$tmp/certificate.p12"
    if [ -d "$tmp/gnupg" ]; then
        GNUPGHOME=$tmp/gnupg gpgconf --kill gpg-agent 2>/dev/null || true
        rm -rf "$tmp/gnupg"
    fi
    if [ -n "${keychain:-}" ]; then security delete-keychain "$keychain" 2>/dev/null || true; fi
}
trap cleanup EXIT
if [ "$os" = macos ] && $signed; then
    # notarytool and Tauri read the key from a file.
    APPLE_API_KEY_PATH=$tmp/AuthKey_$APPLE_API_KEY.p8
    (umask 077 && printf '%s\n' "$APPLE_API_PRIVATE_KEY" >"$APPLE_API_KEY_PATH")
    export APPLE_API_KEY_PATH
    if [ -n "${APPLE_CERTIFICATE:-}" ]; then
        # A keychain of our own, before the extension is signed (Tauri would
        # import it only when it builds, after): codesign finds the identity
        # in the search list. Tauri is then given the identity alone, not the
        # certificate, or it imports a second copy (an ambiguous identity).
        keychain=$tmp/signing.keychain-db
        password=$(openssl rand -hex 16)
        security create-keychain -p "$password" "$keychain"
        security set-keychain-settings -lut 21600 "$keychain"
        security unlock-keychain -p "$password" "$keychain"
        printf '%s' "$APPLE_CERTIFICATE" | base64 --decode >"$tmp/certificate.p12"
        security import "$tmp/certificate.p12" -k "$keychain" -P "$APPLE_CERTIFICATE_PASSWORD" -T /usr/bin/codesign
        rm -f "$tmp/certificate.p12"
        security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$password" "$keychain" >/dev/null
        # shellcheck disable=SC2046 # the list's paths, one word each
        security list-keychains -d user -s "$keychain" $(security list-keychains -d user | tr -d '"')
        unset APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD password
    fi
    security find-identity -v -p codesigning | grep -qF "\"$APPLE_SIGNING_IDENTITY\"" ||
        { echo "no identity '$APPLE_SIGNING_IDENTITY' in the keychains" >&2; exit 1; }
fi
if [ "$os" = windows ] && $signed; then
    command -v ssign >/dev/null || { echo "no ssign on PATH: release.yml installs it" >&2; exit 1; }
    # The runner's Windows SDK (10.0.26100.0 on windows-2025), its newest.
    # `|| true`: without the SDK, ls fails and pipefail would stop the script
    # before its message.
    signtool=$(ls "/c/Program Files (x86)/Windows Kits/10/bin/"*/x64/signtool.exe 2>/dev/null | sort -V | tail -1 || true)
    [ -n "$signtool" ] || { echo "no signtool.exe in the Windows SDK" >&2; exit 1; }
fi

# Linux: the key that signs the .rpm files. A tag's is the one in
# LINUX_SIGNING_KEY, which must be the key every package ships (ci/packages.asc:
# apt's keyring and rpm's key, public); a run by hand signs with a throwaway
# key made for it (the real one signs releases only), imported by the checks.
if [ "$os" = linux ]; then
    export GNUPGHOME=$tmp/gnupg
    (umask 077 && mkdir "$GNUPGHOME")
    secret_key() { gpg --batch --with-colons --list-secret-keys | awk -F: '/^fpr/ { print $10; exit }'; }
    shipped=$(gpg --batch --with-colons --show-keys ci/packages.asc | awk -F: '/^fpr/ { print $10; exit }')
    if $signed; then
        printf '%s\n' "$LINUX_SIGNING_KEY" | gpg --batch --quiet --import
        unset LINUX_SIGNING_KEY
        rpm_key=$(secret_key)
        [ "$rpm_key" = "$shipped" ] ||
            { echo "LINUX_SIGNING_KEY ($rpm_key) isn't the key the packages ship, ci/packages.asc ($shipped)" >&2; exit 1; }
    else
        gpg --batch --quiet --pinentry-mode loopback --passphrase '' \
            --quick-generate-key "Theca packages, a run's throwaway key <throwaway@invalid>" rsa4096 sign 1d
        rpm_key=$(secret_key)
    fi
    gpg --batch --export --armor "$rpm_key" >"$tmp/rpm-signer.asc"
    # apt's Signed-By wants the binary keyring.
    gpg --batch --dearmor <ci/packages.asc >"$tmp/theca.gpg"
fi
# Signs rpm files with the key above.
rpm_sign() {
    rpmsign --addsign --define "__gpg $(command -v gpg)" --define "_gpg_name $rpm_key" --define "_gpg_path $GNUPGHOME" "$@" >/dev/null
}

check() {
    out=$("$@" --version | tr -d '\r')
    [ "$out" = "theca $version" ] || { echo "$1 --version: '$out', not 'theca $version'" >&2; exit 1; }
    echo "checked: $1 prints $out"
}

# A .deb's, then an .rpm's checks (Linux): the repository's files, fuse3, the
# conflict with $2 (theca and theca-cli own the same /usr/bin/theca), the
# files not conffiles (removed with the package, overwritten by an upgrade),
# and the command, unpacked, printing this version.
deb_check() {
    local file=$1 other=$2 f listing
    # The listing in a variable: `grep -q` closing a pipe early fails the
    # command writing to it, under pipefail.
    listing=$(dpkg-deb -c "$file")
    for f in etc/apt/sources.list.d/theca.sources usr/share/keyrings/theca.gpg usr/bin/theca; do
        grep -Eq " (\./)?$f\$" <<<"$listing" || { echo "$file doesn't hold /$f" >&2; exit 1; }
    done
    # fuse3 itself: not libfuse3-3, which another dependency could pull in.
    grep -Eq '(^|[ ,|])fuse3([ ,|(]|$)' <<<"$(dpkg-deb -f "$file" Depends)" || { echo "$file doesn't depend on fuse3" >&2; exit 1; }
    for f in Conflicts Replaces; do
        [ "$(dpkg-deb -f "$file" "$f")" = "$other" ] || { echo "$file: $f isn't $other" >&2; exit 1; }
    done
    rm -rf "$tmp/control" "$tmp/unpacked-deb"
    dpkg-deb -e "$file" "$tmp/control"
    if [ -f "$tmp/control/conffiles" ]; then
        echo "$file has conffiles: $(cat "$tmp/control/conffiles")" >&2
        exit 1
    fi
    dpkg-deb -x "$file" "$tmp/unpacked-deb"
    check "$tmp/unpacked-deb/usr/bin/theca"
}
rpm_check() {
    local file=$1 other=$2 f listing
    grep -q 'digests signatures OK' <<<"$(rpm --dbpath "$tmp/rpmdb" -K "$file")" ||
        { echo "$file isn't signed by this run's key" >&2; exit 1; }
    listing=$(rpm --dbpath "$tmp/rpmdb" -qpl "$file")
    for f in /etc/yum.repos.d/theca.repo /etc/pki/rpm-gpg/RPM-GPG-KEY-theca /usr/bin/theca; do
        grep -qx "$f" <<<"$listing" || { echo "$file doesn't hold $f" >&2; exit 1; }
    done
    grep -Eq '^fuse3( |$)' <<<"$(rpm --dbpath "$tmp/rpmdb" -qp --requires "$file")" || { echo "$file doesn't require fuse3" >&2; exit 1; }
    # A conflict only: an Obsoletes in each package would hide the other from
    # dnf once one is installed.
    [ "$(rpm --dbpath "$tmp/rpmdb" -qp --conflicts "$file")" = "$other" ] || { echo "$file: conflicts isn't $other" >&2; exit 1; }
    obsoletes=$(rpm --dbpath "$tmp/rpmdb" -qp --obsoletes "$file")
    [ -z "$obsoletes" ] || { echo "$file obsoletes $obsoletes" >&2; exit 1; }
    # rpm2cpio exits 1 on Tauri's .rpm, whose payload it writes whole and which
    # Fedora installs and runs (tried, Fedora 43): what is checked is the
    # command it unpacks.
    rm -rf "$tmp/unpacked-rpm" && mkdir "$tmp/unpacked-rpm"
    rpm2cpio "$file" >"$tmp/payload.cpio" || true
    (cd "$tmp/unpacked-rpm" && cpio -idm --quiet <"$tmp/payload.cpio")
    check "$tmp/unpacked-rpm/usr/bin/theca"
}

# The developer's signatures (signed runs only), each failing the run with
# what it found. Gatekeeper's verdict on $1, spctl's options after it.
notarized() {
    local file=$1 out
    shift
    out=$(spctl -a -vvv "$@" "$file" 2>&1) || { echo "Gatekeeper refuses $file: $out" >&2; exit 1; }
    grep -q 'source=Notarized Developer ID' <<<"$out" ||
        { echo "$file isn't notarized: $out" >&2; exit 1; }
}
stapled() {
    local out
    out=$(xcrun stapler validate -v "$1" 2>&1) ||
        { echo "no notarization ticket stapled to $1: $out" >&2; exit 1; }
}
# An .app as a user gets it: notarized, its ticket stapled, its extension
# signed by the developer with the hardened runtime.
developer_signed() {
    local appex=$1/Contents/PlugIns/ThecaDrive.appex details
    notarized "$1" -t execute
    stapled "$1"
    details=$(codesign --display --verbose=2 "$appex" 2>&1)
    grep -qF "Authority=$APPLE_SIGNING_IDENTITY" <<<"$details" ||
        { echo "the drive's extension isn't signed by $APPLE_SIGNING_IDENTITY: $details" >&2; exit 1; }
    grep -q 'flags=.*(runtime)' <<<"$details" ||
        { echo "the drive's extension lacks the hardened runtime: $details" >&2; exit 1; }
    # A secure timestamp (codesign's default may give one or not).
    grep -q '^Timestamp=' <<<"$details" ||
        { echo "the drive's extension has no secure timestamp: $details" >&2; exit 1; }
    echo "checked: $1 is notarized, stapled, its extension the developer's, timestamped"
}
# Apple's notarization of $1 (Tauri does the .app's), then its ticket
# stapled. notarytool's exit status isn't trusted: its answer must say
# Accepted (as Tauri reads it).
notarize() {
    local out status id
    out=$(xcrun notarytool submit "$1" --key "$APPLE_API_KEY_PATH" --key-id "$APPLE_API_KEY" \
        --issuer "$APPLE_API_ISSUER" --wait --output-format json) || true
    status=$(python3 -c 'import json, sys; print(json.load(sys.stdin).get("status", ""))' <<<"$out" 2>/dev/null) || status=
    if [ "$status" != Accepted ]; then
        echo "notarizing $1: '${status:-no answer}': $out" >&2
        id=$(python3 -c 'import json, sys; print(json.load(sys.stdin).get("id", ""))' <<<"$out" 2>/dev/null) || id=
        if [ -n "$id" ]; then
            xcrun notarytool log "$id" --key "$APPLE_API_KEY_PATH" --key-id "$APPLE_API_KEY" \
                --issuer "$APPLE_API_ISSUER" >&2 || true
        fi
        exit 1
    fi
    xcrun stapler staple "$1" >/dev/null
    echo "notarized: $1"
}
# Windows: $1 signed with the developer's Certum certificate, timestamped.
# signtool's flags kept from Git Bash's path conversion (/pa would become a
# path), the file given as a Windows path (stack wiki: Windows).
authenticode() {
    local out
    out=$(MSYS2_ARG_CONV_EXCL='*' "$signtool" verify /pa /v "$(cygpath -w "$1")" 2>&1) ||
        { echo "$1 isn't signed: $out" >&2; exit 1; }
    grep -q 'Issued to: Open Source Developer' <<<"$out" ||
        { echo "$1 isn't signed with the developer's Certum certificate: $out" >&2; exit 1; }
    grep -q 'The signature is timestamped' <<<"$out" ||
        { echo "$1's signature has no timestamp: $out" >&2; exit 1; }
    echo "checked: $1 is signed by the developer, timestamped"
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
bundle_config='"createUpdaterArtifacts":true'
# Windows: Tauri signs theca.exe before NSIS packs it, NSIS's plugins, the
# uninstaller and the installer, calling ssign on each (%1, the file).
if [ "$os" = windows ] && $signed; then
    bundle_config="$bundle_config,\"windows\":{\"signCommand\":{\"cmd\":\"ssign\",\"args\":[\"%1\"]}}"
fi
tauri_build "{\"build\":{\"beforeBuildCommand\":\"\"},\"bundle\":{$bundle_config},\"plugins\":{\"updater\":{\"pubkey\":\"$signing_pub\"}}}" \
    --bundles "$bundles" --features self-update
# Linux's app a second time, without self-update: the .deb and the .rpm, which
# add our repository (ci/packages/) to what apt and dnf read.
if [ "$os" = linux ]; then
    deb_files="\"/etc/apt/sources.list.d/theca.sources\":\"ci/packages/theca.sources\",\"/usr/share/keyrings/theca.gpg\":\"$tmp/theca.gpg\""
    rpm_files="\"/etc/yum.repos.d/theca.repo\":\"ci/packages/theca.repo\",\"/etc/pki/rpm-gpg/RPM-GPG-KEY-theca\":\"ci/packages.asc\""
    tauri_build "{\"build\":{\"beforeBuildCommand\":\"\"},\"bundle\":{\"linux\":{\"deb\":{\"files\":{$deb_files}},\"rpm\":{\"files\":{$rpm_files}}}}}" \
        --bundles deb,rpm
fi
# theca-cli, the command line for apt and dnf: the CLI a third time, without
# self-update, packaged by nfpm (pinned, its checksum checked), with the same
# repository files as the app's.
if [ "$os" = linux ]; then
    cargo build --release --target-dir target/cli-pkg
    case $arch in
    x86_64) nfpm_arch=amd64 nfpm_sha=6d7f9a28b2f397934403950fcbba83f598b08d02edbe79a0815b3b4897341750 ;;
    arm64) nfpm_arch=arm64 nfpm_sha=a9a75213763783b7669719e505474c748c5ad7dc66591a195619deb50f85c2f4 ;;
    esac
    curl -fsSL --retry 3 -o "$tmp/nfpm.tar.gz" \
        "https://github.com/goreleaser/nfpm/releases/download/v2.48.0/nfpm_2.48.0_Linux_$arch.tar.gz"
    echo "$nfpm_sha  $tmp/nfpm.tar.gz" | sha256sum -c --quiet -
    mkdir "$tmp/nfpm" && tar -xzf "$tmp/nfpm.tar.gz" -C "$tmp/nfpm" nfpm
    cp "$tmp/theca.gpg" target/cli-pkg/theca.gpg
    for format in deb rpm; do
        NFPM_VERSION=$version NFPM_ARCH=$nfpm_arch "$tmp/nfpm/nfpm" package -f ci/packages/nfpm.yaml \
            -p "$format" -t "dist/$cli.$format" >/dev/null
    done
fi

# The CLI: no GUI library linked (ADR 0004). Not on macOS since 1.2.0: a
# Mac's CLI is the app's own `theca` (its drive needs the app's extension).
if [ "$os" != macos ]; then
    cargo build --release --features self-update --target-dir target/cli
    mkdir "$tmp/$cli"
    cp LICENSE README.md USING.md "$tmp/$cli/"
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
    # Tauri notarized and stapled the .app, the .app.tar.gz made from it; it
    # only signs the .dmg.
    if $signed; then notarize "dist/$name.dmg"; fi
    mnt=$tmp/dmg
    hdiutil attach -nobrowse -readonly -mountpoint "$mnt" "dist/$name.dmg" >/dev/null
    check "$mnt/Theca.app/Contents/MacOS/theca"
    if $signed; then developer_signed "$mnt/Theca.app"; fi
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
    if $signed; then
        stapled "dist/$name.dmg"
        notarized "dist/$name.dmg" -t open --context context:primary-signature
        # What the updater installs.
        mkdir "$tmp/updater"
        tar -xzf "dist/$name.app.tar.gz" -C "$tmp/updater"
        developer_signed "$tmp/updater/Theca.app"
        echo "checked: the .dmg and the updater's .app are notarized, stapled, the developer's"
    fi
    ;;
windows)
    cp "$bundle"/nsis/Theca_"$version"_*-setup.exe "dist/$name.exe"
    cp "$bundle"/nsis/Theca_"$version"_*-setup.exe.sig "dist/$name.exe.sig"
    if $signed; then authenticode "dist/$name.exe"; fi
    # NSIS, silently, waited for: per user, under %LOCALAPPDATA%.
    powershell -NoProfile -Command \
        "Start-Process -Wait -FilePath '$(cygpath -w "dist/$name.exe")' -ArgumentList '/S'"
    installed=$(find "$(cygpath "$LOCALAPPDATA")" -maxdepth 3 -ipath '*theca*/theca.exe' | head -1)
    [ -n "$installed" ] || { echo "the installer put no theca.exe under %LOCALAPPDATA%" >&2; exit 1; }
    check "$installed"
    # Before uninstall_removes_drives removes them.
    if $signed; then
        authenticode "$installed"
        authenticode "$(dirname "$installed")/uninstall.exe"
    fi
    uninstall_removes_drives "$installed" "dist/$name.exe"
    ;;
linux)
    cp "$bundle"/appimage/Theca_"$version"_*.AppImage "dist/$name.AppImage"
    cp "$bundle"/appimage/Theca_"$version"_*.AppImage.sig "dist/$name.AppImage.sig"
    cp "$bundle"/deb/Theca_"$version"_*.deb "dist/$name.deb"
    cp "$bundle"/rpm/*.rpm "dist/$name.rpm"
    rpm_sign "dist/$name.rpm" "dist/$cli.rpm"
    # No FUSE on the runner: the AppImage unpacks itself instead.
    APPIMAGE_EXTRACT_AND_RUN=1 check "dist/$name.AppImage"
    # The four packages for apt and dnf: our repository's files, fuse3 (drives
    # need it), the other one as a conflict, the command in each, and each
    # .rpm signed by this run's key (rpm_key).
    rpm --dbpath "$tmp/rpmdb" --import "$tmp/rpm-signer.asc"
    deb_check "dist/$name.deb" theca-cli
    deb_check "dist/$cli.deb" theca
    rpm_check "dist/$name.rpm" theca-cli
    rpm_check "dist/$cli.rpm" theca
    # Installed with apt, as users do.
    sudo apt-get install -yq "./dist/$name.deb" >/dev/null
    check /usr/bin/theca
    ;;
esac

# The CLI archive: a folder of its name, unpacked again to be checked.
check_dir=$tmp/unpacked
mkdir "$check_dir"
if [ "$os" = macos ]; then
    :
elif [ "$os" = windows ]; then
    # Built outside Tauri: signed here, before the zip and its .sig.
    if $signed; then ssign target/cli/release/theca.exe; fi
    cp target/cli/release/theca.exe "$tmp/$cli/"
    (cd "$tmp" && 7z a -tzip -bso0 "$dist/$cli.zip" "$cli")
    sign "dist/$cli.zip"
    7z x -bso0 -o"$check_dir" "dist/$cli.zip"
    check "$check_dir/$cli/theca.exe"
    if $signed; then authenticode "$check_dir/$cli/theca.exe"; fi
else
    cp target/cli/release/theca "$tmp/$cli/"
    tar -czf "dist/$cli.tar.gz" -C "$tmp" "$cli"
    sign "dist/$cli.tar.gz"
    tar -xzf "dist/$cli.tar.gz" -C "$check_dir"
    check "$check_dir/$cli/theca"
fi
ls -l dist
# The update itself, A to B, with a test key (no release needed).
ci/update-check.sh "$os" "$arch"
