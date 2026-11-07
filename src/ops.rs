//! What a user does, for the CLI and the GUI alike: log in, add and remove
//! sync pairs and accounts, undo a delete.

use std::path::{Path, PathBuf};
use std::time::Duration;

use theca_client::{Client, Error};
use theca_common::api;
use uuid::Uuid;

use crate::local;
use crate::state::{Account, Pair, State, StateError};

#[derive(Debug, thiserror::Error)]
pub enum OpError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Client(#[from] Error),
    #[error("{0}")]
    Refused(String),
    /// A first sync into a folder holding files: a merge, asked first.
    #[error(
        "{path} holds {}: they would be merged with {library} (identical files kept, different ones become new versions); confirm the merge",
        crate::engine::plural(*files, "file"),
        path = path.display()
    )]
    Holds {
        path: PathBuf,
        library: String,
        files: usize,
    },
}

pub type Result<T> = std::result::Result<T, OpError>;

fn refused<T>(why: impl Into<String>) -> Result<T> {
    Err(OpError::Refused(why.into()))
}

/// Libraries up to this size are synced when they appear, by default.
pub const AUTO_SYNC_LIMIT: i64 = 500 * 1024 * 1024;

/// This device's name, as proposed: its host name.
pub fn device_name() -> String {
    gethostname::gethostname()
        .to_string_lossy()
        .trim_end_matches(".local")
        .to_owned()
}

/// The default folder proposed for a new account: `~/theca` for the first,
/// `~/theca-<instance host>` once that one is taken.
pub fn proposed_folder(state: &State, instance: &str) -> Result<PathBuf> {
    let home = directories::UserDirs::new()
        .map(|d| d.home_dir().to_owned())
        .unwrap_or_else(|| PathBuf::from("."));
    let taken: Vec<PathBuf> = state
        .accounts()?
        .into_iter()
        .map(|a| a.default_folder)
        .collect();
    let first = home.join("theca");
    if !taken.contains(&first) {
        return Ok(first);
    }
    let host = instance
        .split("://")
        .last()
        .unwrap_or(instance)
        .split(['/', ':'])
        .next()
        .unwrap_or("instance");
    Ok(home.join(format!("theca-{host}")))
}

/// Logs a device in through the browser: `show` gets the flow (its URL to
/// open, its code to show); this waits for the approval, then saves the
/// account. With `token` (an app password made in the web app, for
/// scripts), no flow.
pub async fn login(
    state: &State,
    instance: &str,
    device: &str,
    folder: Option<PathBuf>,
    token: Option<String>,
    show: impl FnOnce(&api::LoginFlow),
) -> Result<Account> {
    let instance = instance.trim_end_matches('/').to_owned();
    let client = Client::new(&instance)?;
    client.check_version().await?;
    let password = match token {
        Some(token) => token,
        None => {
            let flow = client.start_login_flow(device).await?;
            show(&flow);
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                match client.poll_login_flow(&flow.poll).await {
                    Ok(Some(created)) => break created.password,
                    Ok(None) => {}
                    Err(Error::Api { status: 404, .. }) => {
                        return refused("the login expired (15 minutes): start again");
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
    };
    let client = client.with_token(password.clone());
    let me = client.me().await?;
    let known = state
        .accounts()?
        .into_iter()
        .find(|a| a.instance == instance && a.uuid == me.account);
    let default_folder = match (folder, &known) {
        (Some(folder), _) => folder,
        (None, Some(known)) => known.default_folder.clone(),
        (None, None) => proposed_folder(state, &instance)?,
    };
    let mut account = Account {
        id: 0,
        instance,
        uuid: me.account,
        email: me.email,
        device: device.to_owned(),
        password,
        default_folder,
        auto_sync_limit: known
            .as_ref()
            .map_or(Some(AUTO_SYNC_LIMIT), |k| k.auto_sync_limit),
        signed_out: false,
    };
    account.id = state.save_account(&account)?;
    Ok(account)
}

pub fn client(account: &Account) -> Result<Client> {
    Ok(Client::new(&account.instance)?.with_token(account.password.clone()))
}

/// A library as the user sees it: synced where, or offered.
#[derive(Debug, Clone)]
pub struct Library {
    pub library: api::Library,
    pub organization: Option<String>,
    pub size: Option<i64>,
    pub files: Option<i64>,
    pub pair: Option<Pair>,
}

/// The account's libraries, each with its organization's name, its size and
/// its pair if synced.
pub async fn libraries(state: &State, account: &Account) -> Result<Vec<Library>> {
    let client = client(account)?;
    let pairs = state.pairs()?;
    let organizations = client
        .me()
        .await
        .map(|me| me.organizations)
        .unwrap_or_default();
    let mut out = Vec::new();
    for library in client.libraries().await? {
        let info = client.library(library.id).await.ok();
        let organization = organizations
            .iter()
            .find(|o| o.organization == library.organization)
            .map(|o| {
                if o.personal {
                    "My files".to_owned()
                } else {
                    o.name.clone()
                }
            });
        out.push(Library {
            organization,
            pair: pairs
                .iter()
                .find(|p| p.account == account.id && p.library == library.id)
                .cloned(),
            size: info.as_ref().map(|i| i.size),
            files: info.as_ref().map(|i| i.files),
            library,
        });
    }
    Ok(out)
}

/// Starts syncing a library into `path` (default: the account's default
/// folder). A folder already holding files needs `merge`.
pub async fn add(
    state: &State,
    account: &Account,
    library: &str,
    path: Option<PathBuf>,
    merge: bool,
) -> Result<Pair> {
    let client = client(account)?;
    let all = client.libraries().await?;
    let wanted = local::key(library);
    let by_name: Vec<&api::Library> = all
        .iter()
        .filter(|l| local::key(&l.name) == wanted)
        .collect();
    let found = match all.iter().find(|l| l.id.to_string() == library) {
        Some(l) => l.clone(),
        // One name in two organizations: never a guess (a merge could send
        // the files into the other one).
        None if by_name.len() > 1 => {
            return refused(format!(
                "several libraries are named {library:?}: use its ID (theca libraries)"
            ));
        }
        None => match by_name.first() {
            Some(l) => (*l).clone(),
            None => return refused(format!("no library {library:?} in {}", account.email)),
        },
    };
    let pairs = state.pairs()?;
    if pairs
        .iter()
        .any(|p| p.account == account.id && p.library == found.id)
    {
        return refused(format!("{} is already synced", found.name));
    }
    let path = match path {
        Some(path) => path,
        None => {
            let others: Vec<(Uuid, String)> = all.iter().map(|l| (l.id, l.name.clone())).collect();
            let taken: Vec<PathBuf> = pairs.iter().map(|p| p.path.clone()).collect();
            let library = (found.id, found.name.as_str(), found.organization);
            default_path(&client, &account.default_folder, library, &others, &taken).await
        }
    };
    check_path(state, &path, None)?;
    let holds = std::fs::read_dir(&path).is_ok_and(|mut d| d.next().is_some());
    if holds && !merge {
        let files = local::scan(&path).map_or(0, |s| s.items.iter().filter(|i| i.is_file).count());
        return Err(OpError::Holds {
            path,
            library: found.name.clone(),
            files,
        });
    }
    std::fs::create_dir_all(&path)
        .map_err(|e| OpError::Refused(format!("{}: {e}", path.display())))?;
    let mut pair = Pair {
        id: 0,
        account: account.id,
        library: found.id,
        root: found.root,
        library_name: found.name,
        path,
        root_file_id: None,
        position: None,
        position_event: None,
        paused: false,
        stopped: None,
        writable: found.writable,
        activity: "waiting".into(),
        synced_at: None,
    };
    pair.id = state.add_pair(&pair)?;
    state.set_pair_path(pair.id, &pair.path, local::root_id(&pair.path).as_deref())?;
    Ok(pair)
}

/// Where a library goes by default: `<folder>/<library>`, or
/// `<folder>/<library> (<organization>)` when another library the account
/// sees has the same name, or a synced folder already takes the plain path
/// (the personal organization is "My files").
pub async fn default_path(
    client: &Client,
    folder: &Path,
    (id, name, organization): (Uuid, &str, Uuid),
    others: &[(Uuid, String)],
    taken: &[PathBuf],
) -> PathBuf {
    let plain = folder.join(name);
    let clash = others
        .iter()
        .any(|(other, n)| *other != id && local::key(n) == local::key(name))
        || taken.contains(&plain);
    if !clash {
        return plain;
    }
    let membership = client.me().await.ok().and_then(|me| {
        me.organizations
            .into_iter()
            .find(|o| o.organization == organization)
    });
    let organization = match membership {
        Some(o) if o.personal => "My files".to_owned(),
        Some(o) => o.name,
        None => organization.to_string()[..8].to_owned(),
    };
    folder.join(format!("{name} ({organization})"))
}

/// Refused as a pair's folder (`except`: the pair being moved): inside or
/// around another pair's (one library's files would land in the other, and
/// deletions cross), the home folder or `/`, or around the state's folder (it
/// holds the app passwords, and would be uploaded).
pub fn check_path(state: &State, path: &Path, except: Option<i64>) -> Result<()> {
    let real = real(path);
    let home = directories::BaseDirs::new().map(|d| self::real(d.home_dir()));
    if real.parent().is_none() || home.as_ref() == Some(&real) {
        return refused(format!(
            "{} is too wide to sync: choose a folder inside it",
            path.display()
        ));
    }
    if let Some(dir) = state.path().and_then(Path::parent)
        && self::real(dir).starts_with(&real)
    {
        return refused(format!(
            "{} holds this app's own data: choose another folder",
            path.display()
        ));
    }
    for p in state.pairs()?.iter().filter(|p| Some(p.id) != except) {
        let other = self::real(&p.path);
        if real.starts_with(&other) || other.starts_with(&real) {
            return refused(format!(
                "{} is inside or around {}, already synced",
                path.display(),
                p.path.display()
            ));
        }
    }
    Ok(())
}

/// `path` as the disk resolves it (symlinks, `..`), even where it doesn't
/// exist yet: its nearest existing folder resolved, the rest appended.
fn real(path: &Path) -> PathBuf {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    let mut rest = Vec::new();
    let mut at = path.as_path();
    loop {
        if let Ok(found) = at.canonicalize() {
            return rest.iter().rev().fold(found, |p, n| p.join(n));
        }
        match (at.parent(), at.file_name()) {
            (Some(up), Some(name)) => {
                rest.push(name.to_owned());
                at = up;
            }
            _ => return path,
        }
    }
}

/// Whether the disk holding `path` (or its nearest existing folder) has room
/// for `size` bytes, with a tenth of it to spare.
pub fn has_room(path: &Path, size: i64) -> bool {
    let existing = path.ancestors().find(|p| p.exists()).unwrap_or(path);
    let needed = u64::try_from(size)
        .unwrap_or(0)
        .saturating_add(u64::try_from(size / 10).unwrap_or(0));
    fs4::available_space(existing).is_ok_and(|free| free > needed)
}

/// Deletes the pair's files that are still exactly what was synced (its
/// `entries`, read before the pair left the state), then the folders left
/// empty; anything modified or never uploaded stays. How many were kept.
pub fn delete_synced_files(pair: &Pair, entries: &[crate::state::Entry]) -> usize {
    // Not the pair's folder any more (replaced, a disk unmounted): what is
    // there now was never synced.
    if local::root_id(&pair.path) != pair.root_file_id {
        return entries.iter().filter(|e| e.is_file).count();
    }
    let by_node: std::collections::HashMap<Uuid, &crate::state::Entry> =
        entries.iter().map(|e| (e.node, e)).collect();
    // A node's names and file IDs from the root.
    let steps = |mut node: Uuid| {
        let mut steps = Vec::new();
        while node != pair.root {
            let e = by_node.get(&node)?;
            steps.push((e.name.as_str(), e.file_id.as_deref()));
            node = e.parent;
        }
        steps.reverse();
        Some(steps)
    };
    for e in entries.iter().filter(|e| e.is_file) {
        let Some(steps) = steps(e.node) else { continue };
        let path = local::resolve(&pair.path, &steps);
        // The engine's whole fingerprint: another file with the same size
        // and mtime (a copy restoring times, an atomic save) was never synced.
        if local::unchanged(&path, e.size, e.mtime, e.file_id.as_deref()) {
            let _ = std::fs::remove_file(&path);
        }
    }
    // Folders, deepest first, if empty.
    let mut folders: Vec<Vec<(&str, Option<&str>)>> = entries
        .iter()
        .filter(|e| !e.is_file)
        .filter_map(|e| steps(e.node))
        .collect();
    folders.sort_by_key(|f| std::cmp::Reverse(f.len()));
    for folder in folders {
        let _ = std::fs::remove_dir(local::resolve(&pair.path, &folder));
    }
    let _ = std::fs::remove_dir(&pair.path);
    if pair.path.exists() {
        local::scan(&pair.path).map_or(0, |s| s.items.iter().filter(|i| i.is_file).count())
    } else {
        0
    }
}

/// Stops syncing: the pair's state goes; its files stay, or only those
/// identical to what was synced are deleted. How many were kept.
pub fn remove(state: &State, pair: &Pair, delete_files: bool) -> Result<usize> {
    // The pair out of the state first: an engine running meanwhile, or the
    // next start after a crash, would take the deletions for the user's.
    let entries = state.entries(pair.id)?;
    state.remove_pair(pair)?;
    let kept = if delete_files {
        delete_synced_files(pair, &entries)
    } else {
        0
    };
    crate::crash_point("removal-deleted");
    Ok(kept)
}

/// Removes an account: its pairs (files kept or deleted as `remove`), then
/// the app password, revoked on the server when possible.
pub async fn remove_account(state: &State, account: &Account, delete_files: bool) -> Result<usize> {
    let mut kept = 0;
    for pair in state
        .pairs()?
        .into_iter()
        .filter(|p| p.account == account.id)
    {
        kept += remove(state, &pair, delete_files)?;
    }
    // Best effort (offline: it stays listed in the web app, to revoke there).
    if let Ok(client) = client(account) {
        let _ = client.logout().await;
    }
    state.remove_account(account.id)?;
    Ok(kept)
}

/// Pauses or resumes a pair. Resuming also lifts a stop about its folder:
/// the engine checks the folder again, and stops it again if it's still
/// missing.
pub fn set_paused(state: &State, pair: &Pair, paused: bool) -> Result<()> {
    state.set_paused(pair.id, paused)?;
    if !paused && pair.stopped_for_its_folder() {
        state.set_stopped(pair.id, None)?;
    }
    Ok(())
}

/// What `locate` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Located {
    /// The synced folder, moved or renamed: it syncs on from there.
    Moved,
    /// Another folder (another file ID): not taken without `confirm`.
    Different,
}

/// Points a pair at its folder again (moved while the app was off, or on
/// another disk). The same folder (its file ID) resumes as it was. Another
/// one, confirmed, starts over as a first sync into it (a merge): compared
/// with the old base, every file it lacks would look deleted, and be trashed
/// on the server.
pub fn locate(state: &State, pair: &Pair, path: &Path, confirm: bool) -> Result<Located> {
    if !path.is_dir() {
        return refused(format!("{} isn't a folder", path.display()));
    }
    check_path(state, path, Some(pair.id))?;
    let id = local::root_id(path);
    let same = id.is_some() && id == pair.root_file_id;
    if !same && !confirm {
        return Ok(Located::Different);
    }
    if !same {
        state.forget_base(pair.id)?;
    }
    state.set_pair_path(pair.id, path, id.as_deref())?;
    state.set_stopped(pair.id, None)?;
    Ok(Located::Moved)
}

/// Restores what the latest delete moved to the trash (`notice`'s Undo).
pub async fn undo(state: &State, notice: i64) -> Result<usize> {
    let Some(n) = state.notices()?.into_iter().find(|n| n.id == notice) else {
        return refused("nothing to undo");
    };
    let (Some(pair), Some(undo)) = (n.pair, n.undo.as_deref()) else {
        return refused("nothing to undo");
    };
    let Some(pair) = state.pair(pair)? else {
        return refused("that folder isn't synced anymore");
    };
    let Some(account) = state.account(pair.account)? else {
        return refused("that account was removed");
    };
    let nodes: Vec<Uuid> = undo.split(' ').filter_map(|n| n.parse().ok()).collect();
    let results = client(&account)?.restore_nodes(&nodes).await?;
    state.dismiss(notice)?;
    Ok(results.iter().filter(|r| r.is_ok()).count())
}

/// Finds a pair by its library's name, its folder, or its ID.
pub fn find_pair(state: &State, which: &str) -> Result<Pair> {
    let pairs = state.pairs()?;
    let path = Path::new(which).canonicalize().ok();
    let found: Vec<Pair> = pairs
        .into_iter()
        .filter(|p| {
            p.id.to_string() == which
                || local::key(&p.library_name) == local::key(which)
                || path
                    .as_deref()
                    .is_some_and(|w| p.path.canonicalize().ok().as_deref() == Some(w))
        })
        .collect();
    match found.len() {
        1 => Ok(found.into_iter().next().expect("one")),
        0 => refused(format!("no synced folder {which:?}")),
        _ => refused(format!(
            "several synced folders match {which:?}: use its path or ID"
        )),
    }
}

/// Finds an account by email, instance or ID; the only one when not given.
pub fn find_account(state: &State, which: Option<&str>) -> Result<Account> {
    let accounts = state.accounts()?;
    let found: Vec<Account> = match which {
        None => accounts,
        Some(w) => accounts
            .into_iter()
            .filter(|a| {
                a.id.to_string() == w || a.email.eq_ignore_ascii_case(w) || a.instance.contains(w)
            })
            .collect(),
    };
    match found.len() {
        1 => Ok(found.into_iter().next().expect("one")),
        0 => refused("no such account: theca login <instance>"),
        _ => refused("several accounts: choose one with --account"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pair on a real folder holding `a.txt`, synced (its base entry).
    fn synced_pair(dir: &Path) -> (State, Pair) {
        let state = State::memory().unwrap();
        let account = state
            .save_account(&crate::state::Account {
                id: 0,
                instance: "https://x".into(),
                uuid: Uuid::now_v7(),
                email: "a@x".into(),
                device: "d".into(),
                password: "p".into(),
                default_folder: dir.to_owned(),
                auto_sync_limit: None,
                signed_out: false,
            })
            .unwrap();
        let root = Uuid::now_v7();
        let mut pair = Pair {
            id: 0,
            account,
            library: Uuid::now_v7(),
            root,
            library_name: "Docs".into(),
            path: dir.to_owned(),
            root_file_id: local::root_id(dir),
            position: Some(1),
            position_event: None,
            paused: false,
            stopped: None,
            writable: true,
            activity: String::new(),
            synced_at: None,
        };
        pair.id = state.add_pair(&pair).unwrap();
        state
            .set_pair_path(pair.id, dir, pair.root_file_id.as_deref())
            .unwrap();
        state
            .record_remote(pair.id, &[], Some(theca_common::api::Position::Seq(1)))
            .unwrap();
        std::fs::write(dir.join("a.txt"), "synced").unwrap();
        let meta = std::fs::metadata(dir.join("a.txt")).unwrap();
        let entry = crate::state::Entry {
            node: Uuid::now_v7(),
            parent: root,
            name: "a.txt".into(),
            is_file: true,
            version: Some(Uuid::now_v7()),
            size: meta.len() as i64,
            mtime: local::mtime(&meta),
            file_id: local::file_id(&dir.join("a.txt")),
        };
        state.synced(pair.id, &entry, Some("a.txt"), &[]).unwrap();
        (state, pair)
    }

    /// A5: another file at the path, same size and mtime (a copy restoring
    /// times, an atomic save), was never synced: it stays.
    #[test]
    fn removal_keeps_another_file_with_the_same_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let (state, pair) = synced_pair(dir.path());
        let a = dir.path().join("a.txt");
        let mtime = std::fs::metadata(&a).unwrap().modified().unwrap();
        let other = dir.path().join("other");
        std::fs::write(&other, "edited").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&other)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        std::fs::rename(&other, &a).unwrap();
        delete_synced_files(&pair, &state.entries(pair.id).unwrap());
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "edited");
    }

    /// A5: a pair whose folder isn't its own any more (replaced, a disk
    /// unmounted) deletes nothing in what is there now.
    #[test]
    fn removal_deletes_nothing_in_a_replaced_folder() {
        let dir = tempfile::tempdir().unwrap();
        let (state, mut pair) = synced_pair(dir.path());
        pair.root_file_id = Some("another:folder".into());
        delete_synced_files(&pair, &state.entries(pair.id).unwrap());
        assert!(dir.path().join("a.txt").exists());
    }

    /// B6: never `/`, the home folder, nor a folder holding the state.
    #[test]
    fn too_wide_folders_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open(&dir.path().join("data/state.sqlite")).unwrap();
        let home = directories::BaseDirs::new().unwrap().home_dir().to_owned();
        for path in [Path::new("/"), &home, dir.path(), &dir.path().join("data")] {
            assert!(
                check_path(&state, path, None).is_err(),
                "{}",
                path.display()
            );
        }
        assert!(check_path(&state, &dir.path().join("Docs"), None).is_ok());
    }

    #[test]
    fn room_on_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        assert!(has_room(&dir.path().join("not/yet/created"), 1));
        assert!(!has_room(dir.path(), i64::MAX / 2));
    }
}
