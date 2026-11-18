#!/usr/bin/env bash
# The apt and rpm repositories (packages.theca.rs: /deb and /rpm), indexed and
# signed from the .deb and .rpm files of Theca's releases. No package is built
# here: the packages are the releases' own, and the index lists them.
#
#   ci/repository.sh <version> <dist> <out>
#       <dist>: this run's packages, theca[-cli]-<version>-linux-<arch>.{deb,rpm}.
#       <out>: made; holds tree/ (the whole repository, packages included),
#       repository.tar.gz (the same without the packages: what a release
#       attaches, and `chapter.li`'s pull.sh reads) and signer.asc (the public
#       key that signed).
#   ci/repository.sh --resign <tree>
#       signs the tree's indexes again with the first secret key of $GNUPGHOME.
#   ci/repository.sh --select <bytes>
#       the releases that fit: "<tag> <bytes>" lines on stdin, newest first,
#       tags printed while their sizes add up to <bytes> at most.
#
# On a tag (GITHUB_REF_TYPE=tag) the key is LINUX_SIGNING_KEY, which must be
# ci/packages.asc's, and the repositories keep the earlier releases' packages
# too, newest first while they fit in KEEP_BYTES (5 GiB), downloaded from the
# repository's releases (GITHUB_REPOSITORY; only those that carry a
# repository.tar.gz, so made by this). Otherwise (a run by hand) a throwaway key
# signs everything, the .rpm files included (their own signature is replaced),
# and only this run's packages are indexed: the real key signs releases only.
#
# repository.tar.gz: the folders `deb/` (dists/stable/..., InRelease) and
# `rpm/<arch>/repodata/`, and files.txt: "<release> <asset name> <path of the
# package in the tree>", one a line, where pull.sh puts each package.
set -euo pipefail
cd "$(dirname "$0")/.."

keep_bytes=${KEEP_BYTES:-$((5 * 1024 * 1024 * 1024))}
apt_arches="amd64 arm64"

select_releases() {
    local budget=$1 total=0 tag bytes
    while read -r tag bytes; do
        total=$((total + bytes))
        [ "$total" -le "$budget" ] || break
        echo "$tag"
    done
}

gpg_sign() { gpg --batch --yes --quiet --pinentry-mode loopback --passphrase '' -u "$fpr" "$@"; }
first_secret() { gpg --batch --with-colons --list-secret-keys | awk -F: '/^fpr/ { print $10; exit }'; }

# Each signature is written beside, then moved over: a tree made of hard links
# (ci/repository-check.sh's copies) keeps the other trees' files as they were.
# Release, InRelease and Release.gpg of a dists/<suite> folder.
sign_apt() {
    local suite=$1
    gpg_sign --clearsign -o "$suite/InRelease.new" "$suite/Release"
    mv -f "$suite/InRelease.new" "$suite/InRelease"
    gpg_sign --detach-sign --armor -o "$suite/Release.gpg.new" "$suite/Release"
    mv -f "$suite/Release.gpg.new" "$suite/Release.gpg"
}
sign_rpm() {
    gpg_sign --detach-sign --armor -o "$1/repomd.xml.asc.new" "$1/repomd.xml"
    mv -f "$1/repomd.xml.asc.new" "$1/repomd.xml.asc"
}

resign() {
    local tree=$1
    fpr=$(first_secret)
    sign_apt "$tree/deb/dists/stable"
    local repodata
    for repodata in "$tree"/rpm/*/repodata; do sign_rpm "$repodata"; done
}

case ${1:-} in
--select)
    select_releases "$2"
    exit
    ;;
--resign)
    resign "$2"
    exit
    ;;
esac

[ $# = 3 ] || { sed -n '2,/^set /p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }
version=$1 dist=$(cd "$2" && pwd) out=$3
rm -rf "$out" && mkdir -p "$out"
out=$(cd "$out" && pwd)
tree=$out/tree
mkdir -p "$tree/deb" "$tree/rpm"
: >"$tree/files.txt"

# The key: the release's on a tag, else a throwaway one.
export GNUPGHOME=$out/.gnupg
(umask 077 && mkdir "$GNUPGHOME")
trap 'GNUPGHOME=$out/.gnupg gpgconf --kill gpg-agent 2>/dev/null || true; rm -rf "$out/.gnupg"' EXIT
tag_run=false
[ "${GITHUB_REF_TYPE:-}" = tag ] && tag_run=true
if $tag_run; then
    [ -n "${LINUX_SIGNING_KEY:-}" ] || { echo "no LINUX_SIGNING_KEY: a release's repositories are signed by the developer" >&2; exit 1; }
    printf '%s\n' "$LINUX_SIGNING_KEY" | gpg --batch --quiet --import
    fpr=$(first_secret)
    shipped=$(gpg --batch --with-colons --show-keys ci/packages.asc | awk -F: '/^fpr/ { print $10; exit }')
    [ "$fpr" = "$shipped" ] ||
        { echo "LINUX_SIGNING_KEY ($fpr) isn't the key the packages ship, ci/packages.asc ($shipped)" >&2; exit 1; }
    cp ci/packages.asc "$out/signer.asc"
else
    gpg --batch --quiet --pinentry-mode loopback --passphrase '' \
        --quick-generate-key "Theca packages, a run's throwaway key <throwaway@invalid>" rsa4096 sign 1d
    fpr=$(first_secret)
    gpg --batch --export --armor "$fpr" >"$out/signer.asc"
    echo "no release key: the repositories are signed with a throwaway key (a run by hand)" >&2
fi

# A package of <release> into the tree, and its line in files.txt.
add_package() { # release file
    local release=$1 file=$2 name arch dir
    name=$(basename "$file")
    case $name in
    *.deb) dir=deb/pool/$release ;;
    *.rpm)
        case $name in
        *-linux-x86_64.rpm) arch=x86_64 ;;
        *-linux-arm64.rpm) arch=aarch64 ;;
        *) echo "$name: no architecture" >&2; exit 1 ;;
        esac
        dir=rpm/$arch/$release
        ;;
    *) echo "$name: neither a .deb nor an .rpm" >&2; exit 1 ;;
    esac
    mkdir -p "$tree/$dir"
    cp "$file" "$tree/$dir/$name"
    echo "$release $name $dir/$name" >>"$tree/files.txt"
}

shopt -s nullglob
this=("$dist"/theca-"$version"-linux-*.deb "$dist"/theca-"$version"-linux-*.rpm \
    "$dist"/theca-cli-"$version"-linux-*.deb "$dist"/theca-cli-"$version"-linux-*.rpm)
[ ${#this[@]} -gt 0 ] || { echo "$dist: no .deb or .rpm of $version" >&2; exit 1; }
used=0
for file in "${this[@]}"; do
    add_package "$version" "$file"
    used=$((used + $(stat -c %s "$file")))
done

# The earlier releases', on a tag: those with a repository.tar.gz, newest
# first, while everything fits.
if $tag_run; then
    : "${GITHUB_REPOSITORY:?}"
    listing=$(gh api "repos/$GITHUB_REPOSITORY/releases?per_page=100" --paginate --jq '
        .[] | select(.draft | not) | select(.assets | any(.name == "repository.tar.gz"))
        | [.tag_name, ([.assets[] | select(.name | test("\\.(deb|rpm)$")) | .size] | add // 0)] | @tsv')
    kept=$(sort -rV <<<"$listing" | awk -v v="$version" '$1 != v' | select_releases "$((keep_bytes - used))")
    for release in $kept; do
        mkdir -p "$out/older/$release"
        gh release download "$release" -R "$GITHUB_REPOSITORY" -p '*.deb' -p '*.rpm' -D "$out/older/$release"
        for file in "$out/older/$release"/*; do add_package "$release" "$file"; done
    done
    rm -rf "$out/older"
fi

# Every .rpm signed by this run's key, a throwaway's included: a release's are
# already signed by the real key, by package.sh.
if ! $tag_run; then
    for file in "$tree"/rpm/*/*/*.rpm; do
        rpmsign --delsign --define "__gpg $(command -v gpg)" "$file" >/dev/null 2>&1 || true
        rpmsign --addsign --define "__gpg $(command -v gpg)" --define "_gpg_name $fpr" \
            --define "_gpg_path $GNUPGHOME" "$file" >/dev/null
    done
fi

# apt: Packages per architecture (apt-ftparchive lists every .deb under pool/,
# split here by the Architecture each declares), then Release, signed.
suite=$tree/deb/dists/stable
(cd "$tree/deb" && apt-ftparchive packages pool >"$out/Packages.all" 2>/dev/null)
for arch in $apt_arches; do
    mkdir -p "$suite/main/binary-$arch"
    awk -v arch="$arch" 'BEGIN { RS = ""; ORS = "\n\n" } $0 ~ ("(^|\n)Architecture: " arch "(\n|$)")' \
        "$out/Packages.all" >"$suite/main/binary-$arch/Packages"
    gzip -9kf "$suite/main/binary-$arch/Packages"
done
rm "$out/Packages.all"
apt-ftparchive \
    -o APT::FTPArchive::Release::Origin=Theca -o APT::FTPArchive::Release::Label=Theca \
    -o APT::FTPArchive::Release::Suite=stable -o APT::FTPArchive::Release::Codename=stable \
    -o APT::FTPArchive::Release::Components=main -o "APT::FTPArchive::Release::Architectures=$apt_arches" \
    release "$suite" >"$out/Release.new"
mv "$out/Release.new" "$suite/Release"
sign_apt "$suite"

# rpm: one repository per architecture, its repomd.xml signed.
for dir in "$tree"/rpm/*/; do
    createrepo_c --quiet "$dir"
    sign_rpm "${dir}repodata"
done

# What a release attaches: the index and files.txt, no package.
tar -czf "$out/repository.tar.gz" -C "$tree" --exclude='*.deb' --exclude='*.rpm' .
echo "repositories: $(wc -l <"$tree/files.txt") packages, signed by $fpr; $out/repository.tar.gz"
