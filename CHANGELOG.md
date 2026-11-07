# Changelog

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

