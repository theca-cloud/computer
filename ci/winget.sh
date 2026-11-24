#!/usr/bin/env bash
# The winget manifests of Theca.Theca (the Windows Package Manager's community
# repository, microsoft/winget-pkgs), from the release's NSIS installer: the
# three files of one version, in the folder layout winget-pkgs wants. The
# first version of a package is submitted by hand with these (a pull request
# adding manifests/t/Theca/Theca/<version>/); the next ones are made by komac
# from the previous version's (release.yml's winget job).
#
#   ci/winget.sh <version> <dist> <base url> <out>
#
# <dist>: the folder holding theca-<version>-windows-x86_64.exe (its checksum
# goes into the manifest). <base url>: where it is downloaded from, the
# release's. <out>: made.
set -euo pipefail

[ $# = 4 ] || { sed -n '2,/^set /p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }
version=$1 dist=$2 base=${3%/} out=$4
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "not a release version: $version" >&2; exit 2; }
installer=$dist/theca-$version-windows-x86_64.exe
[ -f "$installer" ] || { echo "$installer: missing" >&2; exit 1; }
sha=$(sha256sum "$installer" | cut -d' ' -f1 | tr a-f A-F)

id=Theca.Theca
folder=$out/manifests/t/Theca/Theca/$version
mkdir -p "$folder"
manifest_version=1.10.0

cat >"$folder/$id.yaml" <<YAML
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.version.$manifest_version.schema.json
PackageIdentifier: $id
PackageVersion: $version
DefaultLocale: en-US
ManifestType: version
ManifestVersion: $manifest_version
YAML

# Tauri's NSIS installer, per user (no admin), silent with /S.
cat >"$folder/$id.installer.yaml" <<YAML
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.installer.$manifest_version.schema.json
PackageIdentifier: $id
PackageVersion: $version
InstallerType: nullsoft
Scope: user
InstallModes:
  - interactive
  - silent
  - silentWithProgress
InstallerSwitches:
  Silent: /S
  SilentWithProgress: /S
UpgradeBehavior: install
Installers:
  - Architecture: x64
    InstallerUrl: $base/theca-$version-windows-x86_64.exe
    InstallerSha256: $sha
ManifestType: installer
ManifestVersion: $manifest_version
YAML

publisher=$(sed -n 's/^ *"publisher": "\(.*\)",$/\1/p' "$(dirname "$0")/../tauri.conf.json" | head -1)
[ -n "$publisher" ] || { echo "tauri.conf.json has no bundle.publisher" >&2; exit 1; }
cat >"$folder/$id.locale.en-US.yaml" <<YAML
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.defaultLocale.$manifest_version.schema.json
PackageIdentifier: $id
PackageVersion: $version
PackageLocale: en-US
Publisher: $publisher
PublisherUrl: https://theca.rs
PackageName: Theca
PackageUrl: https://theca.rs/computer
License: AGPL-3.0-only
LicenseUrl: https://github.com/theca-cloud/computer/blob/main/LICENSE
ShortDescription: Folders kept identical to Theca libraries, or libraries as drives.
Moniker: theca
Tags:
  - cloud
  - files
  - sync
  - self-hosted
ReleaseNotesUrl: https://github.com/theca-cloud/computer/releases/tag/$version
ManifestType: defaultLocale
ManifestVersion: $manifest_version
YAML
echo "$folder"
