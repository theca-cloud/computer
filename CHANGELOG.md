# Changelog

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

