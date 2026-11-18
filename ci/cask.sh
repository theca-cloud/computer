#!/usr/bin/env bash
# Theca's Homebrew cask (theca-cloud/homebrew-tap, `brew install
# theca-cloud/tap/theca`), from the release's two .dmg files: the app, and its
# own `theca` linked into the PATH (a Mac's only CLI: theca/computer#30).
# `auto_updates true`: the app updates itself (ADR 0006), Homebrew leaves it.
#
#   ci/cask.sh <version> <dist> <base url> <out>
#
# <dist>: the folder holding theca-<version>-macos-{arm64,x86_64}.dmg (their
# checksums go into the cask). <base url>: where they are downloaded from, the
# release's (or file://<dist>, ci/cask-check.sh). <out>: made, holds Casks/theca.rb.
set -euo pipefail

[ $# = 4 ] || { sed -n '2,/^set /p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }
version=$1 dist=$2 base=${3%/} out=$4
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "not a release version: $version" >&2; exit 2; }
sum() {
    local file=$dist/theca-$version-macos-$1.dmg
    [ -f "$file" ] || { echo "$file: missing" >&2; exit 1; }
    # shasum: on every Mac, where this runs (sha256sum isn't).
    shasum -a 256 "$file" | cut -d' ' -f1
}
arm=$(sum arm64)
intel=$(sum x86_64)
# The release's folder is named by the version: the cask says #{version}.
url_base=${base//"$version"/'#{version}'}

mkdir -p "$out/Casks"
cat >"$out/Casks/theca.rb" <<CASK
cask "theca" do
  arch arm: "arm64", intel: "x86_64"

  version "$version"
  sha256 arm:   "$arm",
         intel: "$intel"

  url "$url_base/theca-#{version}-macos-#{arch}.dmg"
  name "Theca"
  desc "Folders kept identical to Theca libraries, or libraries as drives"
  homepage "https://theca.rs/computer"

  # The app updates itself.
  auto_updates true
  depends_on macos: ">= :ventura"

  app "Theca.app"
  # \`theca\` on the PATH: the app's own, a Mac has no other CLI.
  binary "#{appdir}/Theca.app/Contents/MacOS/theca"
end
CASK
echo "$out/Casks/theca.rb"
