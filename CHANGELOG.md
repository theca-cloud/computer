# Changelog

## [1.5.0] - 2026-11-27

### Features

- **computer**: A Mac drive browses fast, quits cleanly and starts without a prompt; the window has pages

### Documentation

- USING.md links Theca's Microsoft Store page

## [1.4.0] - 2026-11-24

### Features

- **drive**: One Theca entry for every drive, in a drives folder; start at login named Theca (**breaking**)

### Fixes

- **ci**: Winget's manifests start with their schema header
- **ci**: The Microsoft Store takes the installer from packages.theca.rs
- **deps**: Cookie 0.18.3, for RUSTSEC-2026-0594 (private cookie nonces repeating after a fork)
- **state**: Every write transaction takes the write lock when it begins

## [1.3.1] - 2026-11-22

### Fixes

- **ci**: The cask's macOS requirement in Homebrew 7's form, and winget's license link
- **windows**: The app opens no console window, and every release run keeps screenshots of the installed app
- **deps**: Url 2.5.9, for RUSTSEC-2026-0592 (a stack overflow in Url::origin on nested blob: URLs)

### Documentation

- The release from this workspace stops at the merge, and its pipelines run alone
- A release ends with its announcement on Discord

## [1.3.0] - 2026-11-19

### Features

- **update**: The app and headless update themselves (theca/computer#3)
- The macOS and Windows packages are signed by the developer
- The computer app's channels: our apt and dnf repositories, a Homebrew tap, winget and the Store
- **computer**: The app starts at login and the daemon runs as a service

### Fixes

- **ci**: The app's tests run after the others, never beside them
- **drive**: A failed or killed drive test can no longer hang its machine

### Documentation

- Cite the wiki instead of the docs repo's files

## [1.2.0] - 2026-11-11

### Features

- **drive**: Libraries as drives on Linux, Windows and macOS
- **logs**: Logs to files, a day each, with their context
- **libraries**: A first step to choose them, and what new ones do

### Fixes

- Core 1.6.2, a network error without its URL's query
- **drive**: A download never shows a drive's file under another name

## [1.1.0] - 2026-11-10

### Features

- Ignore files (.thecaignore): what they name is never uploaded

### Fixes

- **ci**: Run the checks on prod and version tags, before any release

### Documentation

- USING.md is theca.rs/computer: sprints keep it current

## [1.0.0] - 2026-11-07

The first stable release: from now on, what the app keeps only moves forward, and its commands keep working within 1.x.

### Breaking

- `theca add --auto-sync-mb` is gone: `theca account <account> --auto-sync-mb <n>` sets it, and `--folder` an account's default folder.
- The app's identifier is now `rs.theca.computer`.

### Features

- What the app keeps (its state file) is upgraded by every new version, 0.2.0's included: no more deleting it by hand. A copy of the previous one is kept beside it (`state.sqlite.v2`), without the app password.
- The app password moves into the system's keychain: the login keychain on macOS, the Credential Manager on Windows, the Secret Service on Linux. Where there is none (most headless servers), it stays in the state file and the login says so; `theca login --no-keyring` (`THECA_NO_KEYRING`) keeps it there. Until the app is signed, each new version asks once on macOS to read it: click Always Allow.
- `theca account` shows and changes an account's settings.
- Exit codes: 3 when `theca status` finds a folder not syncing, 4 when an account must log in again. Only 0 or not is promised; the codes are indications.
- [USING.md](https://github.com/theca-cloud/computer/blob/1.0.0/USING.md): installing it, the app password, headless use.

## [0.2.0] - 2026-11-07

### Breaking

- Needs a server on Theca's community edition 1.4.4 or later (`core` 1.4.0), and says so when logging in to an older one.
- The state an earlier version wrote is refused: delete the file it names, then log in and add your folders again (their files stay, and are adopted).

### Fixes

- A file is deleted or overwritten only if it is still the synced one, found by its file ID: on Linux, an unsynced `REPORT.txt` beside the synced `Report.txt` could be deleted.
- A download never overwrites a file saved while it lands.
- Names are compared exactly as the server does; two server names that are one file on macOS or Windows (`σ` and `ς`) are listed, never merged.
- A first sync into a folder keeps the server's spelling of names (the next round renamed them on the server).
- An external disk mounted again is followed when its files show it's the same disk, and a folder stopped while its disk was gone syncs again by itself once it's back.
- Ctrl-C, SIGTERM and Quit stop within seconds, leaving no partial download behind.
- A folder that stops syncing is notified by the app, logged, and makes `theca status` fail.
- A server restored from a backup is noticed even after new changes.
- Two state files never share a lock file.

## [0.1.0] - 2026-11-06

### Features

- The sync client: local folders kept identical to Theca libraries, both ways
- **release**: Packages for macOS, Windows and Linux, built and released on GitHub

### Fixes

- **gui**: A long folder path no longer widens the window
- **ci**: The linux workflow runs only on a linux/amd64 agent
- **tests**: A stopped device's engine has stopped; failed scenarios show the engines' logs
- **sync**: A new file given a deleted file's inode is no move
- **tests**: Each scenario's database is dropped when it ends
- **tests**: The engine's notices and entries are awaited, not read at once after converging
- The sync client never loses a user's files (sprint 11)

### Documentation

- **ci**: Our agents take only the workflows labelled for them
- **ci**: The Windows VM is started by hand, like the Mac agents
- **ci**: Starting and stopping the Mac and Windows agents, at the top
- **readme**: A Discord link, for questions, support and saying you run Theca

### Build and CI

- **release**: Releases before 1.0 are GitHub pre-releases, saying to keep a backup

