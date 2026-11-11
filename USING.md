# Theca for computers

The Theca app for macOS, Windows and Linux keeps local folders identical to Theca libraries, both ways. It is a tray app with a small window, or a CLI and daemon (`theca`) for servers. It needs a Theca server on the community edition 1.4.4 or later (`core` 1.4.0): it says so when logging in to an older one.

## Install

Download it from the [releases](https://github.com/theca-cloud/computer/releases), for your system and processor (`arm64`: Apple Silicon, ARM servers and Raspberry Pis; `x86_64`: Intel and AMD):

| | The app | Headless (servers) |
|---|---|---|
| macOS | `theca-<version>-macos-<arch>.dmg`: open it, drag Theca to Applications | the app's own `theca` (below) |
| Windows | `theca-<version>-windows-x86_64.exe`: installs for you alone, no admin | `theca-headless-<version>-windows-x86_64.zip` |
| Linux | `theca-<version>-linux-<arch>.deb` (Debian, Ubuntu: `sudo apt install ./theca-….deb`), or the `.AppImage` anywhere else (`chmod +x`, then run it) | `theca-headless-<version>-linux-<arch>.tar.gz` |

The app also answers the CLI (`theca --help`). Headless, unpack the archive and put `theca` on your `PATH` (`sudo install theca-headless-*/theca /usr/local/bin/`), then `theca login`. On a Mac used from the command line, install the app and use its `theca`: `/Applications/Theca.app/Contents/MacOS/theca` (a Mac's drives need the app's extension; there is no headless Mac archive from 1.2.0). Linux builds run on Ubuntu 22.04 and Debian 12 or newer; drives on Linux need `fuse3` (the `.deb` installs it; elsewhere, install it).

To update, install the new version over the old one: what the app keeps is upgraded the first time the new version starts.

### Opening an unsigned app

The apps aren't signed by Apple or Microsoft yet, so the first time:

- **macOS** says Apple could not verify Theca. Open **System Settings → Privacy & Security**, scroll to Security, click **Open Anyway** next to Theca, and confirm.
- **Windows** SmartScreen says "Windows protected your PC": click **More info**, then **Run anyway**.

### Checking a download

Each release lists the SHA-256 of every file in `SHA256SUMS`: `shasum -a 256 -c SHA256SUMS --ignore-missing` (macOS), `sha256sum -c SHA256SUMS --ignore-missing` (Linux), `Get-FileHash <file>` (Windows, compared by eye).

## Your app password

Logging in creates an app password for this computer, revocable from the web app (**Account → App passwords**). It is kept in the system's keychain: the login keychain on macOS, the Credential Manager on Windows, the Secret Service (GNOME Keyring, KWallet) on Linux. Where there is none, as on most headless servers, it is kept in the app's state file instead, readable by you alone, and the login says so; `theca login --no-keyring` keeps it there by choice.

On macOS, until the app is signed, each new version asks once for your password to read its keychain entry: click **Always Allow**.

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

| | Where it is | Needs |
|---|---|---|
| Windows | the folder you choose, as a synced one (NTFS only) | Windows 10 1709 or newer; Theca running for a file not downloaded to open |
| macOS | `~/Library/CloudStorage/Theca-<library>`, where macOS puts it (in Finder's sidebar); `Theca-<library> (<organization>)` when another drive has the library's name (then numbered) | macOS 13; the app (its `theca` from the command line) running for a file not downloaded to open |
| Linux | the folder you choose, as a synced one | `fuse3`; Theca running for the folder to show anything (it is mounted while Theca runs) |

**On a Mac, macOS adds a drive switched off**: switch it on in **System Settings → General → Login Items & Extensions → File Providers → Theca** (the app has a button to that pane; `theca status` says when a drive waits for it).

- **Keep a file or folder on this computer** (downloaded, never freed, what arrives later in a folder kept too): **Keep…** in the app, `theca keep <path>`, Windows' **Always keep on this device**, or Finder's **Always Keep on This Mac**. Kept files don't count in the drive limit.
- **Free its space now** (it stays listed, downloaded again when opened, no longer kept): **Free** in the app, `theca free <path>`, Windows' **Free up space**, or Finder's **Free Up Space** (on what is kept). A file open in a program is freed once it is closed (on a Mac it is refused while open: free it again); an edit not uploaded yet is never freed.
- **The drive limit**, one for the computer: `theca drive-limit` shows it, `theca drive-limit 20GB` (or `20GiB`) sets it, `theca drive-limit default` goes back to the default (10 GiB, or a tenth of the disk if smaller). Kept files and edits not uploaded yet don't count. Windows and macOS may free more on their own when the disk is low.
- **What counts as a use** (the limit frees the least recently used first): every open on Linux; on Windows, an open by any program but Theca; on macOS, an open through Finder or "Open With" (a file read by a script isn't seen as used). A file never opened since its download counts from its download.
- **Offline**, a file already downloaded opens; one that isn't fails to open (Theca says so on Windows and Linux; Finder shows its error on a Mac).
- **Removing a drive** moves the edits not uploaded yet to `<default folder>/<library> (not uploaded)`, never deleted; everything else goes (it is on the server). Uninstalling Theca on Windows removes its drives first, the same way.
- **On Linux, a file saved into the drive's folder while Theca wasn't running** (so not into the drive) stops the drive until you move it out; the drive then comes back by itself.

## Headless use

```sh
theca login https://cloud.example.com   # prints a page to open and a code to check, then the libraries
theca libraries                         # what the account can sync
theca add Documents                     # into ~/theca/Documents by default
theca run                               # syncs, in the foreground (what a service runs)
theca status                            # what syncs, what it's doing, errors
```

A login adds no library: it lists them with the `theca add` lines to choose with. What libraries new to the account later do is `--new-libraries`: `silent` by default (listed only, `theca libraries`), `sync` (up to the size limit), or `drive`.

Every command and its options: `theca --help`, `theca <command> --help`. The commands: `login`, `accounts`, `account` (an account's default folder, and what libraries new to it do: `--new-libraries sync|drive|notify|silent`, `--auto-sync-limit 2GB`, 0 for no limit), `remove-account`, `libraries`, `add`, `remove`, `pause`, `resume`, `locate` (a folder moved or on another disk), `status`, `undo` (restores what the last deletion moved to the trash), `run`, `logs`; for drives, `keep`, `free`, `drive-limit`. A script logs in without a browser with an app password made in the web app: `THECA_TOKEN=… theca login <instance>`.

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

`theca logs` shows where they are and their settings, and sets them: `theca logs --level debug` (`error`, `warn`, `info`, `debug`, `trace`), `--days 30`, `--max-size 1GB`, `--path /var/log/theca`; `default` resets one. The app has them in its settings, with **Open logs folder**. A change applies at the next start of the app or `theca run`. `RUST_LOG`, set, wins over the level. On a Mac, the drive's extension keeps its own small log in its container (`theca logs` names it).

### Where the app keeps its state

One file, `state.sqlite`: the accounts, the synced folders and what was last in sync. The app started at login always uses the default one.

| | |
|---|---|
| macOS | `~/Library/Application Support/rs.theca.theca/state.sqlite` |
| Linux | `~/.local/share/theca/state.sqlite` (`$XDG_DATA_HOME/theca/` when set) |
| Windows | `%APPDATA%\theca\theca\data\state.sqlite` |

Before an upgrade changes it, a copy of the previous one is kept beside it (`state.sqlite.v2`), without the app passwords. On Linux, a drive's files are kept beside it too, in `drives/` (its folder is mounted from there).
