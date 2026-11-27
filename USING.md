# Theca for computers

The Theca app for macOS, Windows and Linux keeps local folders identical to Theca libraries, both ways. It is a tray app with a small window, or a CLI and daemon (`theca`) for servers. It needs a Theca server on the community edition 1.4.4 or later (`core` 1.4.0): it says so when logging in to an older one.

## Install

From the channel your system already uses, which keeps Theca up to date too ([Updating](#updating)):

| | The app | CLI (servers) |
|---|---|---|
| macOS | [Homebrew](https://brew.sh): `brew install theca-cloud/tap/theca` | the app's own `theca`, which Homebrew puts on your `PATH` |
| Windows | the [Microsoft Store](https://apps.microsoft.com/detail/xpdm56pl28s36z), or `winget install Theca.Theca` | `theca-cli-<version>-windows-x86_64.zip` ([direct downloads](#direct-downloads)) |
| Debian, Ubuntu | our apt repository: `theca` | our apt repository: `theca-cli` |
| Fedora | our dnf repository: `theca` | our dnf repository: `theca-cli` |
| Other Linux (Arch too) | the `.AppImage` ([direct downloads](#direct-downloads)) | `theca-cli-<version>-linux-<arch>.tar.gz` |

If Homebrew refuses our tap as untrusted (Homebrew 7 asks before loading a tap that isn't its own), trust it first:

```sh
brew tap theca-cloud/tap
brew trust theca-cloud/tap
brew install theca-cloud/tap/theca
```

### Debian, Ubuntu: apt

```sh
sudo curl -fsSL https://packages.theca.rs/theca.gpg -o /usr/share/keyrings/theca.gpg
sudo curl -fsSL https://packages.theca.rs/theca.sources -o /etc/apt/sources.list.d/theca.sources
sudo apt update && sudo apt install theca      # or theca-cli, without the window
```

### Fedora: dnf

```sh
sudo curl -fsSL https://packages.theca.rs/RPM-GPG-KEY-theca -o /etc/pki/rpm-gpg/RPM-GPG-KEY-theca
sudo curl -fsSL https://packages.theca.rs/theca.repo -o /etc/yum.repos.d/theca.repo
sudo dnf install theca                         # or theca-cli, without the window
```

The repositories and the `.rpm` files are signed with one key, fingerprint `B7AE 400C DAA6 5388 CEC6 316D 43CE 4F13 6D66 5585`: `apt` and `dnf` check it, as the files above say. `theca` and `theca-cli` both own the `theca` command, so only one is installed at a time: `apt` replaces one with the other, `dnf` is asked to (`sudo dnf install --allowerasing theca-cli`). Installing a `.deb` or an `.rpm` downloaded from the releases adds the same repository, so `apt` and `dnf` update it from then on. The `.rpm` is built for Fedora: Red Hat Enterprise Linux 9 and its clones (Rocky, Alma) are too old for it (glibc 2.34, no WebKitGTK 4.1): use the `.AppImage`, or on a server the CLI archive.

### Direct downloads

From the [releases](https://github.com/theca-cloud/computer/releases), for your system and processor (`arm64`: Apple Silicon, ARM servers and Raspberry Pis; `x86_64`: Intel and AMD):

| | The app | CLI (servers) |
|---|---|---|
| macOS | `theca-<version>-macos-<arch>.dmg`: open it, drag Theca to Applications | the app's own `theca` (below) |
| Windows | `theca-<version>-windows-x86_64.exe`: installs for you alone, no admin | `theca-cli-<version>-windows-x86_64.zip` |
| Linux | `theca-<version>-linux-<arch>.AppImage` (`chmod +x`, then run it), or the `.deb` / `.rpm` | `theca-cli-<version>-linux-<arch>.tar.gz`, or the `theca-cli` `.deb` / `.rpm` |

The app also answers the CLI (`theca --help`), except on Windows: there the app's `theca.exe` has no console, so it opens no black window when started from the Start menu but prints nothing in a terminal either, and the command line is the CLI archive's. For the CLI archive, unpack it and put `theca` on your `PATH` (`sudo install theca-cli-*/theca /usr/local/bin/`), then `theca login`. On a Mac there is no CLI archive (since 1.2.0, a Mac's drives need the app's extension): use the app's own `theca`, which Homebrew puts on your `PATH`; installed from a `.dmg`, link it once with `sudo ln -sf /Applications/Theca.app/Contents/MacOS/theca /usr/local/bin/theca`. Linux builds run on Ubuntu 22.04 and Debian 12 or newer; drives on Linux need `fuse3` (the packages install it; elsewhere, install it).

### Updating

The app and the CLI update themselves from Theca's releases, except what a package manager installed (`theca` and `theca-cli` from our apt and dnf repositories or a `.deb` / `.rpm`): `apt` and `dnf` update those with the rest of the system (`theca update` says so there). An update is checked against Theca's release key before anything is replaced, and none is installed while one of your accounts is on a server older than it needs, or one that can't be reached (tried again at the next check). What the app keeps is upgraded the first time the new version starts; installing the new version over the old one by hand still works.

- **When**: at start, then every 6 hours, in the app and in `theca run`, as its setting says: `auto` installs and restarts (the app's default), `ask` says a new version is out (a notification, the window and the tray; the CLI, the log and `theca status`; the CLI's default), `off` never checks by itself.
- **In the app**: Settings, Updates: the setting, **Check for updates**, and a new version with its release notes and **Install and restart**. The tray has **Check for updates…**, and says **Update to X.Y.Z** while one waits: click it to install.
- **CLI**: `theca updates auto`, `ask` or `off` sets it (`theca updates` shows it). `theca update` checks and installs now, whatever the setting: the binary is replaced where it is, by a user who can write it (`sudo theca update` for `/usr/local/bin/theca`); a running `theca run` goes on with the old version until it starts again. In `auto`, `theca run` installs the new version and starts it in its place, when it can write its binary.
- **`theca update` with the app open** hands over: the app installs and restarts. Without a display (a server over ssh), there is no app to hand over to: it installs itself, and says so.
- **What it leaves**: on Windows, the binary it replaced, set aside as `theca.exe.old` beside it (it may still be running), removed at the next update. `theca status` waits 3 seconds at most to say a new version is out: offline, it says nothing of it.

### Windows' SmartScreen

The apps are signed by their developer: macOS opens them without a warning, Apple having checked them. On Windows, the installer and the app name their publisher, "Open Source Developer Pascal Perrenoud"; until that certificate has earned a reputation with Microsoft, SmartScreen may still say "Windows protected your PC": click **More info**, check the publisher, then **Run anyway**. An install from the Microsoft Store never shows it.

### Checking a download

Each release lists the SHA-256 of every file in `SHA256SUMS`: `shasum -a 256 -c SHA256SUMS --ignore-missing` (macOS), `sha256sum -c SHA256SUMS --ignore-missing` (Linux), `Get-FileHash <file>` (Windows, compared by eye).

## Your app password

Logging in creates an app password for this computer, revocable from the web app (**Account → App passwords**). It is kept in the system's keychain: the login keychain on macOS, the Credential Manager on Windows, the Secret Service (GNOME Keyring, KWallet) on Linux. Where there is none, as on most headless servers, it is kept in the app's state file instead, readable by you alone, and the login says so; `theca login --no-keyring` keeps it there by choice.

On macOS, the first signed version (after 1.2 or earlier) asks once for your password to read its keychain entry: click **Always Allow**. The versions after it shouldn't ask again.

## Choosing libraries

Right after signing in, the app lists the account's libraries: each one can be **synced** to a folder (`<the folder you chose>/<library>`, changeable per library), kept as a **drive**, or left (**Nothing**). Nothing is added until you click **Add**; **Later** adds nothing, and the libraries stay listed in the window.

The same step asks what libraries added to the account later do, changeable in the settings:

- **Show them, with a notification** (the default): listed in the window, with a system notification; nothing to answer.
- **Show them, quietly**: listed only.
- **Sync them by themselves**, into the account's folder, up to a size (500 MB by default, 0 for no limit): a larger one is shown, with a notification.
- **Add them as drives**: where this computer can't hold a drive, it says so, and syncs nothing instead.

A library you removed is never added again by itself.

## Ignore files

A file named `.thecaignore` keeps what it names from being uploaded, in its folder and every folder below it. Its patterns are `.gitignore`'s, with git's rules:

```
node_modules/
build/
*.log
```

It syncs like any other file, so every device follows it, and the web app and WebDAV too. What it names never goes up from this computer: not a new file, an edit, a folder, a move or a deletion. What the server already holds there still comes down onto the copies you haven't changed: new files and new versions, and a deletion on the server removes the local copy. A copy you changed stays as it is. Deleting a folder that isn't ignored still moves everything in it to the trash, what it names included. An ignore file that names itself stays on this computer only. A line that isn't text (UTF-8) is skipped and shown in `theca status` and the app's window.

One limitation: an ignore file arriving after the files it names (a copy still in progress, or a move from another disk) lets those that came first be uploaded. A move within the same disk is fine. More, with examples: [theca.rs/features](https://theca.rs/features#ignored-files).

## Drives

A library can be a **drive** instead of a synced folder: your file manager shows all of it, a file is downloaded when you open it, and downloads are kept within a limit, the least recently used freed first. Tick **Drive** beside a library when adding it, or `theca add <library> --drive`.

Every drive is a folder of one entry, **Theca**, in the list your file manager keeps (Finder's sidebar, Explorer's navigation pane, the devices of a Linux file manager), however many libraries you keep as drives and whichever accounts they are of. A drive has no folder of its own to choose (`theca add <library> <folder> --drive` is refused); its folder is named after the library, `<organization> - <library>` when another drive has that name, with your account's address after it when that is taken too.

| | The entry | Needs |
|---|---|---|
| Windows | **Theca** in Explorer's navigation pane, the folder `%USERPROFILE%\theca-drives` (NTFS only) | Windows 10 1709 or newer; Theca running for a file not downloaded to open |
| macOS | **Theca** in Finder's sidebar, the folder `~/Library/CloudStorage/Theca`, where macOS puts it | macOS 13; the app (its `theca` from the command line) running for a file not downloaded to open |
| Linux | **theca-drives** in the file manager's devices, the folder `~/theca-drives`, there only while Theca runs (it is mounted by Theca, and removed when Theca stops) | `fuse3`; Theca running for the folder to show anything |

The entry's top level is Theca's: a drive's folder is never renamed, moved or deleted from the file manager (that refuses), and a file moves from a drive to another only as a copy. On Windows a file made at the top of `theca-drives` stays where it is, belongs to no drive, and is never uploaded. A drive made by an earlier version moves into the entry the first time the new version runs (a notice says so, once); nothing not uploaded is lost, and on Windows the files you kept are kept again once the drive has been listed.

**On a Mac, macOS adds a drive switched off**: switch it on in **System Settings → General → Login Items & Extensions → File Providers → Theca** (the app has a button to that pane; `theca status` says when a drive waits for it).

- **Keep a file or folder on this computer** (downloaded, never freed, what arrives later in a folder kept too): **Keep…** in the app, `theca keep <path>`, Windows' **Always keep on this device**, or Finder's **Always Keep on This Mac**. Kept files don't count in the drive limit.
- **Free its space now** (it stays listed, downloaded again when opened, no longer kept): **Free** in the app, `theca free <path>`, Windows' **Free up space**, or Finder's **Free Up Space** (on what is kept). A file open in a program is freed once it is closed (on a Mac it is refused while open: free it again); an edit not uploaded yet is never freed.
- **The drive limit**, one for the computer: `theca drive-limit` shows it, `theca drive-limit 20GB` (or `20GiB`) sets it, `theca drive-limit default` goes back to the default (10 GiB, or a tenth of the disk if smaller). Kept files and edits not uploaded yet don't count. Windows and macOS may free more on their own when the disk is low.
- **What counts as a use** (the limit frees the least recently used first): every open on Linux; on Windows, an open by any program but Theca; on macOS, an open through Finder or "Open With" (a file read by a script isn't seen as used). A file never opened since its download counts from its download.
- **Offline**, a file already downloaded opens; one that isn't fails to open (Theca says so on Windows and Linux; Finder shows its error on a Mac).
- **A drive paused or stopped** stays in the entry, as a synced folder stays on disk: a file already downloaded opens, one that isn't fails to open, and what you change waits until the drive syncs again.
- **Removing a drive** moves the edits not uploaded yet to `<default folder>/<library> (not uploaded)`, never deleted; everything else goes (it is on the server). The last drive takes the entry with it. On a Mac, an edit macOS holds and can't upload yet (offline) keeps the drive from being removed: the removal says so, and works once it is online. Uninstalling Theca on Windows removes its drives first, the same way.
- **On Linux, a file saved into `~/theca-drives` while Theca wasn't running** (so not into a drive) stops every drive until you move it out; the drives then come back by themselves.

## Starting with the computer

A computer that restarts keeps syncing without anyone opening Theca: the app starts at login on a desktop, a service runs the CLI on a server.

**The app.** A fresh install turns **Start at login** on at its first launch, once; it is a switch in the app's window, and one you turn off stays off. An upgrade doesn't turn it on for you. Started at login, the app stays in the tray, with no window.

**A Linux server.** The `theca-cli` package (apt, dnf) ships a systemd unit, `theca@<user>`, that runs `theca run` as that user at boot, with their state and folders, never as root, whether or not they are logged in:

```sh
theca login https://cloud.example.com --no-keyring   # a service can't reach a desktop's keychain
theca add Documents
sudo systemctl enable --now theca@$USER
```

`theca login` ends by printing that last line when it fits (it names you, not root, under `sudo`). `systemctl status theca@<user>` and `journalctl -u theca@<user>` say how it runs, `theca logs` what it does. The packages restart the running services when they upgrade and stop them when removed. A service restarts itself 10 seconds after a failure, so one that was refused because the app holds the state starts by itself once the app quits. On a desktop use the app: it and a service can't sync one state at once, and an app started at login says so in a notification when a service holds it. With the `.tar.gz` archive, which has no unit, save this as `/etc/systemd/system/theca@.service` (and put `theca` at `/usr/bin/theca`, or change `ExecStart`):

```ini
[Unit]
Description=Theca, keeping %i's folders identical to their libraries
Documentation=https://theca.rs/computer
After=network-online.target
Wants=network-online.target

[Service]
User=%i
ExecStart=/usr/bin/theca run
Restart=on-failure
RestartSec=10

[Install]
WantedBy=multi-user.target
```

A drive under the service isn't tried yet: use synced folders on a server.

**A Windows server.** In a terminal opened **as administrator**, with the account logged in the same way (`theca login <instance> --no-keyring`, then `theca add`):

```sh
theca service on     # asks for your Windows password, once; Theca doesn't keep it
theca service off    # stops the service and removes it
```

`theca service on` registers a Windows service, `theca-<your user name>`, that starts with Windows, runs `theca run` as you (your folders and state) and restarts after a failure. `--account <name>` registers another user's account, and `theca --state <file> service on` runs the service on that state file. Windows' own **Services** window shows and stops it. Like the Linux unit it can't share a state with the app: use one or the other. Run it again to change the password or the binary: the service is stopped, changed and started. The service runs the `theca.exe` that registered it, as the account. For an **administrator's** account `theca service on` refuses unless that file is under Program Files, where only administrators can write: otherwise any program of yours could replace a `theca.exe` under your own profile and have it run with your full rights at the next start. Copy it to `C:\Program Files\Theca\` and run `theca service on` from there. A standard account can run it from anywhere. A wrong password is refused at once.

**A Mac** needs nothing more than the app at login: there is no service for the CLI.

## CLI use

```sh
theca login https://cloud.example.com   # prints a page to open and a code to check, then the libraries
theca libraries                         # what the account can sync
theca add Documents                     # into ~/theca/Documents by default
theca run                               # syncs, in the foreground (what a service runs)
theca status                            # what syncs, what it's doing, errors
```

A login adds no library: it lists them with the `theca add` lines to choose with. What libraries new to the account later do is `--new-libraries`: `silent` by default (listed only, `theca libraries`), `sync` (up to the size limit), or `drive`.

Every command and its options: `theca --help`, `theca <command> --help`. The commands: `login`, `accounts`, `account` (an account's default folder, and what libraries new to it do: `--new-libraries sync|drive|notify|silent`, `--auto-sync-limit 2GB`, 0 for no limit), `remove-account`, `libraries`, `add`, `remove`, `pause`, `resume`, `locate` (a folder moved or on another disk), `status` (its notices come under a heading, each with its number), `dismiss` (a notice by its number, or `--all`: the app's window dismisses them too; an error of the connection to the server goes by itself once it is back), `undo` (restores what the last deletion moved to the trash), `run`, `service` (Windows: `on`, `off`, above), `logs`, `update` and `updates` (above); for drives, `keep`, `free`, `drive-limit`. A script logs in without a browser with an app password made in the web app: `THECA_TOKEN=… theca login <instance>`.

One `theca run`, or the app, syncs a state file at a time: a second one refuses to start. `run` stops within a few seconds on Ctrl-C or `SIGTERM`.

### Exit codes

A command exits with 0 on success, and anything else on an error. The codes below say which error; they may change between versions, but 0 or not won't.

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | An error, said on the error output |
| 2 | Invalid arguments |
| 3 | `status`: a folder isn't syncing (stopped, or its account retrying after an error) |
| 4 | The server refused an account's app password (signed out), or it is gone from the keychain: `theca login` again |

### Environment variables

| Variable | |
|---|---|
| `THECA_STATE` | The state file, instead of the default below (`--state`) |
| `THECA_TOKEN` | `login`: an app password made in the web app, instead of the browser (`--token`) |
| `THECA_NO_KEYRING` | `login`: set (to anything but `0`, `false`, `no` or `off`), keeps the app password in the state file (`--no-keyring`) |
| `RUST_LOG` | What the log shows, on the error output and in the log files, over `theca logs --level`: `theca=debug` for more |

### Logs

The app and every `theca` command log what they do to a file per day, in `logs` beside the state (`theca.2026-11-11.log`, in UTC): to read, or to send us, when something goes wrong. Each line says what it is about (the account, the synced folder, the round, a drive's file). Finished days are gzipped; 14 days and 100 MB are kept in all, the oldest removed first. They name your files and folders; never your app password.

`theca logs` shows where they are and their settings, and sets them: `theca logs --level debug` (`error`, `warn`, `info`, `debug`, `trace`), `--days 30`, `--max-size 1GB`, `--path /var/log/theca`; `default` resets one. The app has them in its settings, with **Open logs folder**. A change applies at the next start of the app or `theca run`. `RUST_LOG`, set, wins over the level. On a Mac, the drive's extension keeps its own small log in the folder it shares with the app, `~/Library/Group Containers/<team>.rs.theca.computer` (`theca logs` names it).

### Where the app keeps its state

One file, `state.sqlite`: the accounts, the synced folders and what was last in sync. The app started at login always uses the default one.

| | |
|---|---|
| macOS | `~/Library/Application Support/rs.theca.theca/state.sqlite` |
| Linux | `~/.local/share/theca/state.sqlite` (`$XDG_DATA_HOME/theca/` when set) |
| Windows | `%APPDATA%\theca\theca\data\state.sqlite` |

Before an upgrade changes it, a copy of the previous one is kept beside it (`state.sqlite.v2`), without the app passwords. On Linux, a drive's files are kept beside it too, in `drives/` (its folder is mounted from there).
