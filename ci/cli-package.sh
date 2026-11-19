#!/usr/bin/env bash
# theca-cli, the command line for apt and dnf: the CLI built without
# self-update (what a package manager installs, apt and dnf update it),
# packaged by nfpm (pinned, its checksum checked) with the unit, the scripts
# and the repository's files of ci/packages/nfpm.yaml. Run by ci/package.sh
# for the release, and by scripts/ci.sh service for the systemd check.
#
#   ci/cli-package.sh <version> <x86_64|arm64> <out folder> <file name> <deb|rpm>...
#
# Each format is written to <out folder>/<file name>.<format>.
set -euo pipefail
cd "$(dirname "$0")/.."

version=$1 arch=$2 out=$3 name=$4
shift 4
case $arch in
x86_64) nfpm_arch=amd64 nfpm_sha=6d7f9a28b2f397934403950fcbba83f598b08d02edbe79a0815b3b4897341750 ;;
arm64) nfpm_arch=arm64 nfpm_sha=a9a75213763783b7669719e505474c748c5ad7dc66591a195619deb50f85c2f4 ;;
*) echo "unknown architecture $arch" >&2; exit 2 ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cargo build --release --target-dir target/cli-pkg
# apt's Signed-By wants the binary keyring.
gpg --batch --dearmor <ci/packages.asc >target/cli-pkg/theca.gpg
curl -fsSL --retry 3 -o "$tmp/nfpm.tar.gz" \
    "https://github.com/goreleaser/nfpm/releases/download/v2.48.0/nfpm_2.48.0_Linux_$arch.tar.gz"
echo "$nfpm_sha  $tmp/nfpm.tar.gz" | sha256sum -c --quiet -
mkdir "$tmp/nfpm" && tar -xzf "$tmp/nfpm.tar.gz" -C "$tmp/nfpm" nfpm
mkdir -p "$out"
for format in "$@"; do
    NFPM_VERSION=$version NFPM_ARCH=$nfpm_arch "$tmp/nfpm/nfpm" package -f ci/packages/nfpm.yaml \
        -p "$format" -t "$out/$name.$format" >/dev/null
done
