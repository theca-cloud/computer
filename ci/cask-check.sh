#!/usr/bin/env bash
# The cask ci/cask.sh writes, installed by Homebrew on a Mac (a runner's) from
# a tap of its own, the .dmg files coming from <dist>: `theca --version` from
# the PATH prints the run's version, and uninstalling takes the link away.
#
#   ci/cask-check.sh <version> <dist> <out>
#
# The app is installed in <out>/Applications, never /Applications. A run by hand
# builds unsigned apps: Gatekeeper would refuse a quarantined one, so its
# quarantine is cleared before it is run (a tag's, notarized, needs none; the
# signing is checked by ci/package.sh).
set -euo pipefail
cd "$(dirname "$0")/.."

[ $# = 3 ] || { sed -n '2,/^set /p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }
version=$1 dist=$(cd "$2" && pwd) out=$3
rm -rf "$out" && mkdir -p "$out/Applications"
out=$(cd "$out" && pwd)
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_FROM_API=1 HOMEBREW_NO_ANALYTICS=1

ci/cask.sh "$version" "$dist" "file://$dist" "$out/tap"

brew tap-new --no-git theca-test/tap >/dev/null
tap=$(brew --repository theca-test/tap)
mkdir -p "$tap/Casks"
cp "$out/tap/Casks/theca.rb" "$tap/Casks/"
brew install --cask --appdir="$out/Applications" theca-test/tap/theca
xattr -cr "$out/Applications/Theca.app"
link=$(brew --prefix)/bin/theca
[ -L "$link" ] || { echo "no $link: the cask linked no \`theca\`" >&2; exit 1; }
got=$("$link" --version)
[ "$got" = "theca $version" ] || { echo "theca --version: '$got', not 'theca $version'" >&2; exit 1; }
echo "checked: brew installed the cask, $link prints $got"
brew uninstall --cask theca-test/tap/theca
# -L, not -e: a link left behind dangles once the app is gone, and -e follows it.
[ ! -L "$link" ] && [ ! -e "$out/Applications/Theca.app" ] || { echo "uninstalling left the link or the app" >&2; exit 1; }
brew untap theca-test/tap >/dev/null
echo "checked: uninstalling removes the link and the app"
