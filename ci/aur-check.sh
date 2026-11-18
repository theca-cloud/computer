#!/usr/bin/env bash
# theca-bin, checked in Arch's container: the PKGBUILD ci/aur.sh writes is
# installed with makepkg from the run's own .deb files (a file:// source), and
# `theca --version` answers the run's version. Also writes what the AUR gets
# (PKGBUILD with the release's URLs, and its .SRCINFO from Arch's makepkg).
#
#   ci/aur-check.sh <version> <dist> <repository> <out>
#
# <repository>: owner/name on GitHub (the release's URLs). <out>/publish holds
# the AUR's files. x86_64 only: the arm64 .deb isn't run on an x86_64 runner.
set -euo pipefail

[ $# = 4 ] || { sed -n '2,/^set /p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }
version=$1 dist=$(cd "$2" && pwd) repository=$3 out=$4
here=$(cd "$(dirname "$0")" && pwd)
image=archlinux:base-devel-20261108.0.623074

rm -rf "$out" && mkdir -p "$out"
out=$(cd "$out" && pwd)
"$here/aur.sh" "$version" "$dist" "https://github.com/$repository/releases/download/$version" "$out/publish"
"$here/aur.sh" "$version" "$dist" "file:///dist" "$out/test"

# As root: a user for makepkg (it refuses root), its files handed back.
name=theca-aur-check-$$
docker run --rm --name "$name" -v "$dist:/dist:ro" -v "$out:/out" "$image" bash -euc '
    pacman -Syu --noconfirm --needed >/dev/null
    useradd -m builder
    echo "builder ALL=(ALL) NOPASSWD: ALL" > /etc/sudoers.d/builder
    install -d -o builder /home/builder/publish /home/builder/test
    cp /out/publish/PKGBUILD /home/builder/publish/
    cp /out/test/PKGBUILD /home/builder/test/
    chown -R builder /home/builder
    su builder -c "cd /home/builder/publish && makepkg --printsrcinfo > .SRCINFO"
    cp /home/builder/publish/.SRCINFO /out/publish/
    su builder -c "cd /home/builder/test && makepkg -si --noconfirm"
    got=$(theca --version)
    echo "$got"
    [ "$got" = "theca '"$version"'" ] || { echo "theca --version: $got, not theca '"$version"'" >&2; exit 1; }
    # What only apt uses isn'"'"'t left on an Arch machine
    if [ -e /etc/apt ] || [ -e /usr/share/keyrings/theca.gpg ]; then echo "apt'"'"'s files are installed" >&2; exit 1; fi
    # Removed with the package, nothing of it left
    pacman -R --noconfirm theca-bin >/dev/null
    if command -v theca >/dev/null; then echo "theca is left after pacman -R" >&2; exit 1; fi
    chown -R '"$(id -u):$(id -g)"' /out
'
