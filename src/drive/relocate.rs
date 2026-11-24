//! Drives made by 1.3.x move into the one entry at the first start of this
//! version (theca/computer#94): each had a mount, a sync root or a domain of
//! its own, in a folder chosen for it (Linux, Windows) or named by macOS. An
//! old drive is recognised by how it is registered, not by a column, so a move
//! cut short is found again at the next start.
//!
//! The pair row stays (its account, its position, its entries, its kept marks);
//! only where the drive is changes, and nothing not uploaded is ever lost.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::local;
use crate::ops::OpError;
#[cfg(any(target_os = "linux", windows))]
use crate::state::Pair;
use crate::state::{Account, Mode, State};

/// One account's move at a time: the names they take are the state's.
static ONE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What it said once the drives moved.
const MOVED: &str = "Your drives moved into one entry";

/// The name a moved drive's folder keeps (the one it had, which is what
/// its user knows it by): numbered when another drive here has it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn unique(base: &str, taken: &[String]) -> String {
    let free = |name: &str| !taken.iter().any(|t| local::key(t) == local::key(name));
    if free(base) {
        return base.to_owned();
    }
    (2..)
        .map(|n| format!("{base} {n}"))
        .find(|name| free(name))
        .expect("a free name")
}

/// The names of the other drives' folders (`except`: the one being moved),
/// and those of the drives moved already.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn taken(state: &State, moved: &[String], except: i64) -> Result<Vec<String>, OpError> {
    let mut names: Vec<String> = state
        .pairs()?
        .iter()
        .filter(|p| p.mode == Mode::Drive && p.id != except)
        .filter_map(|p| Some(p.path.file_name()?.to_string_lossy().into_owned()))
        .collect();
    names.extend(moved.iter().cloned());
    Ok(names)
}

/// This account's drives made by an earlier version moved into the drives
/// folder, once, with a notice. Run when its engine starts.
pub async fn run(state: &State, account: &Account) -> Result<(), OpError> {
    let _one = ONE.lock().await;
    let moved = relocate(state, account).await?;
    if moved > 0 {
        state.notify(
            Some(account.id),
            None,
            "info",
            None,
            &format!("{MOVED}: {}", entry()),
            None,
        )?;
    }
    Ok(())
}

/// Where the drives are now, as the notice says.
fn entry() -> String {
    #[cfg(target_os = "macos")]
    return "Theca in Finder's sidebar".to_owned();
    #[cfg(not(target_os = "macos"))]
    return "the drives folder (theca-drives)".to_owned();
}

/// Linux: a drive whose folder isn't in the drives folder (it was its own
/// mount): its folder rewritten, the old mount cleared and the old folder
/// removed if empty. No content moves: the drive's files are in its backing
/// folder, which stays.
#[cfg(target_os = "linux")]
async fn relocate(state: &State, account: &Account) -> Result<usize, OpError> {
    let folder = crate::ops::drives_folder(state)?;
    let mut moved = Vec::new();
    for p in old_drives(state, account, &folder)? {
        let base = p.path.file_name().map_or_else(
            || p.library_name.clone(),
            |n| n.to_string_lossy().into_owned(),
        );
        let name = unique(&base, &taken(state, &moved, p.id)?);
        // A mount of the old folder a crash left, or the old version's.
        crate::drive::fuse::clear(&p.path);
        state.set_pair_path(p.id, &folder.join(&name), p.root_file_id.as_deref())?;
        if state.drives_folder()?.is_none() {
            state.set_drives_folder(&folder)?;
        }
        // Only if it is empty: whatever else is there is the user's.
        let _ = std::fs::remove_dir(&p.path);
        moved.push(name);
    }
    Ok(moved.len())
}

/// The account's drives that aren't folders of the drives folder.
#[cfg(any(target_os = "linux", windows))]
fn old_drives(
    state: &State,
    account: &Account,
    folder: &std::path::Path,
) -> Result<Vec<Pair>, OpError> {
    Ok(state
        .pairs()?
        .into_iter()
        .filter(|p| p.account == account.id && p.mode == Mode::Drive)
        .filter(|p| p.path.parent() != Some(folder))
        .collect())
}

/// Windows: a sync root can't move, so the drive is removed (journaled: its
/// edits not uploaded moved aside, its sync root unregistered, its folder
/// deleted) and added again under the one sync root, its kept files recorded
/// first and kept again once the new drive's first rounds made their
/// placeholders (`repin`). The marker says it is being done, so a crash
/// between the removal and the add is finished by the next start.
#[cfg(windows)]
async fn relocate(state: &State, account: &Account) -> Result<usize, OpError> {
    let folder = crate::ops::drives_folder(state)?;
    let key = format!("readd_{}", account.id);
    let save =
        |todo: &[Readd]| state.set_setting(&key, &serde_json::to_string(todo).unwrap_or_default());
    let mut todo: Vec<Readd> = state
        .setting(&key)?
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    // The drives still in a folder of their own: listed to be added again (once,
    // whatever a crash left of an earlier try), then removed. A removal that
    // fails leaves the drive as it is, off the list.
    let mut moved = 0;
    for p in old_drives(state, account, &folder)? {
        if !todo.iter().any(|r| r.library == p.library) {
            todo.push(Readd {
                library: p.library,
                kept: kept_paths(state, &p)?,
            });
            save(&todo)?;
        }
        if let Err(e) = crate::ops::remove(state, &p, false) {
            tracing::warn!(library = %p.library_name, "an earlier drive not moved: {e}");
            todo.retain(|r| r.library != p.library);
            save(&todo)?;
            continue;
        }
        moved += 1;
    }
    // Added again, each on its own: what fails for the network or the server
    // waits for the next start; what is refused for good (a library gone, its
    // access removed) is said once, and not tried again.
    let mut left = Vec::new();
    for next in todo {
        match crate::ops::add(
            state,
            account,
            &next.library.to_string(),
            None,
            false,
            Mode::Drive,
        )
        .await
        {
            Ok(pair) => {
                if !next.kept.is_empty() {
                    state.set_setting(&format!("repin_{}", pair.id), &next.kept.join("\n"))?;
                }
            }
            Err(OpError::Refused(why)) => state.notify(
                Some(account.id),
                None,
                "error",
                None,
                &format!("A drive made by an earlier version couldn't be added again ({why}): add it as a drive by hand"),
                None,
            )?,
            Err(e) => {
                tracing::warn!(library = %next.library, "a drive not added again yet: {e}");
                left.push(next);
            }
        }
    }
    if left.is_empty() {
        state.clear_setting(&key)?;
    } else {
        save(&left)?;
    }
    Ok(moved)
}

/// A drive removed to be added again (Windows): its library, and the paths of
/// what was kept, below its folder.
#[cfg(windows)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Readd {
    library: uuid::Uuid,
    kept: Vec<String>,
}

/// What a drive keeps, as paths below its folder.
#[cfg(windows)]
fn kept_paths(state: &State, pair: &Pair) -> Result<Vec<String>, OpError> {
    let kept = state.kept(pair.id)?;
    if kept.is_empty() {
        return Ok(Vec::new());
    }
    let entries = state.entries(pair.id)?;
    Ok(crate::drive::paths(&entries, pair.root)
        .into_iter()
        .filter(|(_, e)| kept.contains(&e.node))
        .map(|(path, _)| {
            path.iter()
                .map(|c| c.to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect())
}

/// A moved drive's kept files kept again (Windows): the paths recorded with
/// `relocate`, those whose placeholder exists by now; the rest wait for a
/// later round, until none is left.
#[cfg(windows)]
pub fn repin(state: &State, pair: &Pair) {
    let key = format!("repin_{}", pair.id);
    let Ok(Some(recorded)) = state.setting(&key) else {
        return;
    };
    let mut left = Vec::new();
    for rel in recorded.lines() {
        let path = pair.path.join(rel);
        if !path.exists() {
            left.push(rel);
        } else if let Err(e) = crate::ops::keep(state, &path, true) {
            tracing::warn!(path = %path.display(), "not kept again: {e}");
        }
    }
    let _ = if left.is_empty() {
        state.clear_setting(&key)
    } else {
        state.set_setting(&key, &left.join("\n"))
    };
}

/// macOS: a drive that has a domain of its own (named by its library's
/// UUID) leaves it: the domain removed keeping what wasn't uploaded (moved to
/// `<default folder>/<library> (not uploaded)`), the one domain made, the
/// drive a folder of it, named as the old domain was in Finder.
#[cfg(target_os = "macos")]
async fn relocate(state: &State, account: &Account) -> Result<usize, OpError> {
    use theca_fileprovider::host;
    let one = crate::drive::provider::domain_of(state)?;
    let mut moved = Vec::new();
    for p in state
        .pairs()?
        .into_iter()
        .filter(|p| p.account == account.id && p.mode == Mode::Drive)
    {
        let old = p.library.simple().to_string();
        if old == one {
            continue;
        }
        let Some(name) = host::domains()
            .map_err(|e| OpError::Refused(format!("macOS didn't list the drives: {e}")))?
            .into_iter()
            .find(|d| d.id == old)
            .map(|d| d.name)
        else {
            continue;
        };
        // The new domain and the drive's place in it first, the old domain last:
        // whatever fails before, the old domain is still there for the next
        // start to find (it is how an old drive is recognised).
        let root = crate::ops::ensure_mac_domain(state)?;
        let name = unique(&name, &taken(state, &moved, p.id)?);
        state.set_pair_path(p.id, &root.join(&name), None)?;
        let kept = host::remove(&old)
            .map_err(|e| OpError::Refused(format!("macOS didn't remove the old drive: {e}")))?;
        crate::ops::keep_aside(state, &p, kept)?;
        moved.push(name);
    }
    Ok(moved.len())
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    /// A moved drive keeps its folder's name; another drive having it, it is
    /// numbered (names compared as the file system compares them).
    #[test]
    fn a_moved_drive_keeps_its_name_unless_another_has_it() {
        let taken = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        assert_eq!(unique("Docs", &taken(&[])), "Docs");
        assert_eq!(unique("Docs", &taken(&["Photos"])), "Docs");
        assert_eq!(unique("Docs", &taken(&["docs"])), "Docs 2");
        assert_eq!(unique("Docs", &taken(&["Docs", "Docs 2"])), "Docs 3");
    }
}
