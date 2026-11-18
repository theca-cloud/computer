#!/usr/bin/env bash
# The repositories ci/repository.sh built, used as a user would: served on
# localhost, then installed from in Debian 12, Ubuntu 24.04 and Fedora, with
# the key that signed them (a throwaway's, on a run by hand).
#
#   ci/repository-check.sh <repository.sh's out folder>
#
# In each: `theca` installs and prints this version, brings its repository's
# files, `theca-cli` replaces it and the other way round, and removing the
# package removes the files. A repository signed by another key is refused,
# and so is a package that isn't the one the index lists (a byte appended):
# nothing gets installed.
set -euo pipefail
cd "$(dirname "$0")/.."

out=$(cd "$1" && pwd)
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
fail() { echo "FAIL: $*" >&2; exit 1; }

# Which releases a repository keeps: the newest that fit, and no more.
kept=$(printf '1.4.0 100\n1.3.1 100\n1.3.0 100\n' | ci/repository.sh --select 250 | tr '\n' ' ')
[ "$kept" = "1.4.0 1.3.1 " ] || fail "250 bytes keep '$kept', not the two newest"
over=$(printf '1.3.0 100\n' | ci/repository.sh --select 50) || fail "--select 50 failed" # empty on a crash too
[ -z "$over" ] || fail "a release over the budget was kept"
[ "$(printf '1.4.0 100\n1.3.0 100\n' | ci/repository.sh --select 200 | wc -l)" = 2 ] || fail "releases that fit exactly weren't all kept"
echo "ok: the releases kept are the newest that fit"

work=$(mktemp -d -p "$out") # beside the tree: hard links need one filesystem
servers=()
cleanup() {
    for pid in "${servers[@]}"; do kill "$pid" 2>/dev/null || true; done
    rm -rf "$work"
}
trap cleanup EXIT

# Two more trees from the good one: its indexes signed by another key, and one
# .deb and one .rpm with a byte appended (copies: the good tree stays).
cp -al "$out/tree" "$work/other"
(
    export GNUPGHOME=$work/other-key
    (umask 077 && mkdir "$GNUPGHOME")
    gpg --batch --quiet --pinentry-mode loopback --passphrase '' \
        --quick-generate-key "another key <other@invalid>" rsa4096 sign 1d 2>/dev/null
    ci/repository.sh --resign "$work/other"
    gpgconf --kill gpg-agent 2>/dev/null || true
)
cp -al "$out/tree" "$work/tampered"
for file in "$work"/tampered/deb/pool/*/theca-"$version"-linux-x86_64.deb "$work"/tampered/rpm/x86_64/*/theca-"$version"-linux-x86_64.rpm; do
    cp "$file" "$work/copy" && printf x >>"$work/copy" && mv -f "$work/copy" "$file"
done

serve() { # folder -> its port
    local port
    port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
    (cd "$1" && exec python3 -m http.server "$port" --bind 127.0.0.1 >"$work/http-$port.log" 2>&1) &
    servers+=($!)
    for _ in $(seq 50); do curl -fsS -o /dev/null "http://127.0.0.1:$port/files.txt" 2>/dev/null && break; sleep 0.1; done
    echo "$port"
}
good=$(serve "$out/tree")
other=$(serve "$work/other")
tampered=$(serve "$work/tampered")

# At the root, for a user adding the repository by hand: what the packages
# install (the apt source and keyring, the rpm repository and key), served.
for pair in theca.sources:ci/packages/theca.sources theca.repo:ci/packages/theca.repo RPM-GPG-KEY-theca:ci/packages.asc; do
    curl -fsS "http://127.0.0.1:$good/${pair%%:*}" | cmp -s - "${pair#*:}" || fail "/${pair%%:*} isn't ${pair#*:}"
done
curl -fsS "http://127.0.0.1:$good/theca.gpg" | cmp -s - <(gpg --batch --dearmor <ci/packages.asc) ||
    fail "/theca.gpg isn't the keyring the packages ship"
echo "ok: the source, the repository and the key are served at the root"

# What each container runs, with the three repositories' URLs and the version.
cat >"$work/apt.sh" <<'APT'
set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get -qq update >/dev/null
apt-get -qq install -y gnupg ca-certificates >/dev/null
gpg --dearmor </check/signer.asc >/usr/share/keyrings/test.gpg
use() { echo "deb [signed-by=/usr/share/keyrings/test.gpg] http://127.0.0.1:$1/deb stable main" >/etc/apt/sources.list.d/test.list; }
installed() { dpkg -s "$1" >/dev/null 2>&1; }
say() { echo "ok: $*"; }
no() { echo "FAIL: $*" >&2; exit 1; }
has_files() { [ -e /etc/apt/sources.list.d/theca.sources ] && [ -e /usr/share/keyrings/theca.gpg ]; }

# Another key's signature: refused, nothing to install.
use "$OTHER"
if apt-get update >/dev/null 2>&1; then no "a repository signed by another key was accepted"; fi
if apt-get install -y theca >/dev/null 2>&1; then no "theca installed from a repository signed by another key"; fi
installed theca && no "theca is installed"
say "a repository signed by another key is refused"

# A package that isn't the one the index lists.
use "$TAMPERED"
apt-get update >/dev/null
if apt-get install -y theca >/dev/null 2>&1; then no "a tampered theca installed"; fi
installed theca && no "a tampered theca is installed"
say "a package that isn't the one the index lists is refused"
apt-get clean

use "$GOOD"
apt-get update >/dev/null
apt-get install -y theca >/dev/null
[ "$(theca --version)" = "theca $VERSION" ] || no "theca --version: $(theca --version)"
has_files || no "theca brought no repository files"
say "theca installs, prints $VERSION, brings its repository's files"
apt-get install -y theca-cli >/dev/null
installed theca && no "theca-cli didn't replace theca"
[ "$(theca --version)" = "theca $VERSION" ] || no "theca-cli --version: $(theca --version)"
has_files || no "theca-cli brought no repository files"
apt-get install -y theca >/dev/null
installed theca-cli && no "theca didn't replace theca-cli"
[ "$(theca --version)" = "theca $VERSION" ] || no "theca --version: $(theca --version)"
has_files || no "theca, back, brought no repository files" # there, before purging asserts they're gone
say "theca-cli replaces theca, and theca replaces theca-cli"
apt-get purge -y theca >/dev/null
[ ! -e /etc/apt/sources.list.d/theca.sources ] && [ ! -e /usr/share/keyrings/theca.gpg ] || no "purging theca left the repository files"
say "removing the package removes the repository files"
APT

cat >"$work/dnf.sh" <<'DNF'
set -eu
use() {
    cat >/etc/yum.repos.d/test.repo <<REPO
[test]
name=test
baseurl=http://127.0.0.1:$1/rpm/\$basearch
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=file:///check/signer.asc
REPO
}
say() { echo "ok: $*"; }
no() { echo "FAIL: $*" >&2; exit 1; }
# `extra` once theca is installed: the repository it brings is ours, enabled
# and not served here (dnf refuses an option for a repository it doesn't have).
extra=
dnf_test() { dnf -q -y $extra --enablerepo=test "$@"; }
has_files() { [ -e /etc/yum.repos.d/theca.repo ] && [ -e /etc/pki/rpm-gpg/RPM-GPG-KEY-theca ]; }

# dnf5 only warns on makecache ("repomd.xml GPG signature verification error")
# and drops the repository: what it offers is nothing.
use "$OTHER"
if dnf_test install theca >/dev/null 2>&1; then no "theca installed from a repository signed by another key"; fi
rpm -q theca >/dev/null 2>&1 && no "theca is installed"
say "a repository signed by another key is refused"

use "$TAMPERED"
if dnf_test install theca >/dev/null 2>&1; then no "a tampered theca installed"; fi
rpm -q theca >/dev/null 2>&1 && no "a tampered theca is installed"
say "a package that isn't the one the index lists is refused"

use "$GOOD"
dnf_test install theca >/dev/null
extra=--setopt=theca.enabled=0
[ "$(theca --version)" = "theca $VERSION" ] || no "theca --version: $(theca --version)"
has_files || no "theca brought no repository files"
say "theca installs, prints $VERSION, brings its repository's files"
# One command, /usr/bin/theca: a conflict, which dnf is told to resolve.
if dnf_test install theca-cli >/dev/null 2>&1; then no "theca-cli installed beside theca"; fi
rpm -q theca-cli >/dev/null 2>&1 && no "theca-cli is installed"
dnf_test install --allowerasing theca-cli >/dev/null
rpm -q theca >/dev/null 2>&1 && no "theca-cli didn't replace theca"
[ "$(theca --version)" = "theca $VERSION" ] || no "theca-cli --version: $(theca --version)"
has_files || no "theca-cli brought no repository files"
dnf_test install --allowerasing theca >/dev/null
rpm -q theca-cli >/dev/null 2>&1 && no "theca didn't replace theca-cli"
[ "$(theca --version)" = "theca $VERSION" ] || no "theca --version: $(theca --version)"
has_files || no "theca, back, brought no repository files" # there, before removing asserts they're gone
say "theca-cli replaces theca, and theca replaces theca-cli"
dnf -q -y remove theca >/dev/null
[ ! -e /etc/yum.repos.d/theca.repo ] && [ ! -e /etc/pki/rpm-gpg/RPM-GPG-KEY-theca ] || no "removing theca left the repository files"
say "removing the package removes the repository files"
DNF

mkdir "$work/check" && cp "$out/signer.asc" "$work/check/"
run() { # image script
    echo "== $1"
    docker run --rm --name "theca-repo-check-$$-${1//[:\/]/-}" --network host \
        -v "$work/check:/check:ro" -v "$work/$2:/run.sh:ro" \
        -e GOOD="$good" -e OTHER="$other" -e TAMPERED="$tampered" -e VERSION="$version" \
        "$1" bash /run.sh
}
# THECA_CHECK_IMAGES: fewer, for a check by hand on packages built against a
# newer glibc than Debian 12's (the runners build on Ubuntu 22.04).
images=${THECA_CHECK_IMAGES:-debian:12 ubuntu:24.04 fedora:43}
for image in $images; do
    case $image in
    fedora:*) run "$image" dnf.sh ;;
    *) run "$image" apt.sh ;;
    esac
done
echo "repositories checked: $images"
