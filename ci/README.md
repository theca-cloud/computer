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
one a failure, not a skip): FUSE on GitHub's Linux runner (`fuse3` installed), Cloud
Files on Windows, and on macOS a minimal `Theca.app` with the drive's extension, built and
installed before the tests (`ci/mac-app.sh`), its domain switched on by script. Woodpecker's
image has no FUSE: the drive's scenarios skip there. The packages check what each OS's drive
needs: the `.deb` depends on `fuse3`, the Windows uninstaller removes a drive, the `.dmg`'s
app carries its extension, signed and still sandboxed. Before a release, a developer's Mac
runs `ci/mac-drive-check.sh` (a download quarantined, opened through Gatekeeper, a drive
added and switched on): no runner quarantines what it builds.

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

GitHub's release job needs no secret: its own `GITHUB_TOKEN` creates the release.

## 3. Keeping it up

- **Rust bumped**: the CI image (step 1), then its tag in `.woodpecker/linux.yaml`.
- **The actions** are pinned to exact versions (`actions/*`, `Swatinem/rust-cache`): bumped
  by hand, all at once.
- **Postgres on Windows**: EDB's zip, by its full version, in `ci/postgres.sh`.
- **A runner retired by GitHub** (`macos-15-intel`, `ubuntu-22.04` one day): the next one in
  the workflows; Linux packages stay on the oldest Ubuntu available (its glibc is the
  oldest they run on).
