#!/usr/bin/env python3
"""A release's latest.json (ADR 0006), from every runner's packages in <dist>
(ci/package.sh's) and their .sig files: Tauri's updater format, plus each
headless archive as <os>-<arch>-headless, and min_server (src/ops.rs's
MIN_SERVER). Every installed client reads the newest one: a published field is
never renamed nor removed. Printed on stdout:

  ci/manifest.py <version> <the release's download URL> <dist> [<notes file>]
"""

import json
import pathlib
import re
import sys
from datetime import datetime, timezone

# Tauri's names for the OSes and architectures package.sh names its files with.
OS = {"macos": "darwin", "windows": "windows", "linux": "linux"}
ARCH = {"arm64": "aarch64", "x86_64": "x86_64"}
# The packages that update themselves, by what their file name ends with: the
# app's (the .app.tar.gz, the NSIS installer, the .AppImage) and headless'.
APP = {"app.tar.gz", "exe", "AppImage"}
HEADLESS = {"tar.gz", "zip"}


def main():
    if len(sys.argv) not in (4, 5):
        sys.exit(__doc__.strip().splitlines()[-1].strip())
    version, base, dist = sys.argv[1], sys.argv[2].rstrip("/"), pathlib.Path(sys.argv[3])
    notes = pathlib.Path(sys.argv[4]).read_text() if len(sys.argv) == 5 else ""
    ops = (pathlib.Path(__file__).resolve().parent.parent / "src" / "ops.rs").read_text()
    found = re.search(r'MIN_SERVER: \(&str, &str\) = \("([^"]+)"', ops)
    if not found:
        sys.exit("src/ops.rs: no MIN_SERVER")
    package_name = re.compile(
        rf"theca-(headless-)?{re.escape(version)}-(macos|windows|linux)-(arm64|x86_64)\.(.+)"
    )
    platforms = {}
    for sig in sorted(dist.glob("*.sig")):
        package = sig.name[: -len(".sig")]
        parts = package_name.fullmatch(package)
        if not parts:
            sys.exit(f"{sig.name}: not the signature of a {version} package")
        headless, os_name, arch, kind = parts.groups()
        if headless and kind in HEADLESS:
            key = f"{OS[os_name]}-{ARCH[arch]}-headless"
        elif not headless and kind in APP:
            key = f"{OS[os_name]}-{ARCH[arch]}"
        else:
            sys.exit(f"{sig.name}: not a package that updates itself")
        if not (dist / package).is_file():
            sys.exit(f"{sig.name}: no {package} beside it")
        if key in platforms:
            sys.exit(f"{sig.name}: a second package for {key}")
        platforms[key] = {"url": f"{base}/{package}", "signature": sig.read_text().strip()}
    if not platforms:
        sys.exit(f"{dist}: no signed package")
    manifest = {
        "version": version,
        "notes": notes.strip(),
        "pub_date": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "min_server": found.group(1),
        "platforms": platforms,
    }
    json.dump(manifest, sys.stdout, indent=2)
    print()


if __name__ == "__main__":
    main()
