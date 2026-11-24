//! macOS's drive, the app's side (ADR 0008): the File Provider extension
//! forwards every call of the system here (`theca_fileprovider::protocol`),
//! and the app answers from the drive's entries. No plan, no scan: the
//! system holds the local side. Local changes go to the server at once (an
//! upload, a move, the trash) and are recorded; the feed's are applied to the
//! entries (`apply_remote`), each stamped with the pair's counter, and the
//! system is told to ask for them.
//!
//! The answering is plain Rust (tested on every OS against the real
//! server); only the socket and the system's calls are macOS's.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use theca_client::{
    Cancel, Client, DownloadOptions, Downloader, Error, FileSource, Sink, Target, UploadOptions,
};
use theca_common::api;
use theca_common::name::Name;
use theca_fileprovider::protocol::{Code, Item, ROOT, Reply, Request, WORKING_SET};
use tracing::Instrument;
use uuid::Uuid;

use super::lock;
use crate::state::{Entry, Pair, State};

/// The one domain every drive is listed in (the state's `drives_id`).
pub fn domain_of(state: &State) -> crate::state::Result<String> {
    Ok(state.drives_id()?.simple().to_string())
}

/// An item's identifier in the one domain: `<pair uuid>/<node>`; a drive's
/// folder, the library's root, `<pair uuid>/`. A pair's UUID, not its ID:
/// the ID is reused after a removal, and the system keeps identifiers in its
/// own database.
pub fn item_id(pair: Uuid, node: Option<Uuid>) -> String {
    match node {
        Some(node) => format!("{pair}/{node}"),
        None => format!("{pair}/"),
    }
}

/// The pair and node an identifier names (no node: a drive's folder).
fn parse_id(id: &str) -> Option<(Uuid, Option<Uuid>)> {
    let (pair, node) = id.split_once('/')?;
    let pair = pair.parse().ok()?;
    if node.is_empty() {
        Some((pair, None))
    } else {
        Some((pair, Some(node.parse().ok()?)))
    }
}

/// The anchor the system keeps: every drive's counter, `<pair uuid>:<seq>`
/// joined by commas, in the order of the UUIDs.
fn anchor_text(seqs: &[(Uuid, i64)]) -> String {
    let mut seqs = seqs.to_vec();
    seqs.sort();
    seqs.iter()
        .map(|(pair, seq)| format!("{pair}:{seq}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// What an anchor says, or none when it isn't one this app gave.
fn parse_anchor(text: &str) -> Option<HashMap<Uuid, i64>> {
    if text.is_empty() {
        return Some(HashMap::new());
    }
    text.split(',')
        .map(|part| {
            let (pair, seq) = part.split_once(':')?;
            Some((pair.parse().ok()?, seq.parse().ok()?))
        })
        .collect()
}

/// A drive the app serves: its pair, its account's client.
struct Drive {
    pair: i64,
    client: Client,
    state: Arc<State>,
    cancel: Cancel,
    /// Syncing: false while it is paused or stopped, which still lists and
    /// opens what is here, but fetches and uploads nothing (the system tries
    /// again).
    active: bool,
    /// The system has been told of its folder (a listing of the domain, or the
    /// changes since an anchor): until then the anchor leaves it out, so that
    /// the first changes the system asks for bring the folder, which a system
    /// that never lists the domain (a test reading it as files) would not.
    announced: bool,
}

/// The drives this process lists in the one domain, by pair UUID, and what
/// the system has downloaded of each (the drive limit evicts from it).
#[derive(Default)]
pub struct Provider {
    drives: Mutex<HashMap<Uuid, Drive>>,
    materialized: Mutex<HashMap<Uuid, HashSet<Uuid>>>,
}

/// Why a call failed, as the extension tells the system.
struct Failed(Code, String);

impl From<Failed> for Reply {
    fn from(Failed(code, message): Failed) -> Self {
        Reply::Error { code, message }
    }
}

fn failed(code: Code, message: impl Into<String>) -> Failed {
    Failed(code, message.into())
}

fn from_client(e: &Error) -> Failed {
    let code = match e {
        Error::Api { status: 404, .. } => Code::NoSuchItem,
        Error::Api { status: 401, .. } => Code::NotAuthenticated,
        e if e.transient() => Code::ServerUnreachable,
        _ => Code::CannotSynchronize,
    };
    failed(code, e.to_string())
}

fn from_state(e: &crate::state::StateError) -> Failed {
    failed(Code::ServerUnreachable, e.to_string())
}

/// A drive's folder in the one domain: its name there is its folder's, the
/// last component of its path.
fn folder_name(pair: &Pair) -> String {
    pair.path.file_name().map_or_else(
        || pair.library_name.clone(),
        |n| n.to_string_lossy().into_owned(),
    )
}

impl Provider {
    /// A drive listed from now on (its engine started): `active` while it
    /// syncs, not when it is paused or stopped.
    pub fn serve(
        &self,
        pair: &Pair,
        uuid: Uuid,
        client: Client,
        state: Arc<State>,
        cancel: Cancel,
        active: bool,
    ) {
        let mut drives = lock(&self.drives);
        let announced = drives.get(&uuid).is_some_and(|d| d.announced);
        drives.insert(
            uuid,
            Drive {
                pair: pair.id,
                client,
                state,
                cancel,
                active,
                announced,
            },
        );
    }

    /// A drive no longer listed (removed, or its engine ended).
    pub fn forget(&self, uuid: Uuid) {
        lock(&self.drives).remove(&uuid);
        lock(&self.materialized).remove(&uuid);
    }

    /// What the system has downloaded of a drive, as it last said.
    pub fn materialized(&self, uuid: Uuid) -> HashSet<Uuid> {
        lock(&self.materialized)
            .get(&uuid)
            .cloned()
            .unwrap_or_default()
    }

    /// The answer to one call of the system.
    pub async fn answer(&self, request: Request) -> Reply {
        let span = tracing::info_span!("request", domain = request.domain());
        async move {
            tracing::debug!(?request, "asked");
            let reply = self.answer_in(request).await;
            // No such item is an answer the system expects (its trash, an
            // item gone): only at debug.
            match &reply {
                Reply::Error {
                    code: Code::NoSuchItem,
                    message,
                } => {
                    tracing::debug!("no such item: {message}")
                }
                Reply::Error { code, message } => tracing::warn!(?code, "refused: {message}"),
                _ => {}
            }
            reply
        }
        .instrument(span)
        .await
    }

    /// The drives and what they hold, as the system asks: the whole domain's
    /// calls (the drives folder, the anchors, what is materialized), then one
    /// drive's, named by the item's identifier.
    async fn answer_in(&self, request: Request) -> Reply {
        let named = match &request {
            Request::Item { id, .. } if id == ROOT => return Reply::Item(top_item()),
            Request::List { container, .. } if container == ROOT => {
                return self.drives_listed().unwrap_or_else(Reply::from);
            }
            Request::List { container, .. } if container == WORKING_SET => {
                return Reply::Items { items: Vec::new() };
            }
            Request::Anchor { .. } => return self.anchor().unwrap_or_else(Reply::from),
            Request::Changes { anchor, .. } => {
                return self.changes(anchor).unwrap_or_else(Reply::from);
            }
            Request::Materialized { ids, .. } => {
                self.materialized_ids(ids);
                return Reply::Done;
            }
            Request::Item { id, .. }
            | Request::Fetch { id, .. }
            | Request::Delete { id, .. }
            | Request::Modify { id, .. } => vec![id.as_str()],
            Request::List { container, .. } => vec![container.as_str()],
            Request::Create { parent, .. } => vec![parent.as_str()],
            Request::Keep { ids, .. } | Request::Free { ids, .. } => {
                ids.iter().map(String::as_str).collect()
            }
        };
        let uuid = match self.one_drive(&named, &request) {
            Ok(uuid) => uuid,
            Err(refused) => return refused.into(),
        };
        let found = lock(&self.drives).get(&uuid).map(|d| {
            (
                d.pair,
                d.client.clone(),
                d.state.clone(),
                d.cancel.clone(),
                d.active,
            )
        });
        let Some((pair, client, state, cancel, active)) = found else {
            // One out of the state (removed) is gone: told so, the system drops
            // what it holds of it instead of asking again for ever. One merely
            // not listed here (its account signed out, its engine not started
            // yet) is unreachable for now.
            let removed = lock(&self.drives)
                .values()
                .next()
                .is_some_and(|d| matches!(d.state.pair_by_uuid(uuid), Ok(None)));
            return if removed {
                failed(Code::NoSuchItem, "the drive was removed")
            } else {
                failed(
                    Code::ServerUnreachable,
                    "Theca doesn't serve this drive now",
                )
            }
            .into();
        };
        let pair = match state.pair(pair) {
            Ok(Some(p)) => p,
            Ok(None) => return failed(Code::NoSuchItem, "the drive was removed").into(),
            Err(e) => return from_state(&e).into(),
        };
        // Paused or stopped: what is here opens, nothing goes to the server
        // (the system tries again once it syncs).
        let changes_the_server = match &request {
            Request::Fetch { .. } | Request::Create { .. } | Request::Delete { .. } => true,
            Request::Modify {
                name,
                parent,
                contents,
                ..
            } => name.is_some() || parent.is_some() || contents.is_some(),
            _ => false,
        };
        if !active && changes_the_server {
            return failed(
                Code::ServerUnreachable,
                "this drive isn't syncing (paused, or stopped)",
            )
            .into();
        }
        let answered = Answer {
            pair: &pair,
            uuid,
            state: &state,
            client: &client,
            cancel: &cancel,
        };
        let freed = match &request {
            Request::Free { ids, .. } => ids.clone(),
            _ => Vec::new(),
        };
        let reply = answered.answer(request).await;
        // Finder's Free Up Space: not kept anymore (answered), then evicted
        // as `theca free` does, on a thread of its own (the system re-reads
        // the items first, and the extension waits for no eviction).
        #[cfg(target_os = "macos")]
        if reply == Reply::Done && !freed.is_empty() {
            let (state, pair) = (state.clone(), pair.clone());
            let span = tracing::Span::current();
            std::thread::spawn(move || {
                let _in = span.enter();
                for id in freed {
                    let Some(entry) = parse_id(&id)
                        .and_then(|(_, node)| node)
                        .and_then(|n| state.entry(pair.id, n).ok().flatten())
                    else {
                        continue;
                    };
                    if let Err(e) = crate::ops::free_on_mac(&state, &pair, entry) {
                        tracing::warn!(item = id, "not freed: {e}");
                    }
                }
            });
        }
        #[cfg(not(target_os = "macos"))]
        let _ = freed;
        reply
    }

    /// The drive a call is about: the first identifier's (an item of another
    /// drive in the same call is refused as no item there: two libraries), and
    /// a drive's folder is nothing to rename, move or delete.
    fn one_drive(&self, ids: &[&str], request: &Request) -> Result<Uuid, Failed> {
        let Some(first) = ids.first() else {
            return Err(failed(Code::NoSuchItem, "no item named"));
        };
        let Some((uuid, _)) = parse_id(first) else {
            return Err(failed(Code::NoSuchItem, format!("no item {first}")));
        };
        let a_drive = |id: &str| matches!(parse_id(id), Some((_, None)));
        match request {
            Request::Modify {
                id, name, parent, ..
            } if a_drive(id) && (name.is_some() || parent.is_some()) => {
                return Err(failed(
                    Code::CannotSynchronize,
                    "a drive isn't renamed nor moved",
                ));
            }
            Request::Delete { id, .. } if a_drive(id) => {
                return Err(failed(
                    Code::CannotSynchronize,
                    "a drive isn't deleted here",
                ));
            }
            // Moved into another drive, or out to the drives folder.
            Request::Modify {
                parent: Some(parent),
                ..
            } if parse_id(parent).map(|(p, _)| p) != Some(uuid) => {
                return Err(failed(
                    Code::CannotSynchronize,
                    "an item stays in its drive (its library)",
                ));
            }
            Request::Create { parent, .. } if parent == ROOT => {
                return Err(failed(
                    Code::CannotSynchronize,
                    "nothing is made at the top: make it in a drive",
                ));
            }
            _ => {}
        }
        Ok(uuid)
    }

    /// The drives, each a folder, named as it is where Theca keeps it.
    fn drives_listed(&self) -> Result<Reply, Failed> {
        let mut items = Vec::new();
        for (uuid, drive) in lock(&self.drives).iter_mut() {
            let pair = drive
                .state
                .pair(drive.pair)
                .map_err(|e| from_state(&e))?
                .ok_or_else(|| failed(Code::NoSuchItem, "the drive was removed"))?;
            drive.announced = true;
            items.push(drive_item(&pair, *uuid));
        }
        items.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Reply::Items { items })
    }

    fn materialized_ids(&self, ids: &[String]) {
        let mut by_drive: HashMap<Uuid, HashSet<Uuid>> = HashMap::new();
        for (pair, node) in ids.iter().filter_map(|i| parse_id(i)) {
            if let Some(node) = node {
                by_drive.entry(pair).or_default().insert(node);
            }
        }
        // The system says all of them at once: a drive it holds nothing of
        // has none.
        let mut table = lock(&self.materialized);
        table.clear();
        table.extend(by_drive);
    }

    /// Every drive's counter now.
    fn anchor(&self) -> Result<Reply, Failed> {
        let mut seqs = Vec::new();
        for (uuid, drive) in lock(&self.drives).iter().filter(|(_, d)| d.announced) {
            let (_, _, now) = drive
                .state
                .changes_since(drive.pair, i64::MAX)
                .map_err(|e| from_state(&e))?;
            seqs.push((*uuid, now));
        }
        Ok(Reply::Anchor {
            anchor: anchor_text(&seqs),
        })
    }

    /// What changed in every drive past an anchor. A drive the anchor doesn't
    /// have is new: its folder and everything in it; one it has that is no
    /// longer listed is gone: its folder.
    fn changes(&self, anchor: &str) -> Result<Reply, Failed> {
        let Some(known) = parse_anchor(anchor) else {
            return Err(failed(
                Code::SyncAnchorExpired,
                "an anchor this app never gave",
            ));
        };
        let mut updated = Vec::new();
        let mut deleted = Vec::new();
        let mut seqs = Vec::new();
        let mut drives = lock(&self.drives);
        // Removed from the state while still listed (its engine hasn't looked
        // yet): gone, as if it were not listed.
        let mut removed = HashSet::new();
        for (uuid, drive) in drives.iter_mut() {
            let Some(pair) = drive.state.pair(drive.pair).map_err(|e| from_state(&e))? else {
                removed.insert(*uuid);
                continue;
            };
            drive.announced = true;
            let answer = Answer {
                pair: &pair,
                uuid: *uuid,
                state: &drive.state,
                client: &drive.client,
                cancel: &drive.cancel,
            };
            let from = match known.get(uuid) {
                Some(seq) => *seq,
                None => {
                    // New to the system: the folder, and what is in it.
                    updated.push(drive_item(&pair, *uuid));
                    -1
                }
            };
            let (changed, gone, now) = drive
                .state
                .changes_since(drive.pair, from)
                .map_err(|e| from_state(&e))?;
            if now < from {
                // An anchor from before this state (a lost state, reinstalled):
                // the system lists again.
                return Err(failed(
                    Code::SyncAnchorExpired,
                    "an anchor this state never gave",
                ));
            }
            let kept = answer.kept()?;
            for e in &changed {
                updated.push(answer.shown(e, &kept)?);
            }
            deleted.extend(gone.iter().map(|n| item_id(*uuid, Some(*n))));
            seqs.push((*uuid, now));
        }
        // A drive the system knew that is no drive anymore (out of the state): its
        // folder is gone. One this process merely doesn't list (its account signed
        // out, its engine not started yet) stays: the system keeps what it holds
        // of it, and learns of it whole when it is listed again.
        let state = drives.values().next().map(|d| d.state.clone());
        deleted.extend(
            known
                .keys()
                .filter(|uuid| !drives.contains_key(uuid) || removed.contains(uuid))
                .filter(|uuid| {
                    state
                        .as_ref()
                        .is_some_and(|s| matches!(s.pair_by_uuid(**uuid), Ok(None)))
                })
                .map(|uuid| item_id(*uuid, None)),
        );
        Ok(Reply::Changes {
            updated,
            deleted,
            anchor: anchor_text(&seqs),
        })
    }
}

/// The drives folder, as the system's root item.
fn top_item() -> Item {
    Item {
        id: ROOT.into(),
        parent: ROOT.into(),
        name: "Theca".into(),
        folder: true,
        size: 0,
        version: String::new(),
        modified: None,
        kept: false,
        kept_itself: false,
        writable: false,
        fixed: true,
    }
}

/// A drive's folder in the drives folder.
fn drive_item(pair: &Pair, uuid: Uuid) -> Item {
    Item {
        id: item_id(uuid, None),
        parent: ROOT.into(),
        name: folder_name(pair),
        folder: true,
        size: 0,
        version: String::new(),
        modified: None,
        kept: false,
        kept_itself: false,
        writable: pair.writable,
        fixed: true,
    }
}

struct Answer<'a> {
    pair: &'a Pair,
    /// Its UUID: what its items' identifiers start with.
    uuid: Uuid,
    state: &'a State,
    client: &'a Client,
    cancel: &'a Cancel,
}

impl Answer<'_> {
    async fn answer(&self, request: Request) -> Reply {
        let answered = match request {
            Request::Item { id, .. } => self.item(&id),
            Request::List { container, .. } => self.list(&container),
            // The whole domain's, answered by the provider.
            Request::Anchor { .. } | Request::Changes { .. } | Request::Materialized { .. } => {
                Err(failed(Code::CannotSynchronize, "a call for the domain"))
            }
            Request::Fetch { id, into, .. } => self.fetch(&id, &into).await,
            Request::Create {
                parent,
                name,
                folder,
                contents,
                ..
            } => {
                self.create(&parent, &name, folder, contents.as_deref())
                    .await
            }
            Request::Modify {
                id,
                base,
                name,
                parent,
                contents,
                used,
                ..
            } => {
                self.modify(&id, base, name, parent, contents.as_deref(), used)
                    .await
            }
            Request::Delete { id, .. } => self.delete(&id).await,
            Request::Keep { ids, .. } => self.keep(&ids, true),
            // Its eviction follows the answer (`Provider::answer`).
            Request::Free { ids, .. } => self.keep(&ids, false),
        };
        answered.unwrap_or_else(Reply::from)
    }

    /// An item's node (a drive's folder: the library's root).
    fn node(&self, id: &str) -> Result<Uuid, Failed> {
        match parse_id(id) {
            Some((pair, None)) if pair == self.uuid => Ok(self.pair.root),
            Some((pair, Some(node))) if pair == self.uuid => Ok(node),
            _ => Err(failed(Code::NoSuchItem, format!("no item {id}"))),
        }
    }

    fn entry(&self, node: Uuid) -> Result<Entry, Failed> {
        self.state
            .entry(self.pair.id, node)
            .map_err(|e| from_state(&e))?
            .ok_or_else(|| failed(Code::NoSuchItem, format!("no item {node}")))
    }

    /// The drive's folder.
    fn root(&self) -> Item {
        drive_item(self.pair, self.uuid)
    }

    /// An entry as the system shows it: kept if it or a folder above is.
    fn shown(&self, e: &Entry, kept: &HashSet<Uuid>) -> Result<Item, Failed> {
        let is_kept = kept_here(self.state, self.pair, e, kept).map_err(|e| from_state(&e))?;
        Ok(Item {
            id: item_id(self.uuid, Some(e.node)),
            parent: if e.parent == self.pair.root {
                item_id(self.uuid, None)
            } else {
                item_id(self.uuid, Some(e.parent))
            },
            name: e.name.clone(),
            folder: !e.is_file,
            size: u64::try_from(e.size).unwrap_or_default(),
            version: e.version.map(|v| v.to_string()).unwrap_or_default(),
            modified: (e.mtime != 0).then(|| e.mtime.div_euclid(1_000_000_000)),
            kept: is_kept,
            kept_itself: kept.contains(&e.node),
            writable: self.pair.writable,
            fixed: false,
        })
    }

    fn kept(&self) -> Result<HashSet<Uuid>, Failed> {
        self.state.kept(self.pair.id).map_err(|e| from_state(&e))
    }

    fn item(&self, id: &str) -> Result<Reply, Failed> {
        if matches!(parse_id(id), Some((_, None))) {
            return Ok(Reply::Item(self.root()));
        }
        let e = self.entry(self.node(id)?)?;
        Ok(Reply::Item(self.shown(&e, &self.kept()?)?))
    }

    fn list(&self, container: &str) -> Result<Reply, Failed> {
        let parent = self.node(container)?;
        let kept = self.kept()?;
        let children = self
            .state
            .children(self.pair.id, parent)
            .map_err(|e| from_state(&e))?;
        let items = children
            .iter()
            .map(|e| self.shown(e, &kept))
            .collect::<Result<_, _>>()?;
        Ok(Reply::Items { items })
    }

    /// The item's version (the entry's) downloaded into `into`.
    async fn fetch(&self, id: &str, into: &Path) -> Result<Reply, Failed> {
        let e = self.entry(self.node(id)?)?;
        let sink = IntoFile::new(into);
        let options = DownloadOptions::default();
        let (downloader, run) = Downloader::new(self.client, &options, self.cancel, |_| {});
        let fetched = downloader.submit(e.node, e.version, &sink);
        drop(downloader);
        let (_, result) = futures::join!(run, fetched);
        result.map_err(|err| from_client(&err))?;
        let now = chrono::Utc::now().timestamp();
        self.state
            .set_used(self.pair.id, e.node, now)
            .map_err(|err| from_state(&err))?;
        Ok(Reply::Item(self.shown(&e, &self.kept()?)?))
    }

    fn writable(&self) -> Result<(), Failed> {
        if self.pair.writable {
            Ok(())
        } else {
            Err(failed(Code::CannotSynchronize, "this library is read-only"))
        }
    }

    fn name(name: &str) -> Result<Name, Failed> {
        Name::new(name).map_err(|e| failed(Code::CannotSynchronize, e.to_string()))
    }

    /// A file's content uploaded: to `target`, recorded with its version.
    async fn upload(&self, contents: &Path, target: Target) -> Result<(Uuid, Uuid, i64), Failed> {
        let source = FileSource::open(contents)
            .map_err(|e| failed(Code::CannotSynchronize, e.to_string()))?;
        let size = std::fs::metadata(contents).map_or(0, |m| m.len() as i64);
        let committed = theca_client::upload(
            self.client,
            &source,
            target,
            &UploadOptions::default(),
            |_| {},
            self.cancel,
        )
        .await
        .map_err(|e| from_client(&e))?;
        Ok((committed.file, committed.version, size))
    }

    fn stamped(&self, e: Entry) -> Result<Reply, Failed> {
        self.state
            .stamp(self.pair.id, &e)
            .map_err(|err| from_state(&err))?;
        Ok(Reply::Item(self.shown(&e, &self.kept()?)?))
    }

    async fn create(
        &self,
        parent: &str,
        name: &str,
        folder: bool,
        contents: Option<&Path>,
    ) -> Result<Reply, Failed> {
        self.writable()?;
        let parent = self.node(parent)?;
        let valid = Self::name(name)?;
        let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
        if folder {
            let created = self
                .client
                .create_folders(parent, std::slice::from_ref(&name.to_owned()))
                .await
                .map_err(|e| from_client(&e))?;
            let node = created
                .first()
                .map(|n| n.id)
                .ok_or_else(|| failed(Code::CannotSynchronize, "no folder created"))?;
            return self.stamped(Entry {
                node,
                parent,
                name: valid.as_str().to_owned(),
                is_file: false,
                version: None,
                size: 0,
                mtime: now,
                file_id: None,
            });
        }
        let empty;
        let contents = match contents {
            Some(path) => path,
            // An empty file: no content handed over.
            None => {
                empty = tempfile::NamedTempFile::new()
                    .map_err(|e| failed(Code::CannotSynchronize, e.to_string()))?;
                empty.path()
            }
        };
        let target = Target::New {
            library: self.pair.library,
            parent,
            name: valid.clone(),
        };
        let (node, version, size) = self.upload(contents, target).await?;
        self.stamped(Entry {
            node,
            parent,
            name: valid.as_str().to_owned(),
            is_file: true,
            version: Some(version),
            size,
            mtime: now,
            file_id: None,
        })
    }

    async fn modify(
        &self,
        id: &str,
        base: Option<String>,
        name: Option<String>,
        parent: Option<String>,
        contents: Option<&Path>,
        used: Option<i64>,
    ) -> Result<Reply, Failed> {
        // A drive's folder has nothing to change here (a rename or a move was
        // refused before): the system touching it (its attributes) is told it
        // is as it is, or the change would wait for ever.
        if matches!(parse_id(id), Some((_, None))) {
            return self.item(id);
        }
        let node = self.node(id)?;
        let mut e = self.entry(node)?;
        if let Some(at) = used {
            self.state
                .set_used(self.pair.id, node, at)
                .map_err(|err| from_state(&err))?;
        }
        if contents.is_none() && name.is_none() && parent.is_none() {
            return Ok(Reply::Item(self.shown(&e, &self.kept()?)?));
        }
        self.writable()?;
        if let Some(contents) = contents {
            // On the version the edit was made on: the server forks the
            // version tree if another came meanwhile.
            let base = base.and_then(|b| b.parse().ok()).or(e.version);
            let target = Target::Version {
                library: self.pair.library,
                file: node,
                base,
            };
            let (_, version, size) = self.upload(contents, target).await?;
            e.version = Some(version);
            e.size = size;
            e.mtime = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
        }
        if name.is_some() || parent.is_some() {
            let new_parent = match &parent {
                Some(p) => self.node(p)?,
                None => e.parent,
            };
            let new_name = name.unwrap_or_else(|| e.name.clone());
            let update = api::UpdateNode {
                id: node,
                name: Some(Self::name(&new_name)?),
                parent: Some(new_parent),
                replace: false,
            };
            let mut results = self
                .client
                .update_nodes(&[update])
                .await
                .map_err(|err| from_client(&err))?;
            match results.pop() {
                Some(Ok(_)) => {
                    e.parent = new_parent;
                    e.name = new_name;
                }
                Some(Err(err)) => return Err(from_client(&err)),
                None => return Err(failed(Code::CannotSynchronize, "not moved")),
            }
        }
        self.stamped(e)
    }

    /// Finder's Always Keep on This Mac (or, for Free Up Space, not kept
    /// anymore): all the items, or none when one
    /// can't be (the drive itself, an item not in the library). The
    /// extension then signals the system, which reads the items again.
    fn keep(&self, ids: &[String], kept: bool) -> Result<Reply, Failed> {
        let mut nodes = Vec::with_capacity(ids.len());
        for id in ids {
            let node = self.node(id)?;
            if node == self.pair.root {
                return Err(failed(
                    Code::CannotSynchronize,
                    "keep what is in the drive, not the drive itself",
                ));
            }
            nodes.push(self.entry(node)?.node);
        }
        for node in nodes {
            self.state
                .set_kept(self.pair.id, node, kept)
                .map_err(|e| from_state(&e))?;
        }
        Ok(Reply::Done)
    }

    /// To the server's trash, what is below with it.
    async fn delete(&self, id: &str) -> Result<Reply, Failed> {
        self.writable()?;
        let node = self.node(id)?;
        let results = self
            .client
            .trash_nodes(&[node], false)
            .await
            .map_err(|e| from_client(&e))?;
        match results.into_iter().next() {
            Some(Ok(())) | Some(Err(Error::Api { status: 404, .. })) => {}
            Some(Err(e)) => return Err(from_client(&e)),
            None => return Err(failed(Code::CannotSynchronize, "not trashed")),
        }
        self.state
            .stamp_gone(self.pair.id, node)
            .map_err(|e| from_state(&e))?;
        Ok(Reply::Done)
    }
}

/// A drive's files kept on this computer (themselves, or below a folder
/// kept), with their paths in its folder.
pub fn kept_files(state: &State, pair: &Pair) -> crate::state::Result<Vec<(Uuid, PathBuf)>> {
    let kept = state.kept(pair.id)?;
    if kept.is_empty() {
        return Ok(Vec::new());
    }
    let entries = state.entries(pair.id)?;
    let mut files = Vec::new();
    for (path, e) in crate::drive::paths(&entries, pair.root) {
        if e.is_file && kept_here(state, pair, e, &kept)? {
            files.push((e.node, pair.path.join(path)));
        }
    }
    Ok(files)
}

/// Whether a drive's file is still kept here (asked again before a
/// download started a while ago: it may have been freed since).
pub fn still_kept(state: &State, pair: &Pair, node: Uuid) -> crate::state::Result<bool> {
    match state.entry(pair.id, node)? {
        Some(e) => kept_here(state, pair, &e, &state.kept(pair.id)?),
        None => Ok(false),
    }
}

/// A drive's entry kept on this computer: it, or a folder above it, is.
pub fn kept_here(
    state: &State,
    pair: &Pair,
    e: &Entry,
    kept: &HashSet<Uuid>,
) -> crate::state::Result<bool> {
    let mut node = e.node;
    let mut up = e.parent;
    for _ in 0..crate::plan::MAX_DEPTH {
        if kept.contains(&node) {
            return Ok(true);
        }
        if up == pair.root {
            return Ok(false);
        }
        node = up;
        match state.entry(pair.id, up)? {
            Some(p) => up = p.parent,
            None => return Ok(false),
        }
    }
    Ok(false)
}

/// A download written into the file the extension named (removed if it
/// fails).
struct IntoFile {
    path: PathBuf,
    file: Mutex<Option<std::fs::File>>,
}

impl IntoFile {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            file: Mutex::new(None),
        }
    }

    fn opened(&self) -> io::Result<MutexGuard<'_, Option<std::fs::File>>> {
        let mut file = self.file.lock().map_err(|_| io::Error::other("poisoned"))?;
        if file.is_none() {
            *file = Some(std::fs::File::create(&self.path)?);
        }
        Ok(file)
    }
}

impl Sink for IntoFile {
    fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = self.opened()?;
        let file = file.as_mut().expect("opened above");
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)
    }

    fn finish(&self) -> io::Result<()> {
        let file = self.opened()?;
        file.as_ref().expect("opened above").sync_all()
    }

    fn abort(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A macOS drive's round: what the feed brought, applied to its entries,
/// each stamped with the pair's counter (nothing local to compare: the
/// system holds it). True when something changed (the system is told).
pub fn apply_remote(state: &State, pair: &Pair) -> crate::state::Result<bool> {
    let pending = state.remote(pair.id)?;
    if pending.is_empty() {
        return Ok(false);
    }
    let mut settled = Vec::new();
    for (node, remote) in &pending {
        match remote {
            Some(s) => state.stamp(
                pair.id,
                &Entry {
                    node: *node,
                    parent: s.parent,
                    name: s.name.clone(),
                    is_file: s.is_file,
                    version: s.version,
                    size: s.size,
                    mtime: s
                        .modified
                        .and_then(|m| m.timestamp_nanos_opt())
                        .unwrap_or_default(),
                    file_id: None,
                },
            )?,
            None => state.stamp_gone(pair.id, *node)?,
        }
        settled.push(*node);
    }
    state.clear_remote(pair.id, &settled)?;
    Ok(true)
}

/// The socket in the extension's container, served by this process (once,
/// whatever the drives: each request names its domain). The container exists
/// once the system has started the extension: waited for.
#[cfg(target_os = "macos")]
pub fn listen(provider: Arc<Provider>, runtime: tokio::runtime::Handle) {
    use std::os::unix::net::UnixListener;
    use theca_fileprovider::protocol::SOCKET;
    let Some(container) = theca_fileprovider::host::container() else {
        return;
    };
    std::thread::spawn(move || {
        let listener = loop {
            if container.is_dir() {
                let path = container.join(SOCKET);
                let _ = std::fs::remove_file(&path);
                match UnixListener::bind(&path) {
                    Ok(listener) => break listener,
                    Err(e) => tracing::warn!("the drive's socket not open: {e}"),
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        };
        for stream in listener.incoming().flatten() {
            let (provider, runtime) = (provider.clone(), runtime.clone());
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let mut stream = stream;
                let mut asked = Vec::new();
                if stream.read_to_end(&mut asked).is_err() {
                    return;
                }
                let reply = match serde_json::from_slice::<Request>(&asked) {
                    Ok(request) => runtime.block_on(provider.answer(request)),
                    Err(e) => failed(Code::CannotSynchronize, format!("not a request: {e}")).into(),
                };
                if let Ok(line) = serde_json::to_vec(&reply) {
                    let _ = stream.write_all(&line);
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An item's identifier names its drive and its node, a drive's folder
    /// no node; anything else names nothing.
    #[test]
    fn an_identifier_names_its_drive_and_its_node() {
        let (pair, node) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(
            parse_id(&item_id(pair, Some(node))),
            Some((pair, Some(node)))
        );
        assert_eq!(parse_id(&item_id(pair, None)), Some((pair, None)));
        for bad in [
            "",
            ROOT,
            WORKING_SET,
            &node.to_string(),
            "a/b",
            &format!("{pair}/x"),
        ] {
            assert_eq!(parse_id(bad), None, "{bad:?}");
        }
    }

    /// The anchor lists every drive's counter, in one order whatever the
    /// order they came in, and reads back; what isn't one is refused.
    #[test]
    fn an_anchor_reads_back() {
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let text = anchor_text(&[(b, 7), (a, 3)]);
        assert_eq!(text, anchor_text(&[(a, 3), (b, 7)]));
        assert_eq!(parse_anchor(&text), Some(HashMap::from([(a, 3), (b, 7)])));
        assert_eq!(parse_anchor(""), Some(HashMap::new()), "no drive yet");
        for bad in ["nonsense", "x:1", &format!("{a}:x"), &format!("{a}:1,")] {
            assert_eq!(parse_anchor(bad), None, "{bad:?}");
        }
    }
}
