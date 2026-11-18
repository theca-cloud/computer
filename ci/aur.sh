#!/usr/bin/env bash
# theca-bin's PKGBUILD and .SRCINFO (AUR, Arch's user repository), from the
# release's .deb files: the app as the .deb has it (built without self-update:
# pacman updates it), unpacked, never built here. .SRCINFO, which the AUR also
# reads, is made by Arch's own makepkg (ci/aur-check.sh).
#
#   ci/aur.sh <version> <dist> <base url> <out>
#
# <dist>: the folder holding theca-<version>-linux-{x86_64,arm64}.deb (their
# checksums go into the PKGBUILD). <base url>: where those files are
# downloaded from: the release's (release.yml) or file://<dist> (the check,
# ci/aur-check.sh). <out>: a folder, made.
set -euo pipefail

[ $# = 4 ] || { sed -n '2,/^set /p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }
version=$1 dist=$2 base=${3%/} out=$4
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "not a release version: $version" >&2; exit 2; }
sum() {
    local file=$dist/theca-$version-linux-$1.deb
    [ -f "$file" ] || { echo "$file: missing" >&2; exit 1; }
    sha256sum "$file" | cut -d' ' -f1
}
x86_64=$(sum x86_64)
aarch64=$(sum arm64)

mkdir -p "$out"
cat > "$out/PKGBUILD" <<PKGBUILD
# The app (theca, tray and window; \`theca\` is also the command line), from the
# release's .deb. Updated by pacman: this build has no self-update.
pkgname=theca-bin
pkgver=$version
pkgrel=1
pkgdesc="Theca for computers: folders kept identical to Theca libraries, or libraries as drives"
arch=('x86_64' 'aarch64')
url="https://theca.rs/computer"
license=('AGPL-3.0-only')
depends=('webkit2gtk-4.1' 'libayatana-appindicator' 'gtk3' 'fuse3')
provides=('theca')
conflicts=('theca')
options=('!strip' '!debug')
noextract=("theca-\$pkgver-\$CARCH.deb")
source_x86_64=("theca-\$pkgver-x86_64.deb::$base/theca-\$pkgver-linux-x86_64.deb")
source_aarch64=("theca-\$pkgver-aarch64.deb::$base/theca-\$pkgver-linux-arm64.deb")
sha256sums_x86_64=('$x86_64')
sha256sums_aarch64=('$aarch64')

package() {
    # data.tar.* is the .deb's files; bsdtar reads the ar archive and its member
    bsdtar -Oxf "\$srcdir/theca-\$pkgver-\$CARCH.deb" 'data.tar*' | bsdtar -xf - -C "\$pkgdir"
    # What only apt uses (the .deb adds our repository to apt's)
    rm -rf "\$pkgdir/etc/apt" "\$pkgdir/usr/share/keyrings"
}
PKGBUILD
