# CI for `computer`: agents and the shared test server

`computer` is tested on four platforms, one Woodpecker workflow each (`.woodpecker/`), every
push. A workflow runs on one agent, picked by its `platform` label: moving a platform to
rented hardware later is giving another agent that label.

| Workflow | Agent | Its end-to-end tests' server |
|---|---|---|
| `linux` (amd64) | the CI server's Docker agent (as for every repo) | one per test; Postgres as a Woodpecker service |
| `windows` | a Windows 11 VM on the CI server, a `local` agent inside | the **shared test server**, on the CI server |
| `macos` | the developer's Mac, a `local` agent, through an SSH tunnel | one per test; Postgres in OrbStack (`scripts/dev.sh`) |
| `linux-arm64` | the developer's Mac, a Docker agent in OrbStack, through the tunnel | one per test; Postgres as a Woodpecker service |

**Why a standing test server for Windows**: a workflow runs all its steps and services on
one agent, and the Windows agent's `local` backend has no services; the server is a Linux
program (on Windows it would test the server, not the app). So the Windows tests use a
server outside the pipeline, running the `core` that `computer` pins (`scripts/ci.sh`
fails saying so when it lags). They share its admin: each test creates an organization of
its own, and they run one at a time.

**The Mac's workflows wait while the Mac is away** (pending, not failed), and run when its
agents connect again.

Versions: Woodpecker **3.20.1** (the server's: agents must match it), `dockurr/windows`
**6.06**, Rust as in `rust-toolchain.toml`.

## 1. The CI image

The `linux` and `linux-arm64` workflows run in `git.pband.ch/theca/ci-computer:<rust>`:
Rust, Node, WebKitGTK and the app's test tools (`ci/Dockerfile`). After each Rust bump,
from a machine logged in to the registry:

```sh
docker login git.pband.ch          # a Gitea token with package write rights
scripts/ci-image.sh                # amd64 and arm64, pushed
```

## 2. The CI server: the Windows VM and the test server

They live in the server's infrastructure repository (`perso/infrastructure`,
`on-demand/ci.yml`), in the CI's Sablier group: started with the CI, stopped with it.

- `ci-windows`: `dockurr/windows`, on the `CI` network (it reaches `ci-server:9000` and the
  test server) and `WAN` (internet). Its password in `/data/docker/env/ci-windows` (the
  image reads it from no file). Viewer and RDP on the host's loopback only.
- `ci-theca-server` (`git.pband.ch/theca/core:<pin>`) and `ci-theca-db`, on `CI` only, run
  as the server's user so their secrets stay `0440` like the CI's others
  (`/data/docker/secrets/ci/theca/`).
- `ci-server` publishes its gRPC on the host's loopback (`127.0.0.1:9000`), for the Mac's
  tunnel.

**The test server is set up once**, through its API from a container on its network (its
admin's password then goes into Woodpecker, step 5):

```sh
password=$(openssl rand -base64 24); echo "ci@test.theca / $password"
docker run --rm --network on-demand_CI curlimages/curl:8.11.1 -fsS \
  -H 'Content-Type: application/json' http://ci-theca-server:8080/api/v1/setup -d '{
    "admin_email": "ci@test.theca", "admin_password": "'"$password"'",
    "organization": "CI",
    "storages": [{"name": "local", "implementation": "filesystem", "path": "/files"}]}'
```

**When `computer` pins a new `core`**: the image tag in `on-demand/ci.yml`, then
`docker compose … up -d --no-deps ci-theca-server` (it migrates at startup).

## 3. Windows, once

The first start downloads and installs Windows 11 by itself (20 to 40 minutes). Watch it
through a tunnel, `ssh -N -L 8006:localhost:8006 -L 3389:localhost:3389 <server>`, at
http://localhost:8006. Then Remote Desktop (Microsoft's *Windows App*) to `localhost`, user
`ci`, the password from `/data/docker/env/ci-windows`.

In an **administrator PowerShell**:

```powershell
# Git (its bash runs the workflow's steps), Rust with the MSVC build tools, Node.
# `--source winget`: the Store source's pinned certificate fails in this VM
# (0x8a15005e), and winget asks every source otherwise.
winget install --exact --id Git.Git --source winget --accept-source-agreements --accept-package-agreements
winget install --exact --id Rustlang.Rustup --source winget
winget install --exact --id Microsoft.VisualStudio.2022.BuildTools --source winget --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
winget install --exact --id OpenJS.NodeJS.LTS --source winget
# After the installs (Git's installer rewrites the PATH it read when it started):
# Git's bin, for bash.exe (it only adds Git\cmd).
[Environment]::SetEnvironmentVariable("Path", [Environment]::GetEnvironmentVariable("Path", "Machine") + ";C:\Program Files\Git\bin", "Machine")
# Long paths: Cargo's and npm's trees go deep.
New-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem" -Name LongPathsEnabled -Value 1 -PropertyType DWORD -Force
git config --system core.longpaths true
```

Sign out of Windows and back in (a session keeps the PATH it started with), then in a new
PowerShell, the tools and the network:

```powershell
bash --version; cargo --version; node --version
curl.exe -fsS http://ci-theca-server:8080/api/version
Test-NetConnection ci-server -Port 9000
```

## 4. The Windows agent

The same shared secret as the server's own agent (`/data/docker/secrets/ci/WP_SHARED`):
a system agent, which takes only the workflows asking for `platform=windows/amd64`. In an
administrator PowerShell:

```powershell
$dir = "C:\woodpecker"; New-Item -ItemType Directory -Force $dir | Out-Null
Invoke-WebRequest -OutFile "$dir\agent.zip" "https://github.com/woodpecker-ci/woodpecker/releases/download/v3.20.1/woodpecker-agent_windows_amd64.zip"
Expand-Archive -Force "$dir\agent.zip" $dir
# The agent reads .env in its working directory.
@"
WOODPECKER_SERVER=ci-server:9000
WOODPECKER_AGENT_SECRET=<WP_SHARED's content>
WOODPECKER_BACKEND=local
WOODPECKER_MAX_WORKFLOWS=1
WOODPECKER_AGENT_CONFIG_FILE=C:\woodpecker\agent.conf
"@ | Set-Content "$dir\.env"
# At the `ci` user's logon (the VM logs it in by itself), in its session: its PATH.
$action = New-ScheduledTaskAction -Execute "$dir\woodpecker-agent.exe" -WorkingDirectory $dir
Register-ScheduledTask -TaskName "Woodpecker agent" -Action $action -Trigger (New-ScheduledTaskTrigger -AtLogOn -User ci) -RunLevel Highest
Start-ScheduledTask -TaskName "Woodpecker agent"
```

It shows in Woodpecker's agents with `platform=windows/amd64`.

## 5. Woodpecker

`theca/computer`'s **Settings → Secrets**, for the `windows` workflow's tests:

- `theca_test_server_url`: `http://ci-theca-server:8080`
- `theca_test_admin_email`: `ci@test.theca`
- `theca_test_admin_password`: the password of step 2

**Settings → Agents** (the server's), once each agent has connected: a filter on each of
ours, so it takes only the workflows labelled for it.

| Agent | Filter |
|---|---|
| the Windows VM's | `!platform=windows/amd64` |
| the Mac's `local` one | `!platform=darwin/arm64` |
| the Mac's Docker one | `!platform=linux/arm64` |

They join with the server's shared secret, so they serve every repo, and a workflow without
labels runs on any agent: on a `local` one its `image:` is ignored and its commands run on
that machine (the Mac, as you). The `!` makes the label required; the server applies it,
whatever the agent reports. An agent keeps its ID across restarts (its config file), and its
filter with it.

## 6. The Mac

Prerequisites: Rust, Node, OrbStack (as for development), and Git LFS (`brew install
git-lfs`): on the `local` backend, Woodpecker's clone runs your `git`, and calls
`git lfs fetch`.

`ci/mac.sh <ssh host>`, whenever the Mac should run CI (in `screen`; Ctrl-C stops it):
it downloads the agent once, reads the shared secret over SSH into a `0600` file, and runs,
as you, the SSH tunnel to the server's loopback gRPC, the `macos` agent through it, and the
`linux-arm64` agent (a container in OrbStack, removed at the end). Nothing starts with the
Mac: workflows wait until you run it. When the agents give up (the server stopped while
idle, a network change), it starts them again. Files in
`~/Library/Application Support/theca-ci/`.

As you, not a user of their own: the `macos` agent runs what `scripts/ci.sh` runs when
you type it, on the same machine.

## 7. Keeping it up

- **Woodpecker upgraded**: the agents to the same version (the Windows zip, the Mac's).
- **Rust bumped**: `scripts/ci-image.sh`, then the image tag in `.woodpecker/linux*.yaml`;
  Windows and the Mac follow `rust-toolchain.toml` by themselves (rustup).
- **`core` pinned anew**: the test server's image tag (step 2).
