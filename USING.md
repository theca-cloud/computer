# Theca for computers

The Theca app for macOS, Windows and Linux keeps local folders identical to Theca libraries, both ways. It is a tray app with a small window, or a CLI and daemon (`theca`) for servers. It needs a Theca server on the community edition 1.4.4 or later (`core` 1.4.0): it says so when logging in to an older one.

## Install

Download it from the [releases](https://github.com/theca-cloud/computer/releases), for your system and processor (`arm64`: Apple Silicon, ARM servers and Raspberry Pis; `x86_64`: Intel and AMD):

| | The app | Headless (servers) |
|---|---|---|
| macOS | `theca-<version>-macos-<arch>.dmg`: open it, drag Theca to Applications | `theca-headless-<version>-macos-<arch>.tar.gz` |
| Windows | `theca-<version>-windows-x86_64.exe`: installs for you alone, no admin | `theca-headless-<version>-windows-x86_64.zip` |
| Linux | `theca-<version>-linux-<arch>.deb` (Debian, Ubuntu: `sudo apt install ./theca-….deb`), or the `.AppImage` anywhere else (`chmod +x`, then run it) | `theca-headless-<version>-linux-<arch>.tar.gz` |

The app also answers the CLI (`theca --help`). Headless, unpack the archive and put `theca` on your `PATH` (`sudo install theca-headless-*/theca /usr/local/bin/`), then `theca login`. Linux builds run on Ubuntu 22.04 and Debian 12 or newer.

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

## Headless use

```sh
theca login https://cloud.example.com   # prints a page to open and a code to check
theca libraries                         # what the account can sync
theca add Documents                     # into ~/theca/Documents by default
theca run                               # syncs, in the foreground (what a service runs)
theca status                            # what syncs, what it's doing, errors
```

Every command and its options: `theca --help`, `theca <command> --help`. The commands: `login`, `accounts`, `account` (an account's default folder and the size up to which new libraries sync by themselves), `remove-account`, `libraries`, `add`, `remove`, `pause`, `resume`, `locate` (a folder moved or on another disk), `status`, `undo` (restores what the last deletion moved to the trash), `run`. A script logs in without a browser with an app password made in the web app: `THECA_TOKEN=… theca login <instance>`.

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
| `RUST_LOG` | What the log shows, on the error output: `theca=info` by default, `theca=debug` for more |

### Where the app keeps its state

One file, `state.sqlite`: the accounts, the synced folders and what was last in sync. The app started at login always uses the default one.

| | |
|---|---|
| macOS | `~/Library/Application Support/rs.theca.theca/state.sqlite` |
| Linux | `~/.local/share/theca/state.sqlite` (`$XDG_DATA_HOME/theca/` when set) |
| Windows | `%APPDATA%\theca\theca\data\state.sqlite` |

Before an upgrade changes it, a copy of the previous one is kept beside it (`state.sqlite.v2`), without the app passwords.
