# CI for `computer`: Woodpecker for Linux, GitHub for the rest

`computer` is tested on four platforms and packaged on five (ADR 0005):

| Where | What | When |
|---|---|---|
| Woodpecker, `.woodpecker/linux.yaml` (the CI server's Docker agents) | `scripts/ci.sh` on Linux amd64: deny, lint, the tests, the app driven through its window | every push, any branch |
| the same workflow, its `mirror` step | `dev` force-pushed to `theca-cloud/computer` on GitHub | on `dev`, once those checks passed |
| GitHub, `.github/workflows/test.yml` | `scripts/ci.sh lint` and `test` on `macos-15`, `windows-2025`, `ubuntu-22.04-arm` | every `dev` reaching GitHub; by hand |
| Woodpecker, `.woodpecker/release.yaml` | `prod` pushed as GitHub's `main`, with the version tag | the tag `scripts/release.sh` creates, once `linux.yaml` passed on it |
| GitHub, `.github/workflows/release.yml` | the tests (`test.yml`), then the packages on five runners (`ci/package.sh`), then the GitHub release | that tag; by hand, without releasing |

Feature branches never reach GitHub. GitHub's standard runners are free for public
repositories. Every test builds and runs its own server from the pinned `core`, on every
platform: the tests' Postgres is a Woodpecker service on Linux, `scripts/dev.sh` on GitHub's
Linux, and a native one on macOS and Windows (`ci/postgres.sh`: they have no Docker).

**Drives are tested where each OS's backend is** (`THECA_TEST_DRIVE=1` makes a missing
one a failure, not a skip): FUSE on GitHub's Linux runner (`fuse3` installed) and on a
developer's Linux, Cloud Files on Windows, and on macOS a minimal `Theca.app` with the drive's
extension, built and installed before the tests (`ci/mac-app.sh`), its domain switched on by
script. Woodpecker's image has no FUSE: the drive's scenarios skip there. A failed or killed
drive test can't hang its machine: a Linux drive's engine is a child process of the test,
ended with it (`tests/common/child.sh`, theca/computer#1), and `scripts/ci.sh test` runs each
`cargo test` under `timeout` (`THECA_TEST_TIMEOUT`, 20 minutes), GitHub's job within 45
minutes. The packages check what each OS's drive
needs: the `.deb` depends on `fuse3`, the Windows uninstaller removes a drive, the `.dmg`'s
app carries its extension, signed and still sandboxed.

Before a release, a developer's Mac runs `ci/mac/drive-check.sh <the .dmg>`: no runner
quarantines what it builds. It sets up what it can itself (a fresh local server,
`ci/mac/server.sh` with CE's compose and its data in `ci/mac/data/`, on `THECA_PORT`, 8080 by default; its admin; a test
library and its files; the app's login with an app password; the app copied in, its
quarantine removed), and asks only what a person does: the drive's switch, Finder's two
actions. `--as-downloaded` installs it as a user gets it instead (quarantined, dragged
into Applications, opened with no Open Anyway, Gatekeeper's verdict checked): once before a
release, with a signed `.dmg`. Each step is PASS or FAIL in `summary.txt`; it stops at the first
failure the rest depends on. For a quicker turn than GitHub's queue, the same Mac runs
`ci/mac/ci.sh` from a git bundle of the branch: the lint and the tests as the macOS job does
(`--drive`: the macOS drive's test, which replaces `/Applications/Theca.app` with a test app,
removes every Theca drive from the Mac and restarts `fileproviderd`: not on a Mac whose drives
are in use),
or `ci/mac/ci.sh dmg`, the `.dmg` as `release.yml` builds it.

Watching GitHub: `gh run list -R theca-cloud/computer` (the latest runs),
`gh run view -R theca-cloud/computer <id> --log-failed` (what failed). `scripts/release.sh`
refuses to release while the latest `test` run on `dev` is red.

## 1. The CI image (Woodpecker)

`linux.yaml` runs in `git.pband.ch/theca/ci-computer:<rust>`: Rust, Node, WebKitGTK and the
app's test tools (`ci/Dockerfile`). After each Rust bump, from a machine logged in to the
registry:

```sh
docker login git.pband.ch          # a Gitea token with package write rights
scripts/ci-image.sh                # amd64, pushed
```

GitHub's runners install Tauri's prerequisites on each run and follow `rust-toolchain.toml`
by themselves (rustup).

## 2. The GitHub mirror's key, once

The mirror steps push with a deploy key of `theca-cloud/computer`'s own, write access to
that repository only:

```sh
ssh-keygen -t ed25519 -N '' -C woodpecker-computer -f /tmp/computer-mirror
# Or on GitHub: theca-cloud/computer → Settings → Deploy keys, "Allow write access".
gh repo deploy-key add /tmp/computer-mirror.pub -R theca-cloud/computer --allow-write --title woodpecker
woodpecker-cli repo secret add --repository theca/computer --name github_deploy_key \
  --value "$(cat /tmp/computer-mirror)" --event push --event tag
rm /tmp/computer-mirror /tmp/computer-mirror.pub
```

GitHub's release job needs no other secret: its own `GITHUB_TOKEN` creates the release.

## 3. The signing secrets

Every key that signs a package is a secret of `theca-cloud/computer`'s **`signing`
environment**, none a repository secret: GitHub hands them only to a job naming the
environment, and the environment's rule lets only version tags (`*.*.*`) name it, so a branch
can't reach them, even by editing a workflow. `release.yml`'s `package` job names it on a
pushed version tag only (anywhere else, a manual run included, `''`: no environment, unsigned packages), and its
packaging step passes each OS's credentials to that OS's runners only. On a tag,
`ci/package.sh` refuses to build without them, or with a part of them. Each has an offline
copy in the developer's password manager: a secret can't be read back. Set from the
developer's machine, never through a session:

```sh
gh secret set <NAME> --env signing -R theca-cloud/computer   # prompts; < file for a file
```

Without them, by hand, `ci/package.sh` builds ad hoc and unsigned (a throwaway updater key),
and says so. Local builds are ad hoc; `ci/update-check.sh`'s A and B are never signed by the
developer.

**macOS: Apple's Developer ID**, enrolled as an individual. The `.app` and its drive's
extension are signed and the `.app` notarized by Tauri and `ci/mac-app.sh`, the `.dmg`
notarized by `ci/package.sh`, every ticket stapled and checked (`spctl`, `stapler`):
`APPLE_CERTIFICATE` (the Developer ID Application certificate and its key, exported from
Keychain Access as a `.p12`, base64: `base64 -i <file>.p12 | gh secret set ...`),
`APPLE_CERTIFICATE_PASSWORD` (the `.p12`'s), `APPLE_API_KEY`, `APPLE_API_ISSUER` and
`APPLE_API_PRIVATE_KEY` (an App Store Connect API key with the Developer role, Users and
Access → Integrations: its ID, its issuer, its `.p8`, downloadable once), and the
environment's variable `APPLE_SIGNING_IDENTITY` ("Developer ID Application: <name> (<team>)").
On the developer's Mac, `ci/mac/ci.sh dmg` signs with the identity in their keychain
(`APPLE_SIGNING_IDENTITY` and the API key exported, no certificate).

**Windows: Certum's Open Source Code Signing in the cloud** (a year, at most 459 days; its
RSA 4096 key generated in SimplySign, where it stays). `ssign` (installed by `release.yml`,
pinned) signs against it over HTTPS: Tauri calls it on `theca.exe`, NSIS's plugins, the
uninstaller and the installer (`bundle.windows.signCommand`), `ci/package.sh` on the CLI
`theca.exe`; `signtool verify /pa` checks each, by "Open Source Developer", timestamped.
`CERTUM_EMAIL` (the SimplySign login) and `CERTUM_OTP` (the `otpauth://` URI of SimplySign's
QR code, read from a screenshot with `zbarimg -q --raw` before the phone scans it): with the
email, the seed signs anything as the developer; leaked, a new QR (Certum's "regain access")
makes it useless. A login code works once: `ssign` keeps its session 20 minutes, for every
file of a build.

**Linux: the repositories' key** (`LINUX_SIGNING_KEY`): one GPG key, RSA 4096, no expiry (an
expired key would stop every machine that missed the update shipping a new one), signs the
`.rpm` files (`rpmsign`, in `ci/package.sh`) and the repositories' indexes
(`ci/repository.sh`). Its public half is `ci/packages.asc` (fingerprint
`B7AE400CDAA65388CEC6316D43CE4F136D665585`), which every package ships: apt's keyring is made
from it at build time, and `packages.theca.rs` serves it. Both scripts refuse, on a tag, a
`LINUX_SIGNING_KEY` that isn't that key. Made on the developer's Mac (the private key never
touches a workspace): `gpg --batch --passphrase '' --quick-generate-key "Theca packages
<packages@theca.rs>" rsa4096 sign never`, then `gpg --export-secret-keys --armor <fingerprint> |
gh secret set LINUX_SIGNING_KEY --env signing -R theca-cloud/computer` and
`gpg --export --armor <fingerprint> > packages.asc` (committed). The revocation certificate
(`gpg --gen-revoke`) and an offline copy of the private key stay in the password manager;
leaked, the revocation and a new key go out in a package update. A run by hand signs with a
throwaway key (the real one signs releases only).

**The updater's key** (ADR 0006): the packages that update themselves (all but the `.deb`)
are signed with one minisign key, its public half `ci/updater.pub`, built into every one;
`TAURI_SIGNING_PRIVATE_KEY` is its private half, with an empty password (so no
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`: `release.yml` passes `''`). Lost, no install could
update again; leaked, it would sign updates for every install. Made once:
`npx @tauri-apps/cli signer generate -w ~/.tauri/theca-updater.key`, then
`gh secret set TAURI_SIGNING_PRIVATE_KEY --env signing -R theca-cloud/computer < ~/.tauri/theca-updater.key`.

`ci/package.sh` signs each package (a `.sig` beside it; without the secret, by hand, a
throwaway key, never on a tag), the `manifest` job writes `latest.json` from them
(`ci/manifest.py`, on every run, kept with it) and the release attaches it. Every run also
checks the update itself, without a release: `ci/update-check.sh` builds A and B (B's
version bumped) with a key made for the run, installs A and updates it to B through a
manifest it serves, after seeing A refuse B signed by another key and stay A.

## 4. The repositories and the channels

**`packages.theca.rs`** (apt at `/deb`, rpm at `/rpm`, on `chapter.li`): `release.yml`'s
`repository` job runs `ci/repository.sh`, then `ci/repository-check.sh`, on every run. The
index lists the `.deb` and `.rpm` files of the release and, on a tag, of the earlier releases
that carry a `repository.tar.gz`, newest first while they fit in 5 GiB; nothing is rebuilt (the
packages are the releases' own). A run by hand signs with a throwaway key and lists its own
packages only. `repository.tar.gz` (attached to the release, so in its `SHA256SUMS`) holds
`deb/dists/stable/…`, `rpm/<x86_64|aarch64>/repodata/…`, the four files a user fetches to add
the repository (`theca.sources`, `theca.gpg`, `theca.repo`, `RPM-GPG-KEY-theca`) and
`files.txt`: one line a package, `<release> <asset name> <path in the tree>` (`deb/pool/<release>/…`,
`rpm/<arch>/<release>/…`); no package. After a release, **by hand on `chapter.li`**:
`theca-packages/pull.sh <version>` (its repository's README): it downloads the packages from
GitHub, checks each against its release's `SHA256SUMS`, and swaps the served tree in one
rename. The check installs from the repositories in Debian 12, Ubuntu 24.04 and Fedora (apt;
dnf, where the two packages are told to replace each other with `--allowerasing`), refuses a
repository signed by another key and a package that isn't the one the index lists.

**The daemon as a service**, on every run. Linux: `release.yml`'s `service-check` job runs
`scripts/ci.sh service` on this run's `theca-cli` `.deb` (`tests/service.rs`): a Debian 12
booted with systemd (a privileged container; the runner's Docker allows it, assumed until a run
shows it) installs it, enables `theca@tester` against the test server, and the unit runs as the
user and syncs both ways, an upgrade restarts it, a second `theca run` is refused, one refused
beside another process starts once that one ends, and removing the package stops it. By hand
here: `scripts/ci.sh service` builds the package (`ci/cli-package.sh`, which `ci/package.sh`
uses too) and needs Docker and the test Postgres; `THECA_TEST_SERVICE_IMAGE=debian:13` is the
base for a package built on a newer system than Ubuntu 22.04 (its glibc is newer than Debian
12's). Windows: `test.yml`'s Windows run sets `THECA_TEST_WINDOWS_SERVICE`, so
`tests/service_windows.rs` makes a local user (`net user`) and registers `theca-<user>` for it
with `theca service on`, then removes both: the runner is an administrator. Not covered: a
self-update under the service, and the app password in Credential Manager (the test keeps it
in the state file).

**The installed app in pictures** (theca/computer#96), on every run: `release.yml`'s `screenshots` job
installs each OS's package from this run (Windows, macOS arm64, Linux x86_64: the `.exe`, the
`.dmg`, the `.deb`), logs in to the public demo (`demo.theca.rs`, the account theca.rs shows
everyone) with an app password made for the run and revoked after it, adds `Documents` and
`My files` as synced folders and `Photos` as a drive, waits until all three are up to date, and
takes `app.png` (the window), `folders.png` and `drive.png` (the file manager) with
`ci/screenshots.sh`. Windows' screen is set to 1920×1080 (`ci/screenshots.ps1`), Linux gets a
desktop (Xvfb and Xfce: the panel's tray and Thunar; not what most users run), macOS keeps its
runner's 1024×768 and has its drive switched on the way `tests/mac_drive.rs` does. They stay on
the run, as the artifacts `screenshots-<os>-<arch>`: the GitHub release, `manifest`, `repository` and
`service-check` take the package artifacts by name (`THECA_PACKAGES`, `release.yml`'s `env`, to
extend with the matrix), never by a glob, so neither a picture nor anything else the job uploads
(it opens the public demo's files in a file manager) can end up in a release or in the signed
repository's index. No job needs `screenshots`: a red one (the demo down) holds nothing back. Nothing compares the pictures: CLAUDE.md has the agent look at
them after a release. By hand: `gh workflow run release.yml --ref <branch>`; `ci/screenshots.sh <os>
<arch>` runs on a machine of that OS with the package in `dist/` (it installs the app and revokes
its app password on exit).

**The channels** `release.yml` doesn't host: on every run `cask-check`, `aur-check` and
`winget-check` build the file and check it (`ci/cask.sh`, `ci/aur.sh`, `ci/winget.sh`: a cask
installed by `brew` on a Mac runner, `theca-bin` by `makepkg` in Arch's container, winget's
manifests against Microsoft's schemas), and on a tag, once the release exists, `tap`, `winget`
and `store` publish, each its own job, run again alone when it fails. `theca-bin` isn't
published: the AUR paused new registrations (theca/computer#69), its check keeps the PKGBUILD
working meanwhile. Their secrets
are in the **`channels`** environment (version tags only, like `signing`), never the signing
key: these jobs run other people's code, which sees their environment's secrets. Set from the
developer's machine, `gh secret set <NAME> --env channels -R theca-cloud/computer`:

- `TAP_TOKEN`: a fine-grained token, `contents: write` on `theca-cloud/homebrew-tap` only. It
  expires (made 2026-11-18, valid until **2027-11-19**): replaced before, or the tap job fails.
- `WINGET_TOKEN`: a classic token (`public_repo`, and `workflow` to sync the fork) of a GitHub
  machine account (`theca-winget`) that owns a fork of `microsoft/winget-pkgs` and nothing
  else, no right on `theca-cloud`: the developer's own token would reach every repository they
  can write to. It expires (made 2026-11-18, valid until **2027-11-18**): replaced before.
  komac (pinned) opens the pull request; it needs a version already in winget-pkgs, so a
  package's first one is submitted by hand with the run's `winget-manifests` artifact.
- `STORE_TENANT_ID`, `STORE_CLIENT_ID`, `STORE_CLIENT_SECRET`, `STORE_SELLER_ID`,
  `STORE_PRODUCT_ID`: the Azure AD application of the developer's Partner Center account
  (Account settings, Manage users, Azure AD applications, role Manager) and the product's ID.
  The `store` job (`msstore`, Microsoft's CLI) runs once a first submission exists, made by hand in
  Partner Center: the repository variable `STORE_LISTED` is then set to `true`. The Store
  refuses an installer URL that redirects, as GitHub's release assets all do: it takes
  `https://packages.theca.rs/windows/<version>/theca-<version>-windows-x86_64.exe`, served once
  `pull.sh <version>` ran on `chapter.li` (which keeps the previous release's too). The job
  waits up to three hours for that URL to answer 200, then fails: run `pull.sh`, then re-run
  the job alone.

## 5. Keeping it up

- **Rust bumped**: the CI image (step 1), then its tag in `.woodpecker/linux.yaml`.
- **The actions** are pinned to exact versions (`actions/*`, `Swatinem/rust-cache`): bumped
  by hand, all at once.
- **Postgres on Windows**: EDB's zip, by its full version, in `ci/postgres.sh`.
- **A runner retired by GitHub** (`macos-15-intel`, `ubuntu-22.04` one day): the next one in
  the workflows; Linux packages stay on the oldest Ubuntu available (its glibc is the
  oldest they run on).
