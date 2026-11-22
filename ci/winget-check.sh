#!/usr/bin/env bash
# The winget manifests ci/winget.sh writes, checked against Microsoft's JSON
# schemas for their version (microsoft/winget-cli, at a pinned commit, each
# file's checksum pinned): GitHub's Windows image has no `winget`, so this is
# the check a Linux runner can make. check-jsonschema is installed in a
# virtual environment of its own, pinned.
#
#   ci/winget-check.sh <version> <dist> <out>
set -euo pipefail
cd "$(dirname "$0")/.."

[ $# = 3 ] || { sed -n '2,/^set /p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }
version=$1 dist=$2 out=$3
repository=${GITHUB_REPOSITORY:-theca-cloud/computer}
winget_cli=c09efb87d20b7b09a692aabfd1d55233733fea5f
schema_version=1.10.0
declare -A schema_sha=(
    [version]=330a3ece4ed602afb0c4f35290d14dec1e188e7b3c2fb330893ac3daac82b547
    [installer]=cca7bea1fbdaa18a3d5231de7f9f896b1dd41283c33a03a0565771861d393b31
    [defaultLocale]=77e406d25b35f908f70fb893335155e9e8a83aa59a6f3e9f959692e995e51e3c
)

rm -rf "$out" && mkdir -p "$out"
out=$(cd "$out" && pwd)
folder=$(ci/winget.sh "$version" "$dist" "https://github.com/$repository/releases/download/$version" "$out/files")

python3 -m venv "$out/venv"
"$out/venv/bin/pip" -q install 'check-jsonschema==0.33.3'
for kind in version installer defaultLocale; do
    schema=$out/manifest.$kind.$schema_version.json
    curl -fsSL --retry 3 -o "$schema" \
        "https://raw.githubusercontent.com/microsoft/winget-cli/$winget_cli/schemas/JSON/manifests/v$schema_version/manifest.$kind.$schema_version.json"
    echo "${schema_sha[$kind]}  $schema" | sha256sum -c --quiet -
done

check() { # <manifest> <kind>
    "$out/venv/bin/check-jsonschema" --schemafile "$out/manifest.$2.$schema_version.json" "$1"
    # winget-pkgs' validation refuses a manifest whose first line isn't this
    # header (SchemaHeaderNotFound), which no JSON schema checks.
    local header="# yaml-language-server: \$schema=https://aka.ms/winget-manifest.$2.$schema_version.schema.json"
    [ "$(head -n 1 "$1")" = "$header" ] || { echo "$1: its first line isn't $header" >&2; exit 1; }
}
check "$folder/Theca.Theca.yaml" version
check "$folder/Theca.Theca.installer.yaml" installer
check "$folder/Theca.Theca.locale.en-US.yaml" defaultLocale
echo "checked: winget's manifests of Theca.Theca $version are valid ($schema_version)"
