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

use crate::state::{Entry, Pair, State};

/// A drive's domain: named after its library (unique on the computer).
pub fn domain_of(pair: &Pair) -> String {
    pair.library.simple().to_string()
}

/// A drive the app serves: its pair, its account's client.
struct Drive {
    pair: i64,
    client: Client,
    state: Arc<State>,
    cancel: Cancel,
}

/// The drives this process serves, by domain, and what the system has
/// downloaded of each (the drive limit evicts from it).
#[derive(Default)]
pub struct Provider {
    drives: Mutex<HashMap<String, Drive>>,
    materialized: Mutex<HashMap<String, HashSet<Uuid>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
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

impl Provider {
    /// A drive served from now on (its engine started).
    pub fn serve(&self, pair: &Pair, client: Client, state: Arc<State>, cancel: Cancel) {
        lock(&self.drives).insert(
            domain_of(pair),
            Drive {
                pair: pair.id,
                client,
                state,
                cancel,
            },
        );
    }

    /// A drive no longer served (its engine stopped, the drive removed).
    pub fn forget(&self, pair: &Pair) {
        lock(&self.drives).remove(&domain_of(pair));
    }

    /// What the system has downloaded of a drive, as it last said.
    pub fn materialized(&self, pair: &Pair) -> HashSet<Uuid> {
        lock(&self.materialized)
            .get(&domain_of(pair))
            .cloned()
            .unwrap_or_default()
    }

    /// The answer to one call of the system.
    pub async fn answer(&self, request: Request) -> Reply {
        let span = tracing::info_span!("request", domain = request.domain());
        async move {
            tracing::debug!(?request, "asked");
            let reply = self.answer_in(request).await;
            if let Reply::Error { code, message } = &reply {
                tracing::warn!(?code, "refused: {message}");
            }
            reply
        }
        .instrument(span)
        .await
    }

    async fn answer_in(&self, request: Request) -> Reply {
        let found = lock(&self.drives)
            .get(request.domain())
            .map(|d| (d.pair, d.client.clone(), d.state.clone(), d.cancel.clone()));
        let Some((pair, client, state, cancel)) = found else {
            return failed(
                Code::ServerUnreachable,
                "Theca doesn't serve this drive now",
            )
            .into();
        };
        let pair = match state.pair(pair) {
            Ok(Some(p)) => p,
            Ok(None) => return failed(Code::NoSuchItem, "the drive was removed").into(),
            Err(e) => return from_state(&e).into(),
        };
        let answered = Answer {
            pair: &pair,
            state: &state,
            client: &client,
            cancel: &cancel,
        };
        if let Request::Materialized { ids, .. } = &request {
            let ids = ids.iter().filter_map(|i| i.parse().ok()).collect();
            lock(&self.materialized).insert(domain_of(&pair), ids);
            return Reply::Done;
        }
        answered.answer(request).await
    }
}

struct Answer<'a> {
    pair: &'a Pair,
    state: &'a State,
    client: &'a Client,
    cancel: &'a Cancel,
}

impl Answer<'_> {
    async fn answer(&self, request: Request) -> Reply {
        let answered = match request {
            Request::Item { id, .. } => self.item(&id),
            Request::List { container, .. } => self.list(&container),
            Request::Anchor { .. } => self.anchor(),
            Request::Changes { anchor, .. } => self.changes(anchor),
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
            Request::Materialized { .. } => Ok(Reply::Done),
            Request::Keep { ids, kept, .. } => self.keep(&ids, kept),
        };
        answered.unwrap_or_else(Reply::from)
    }

    /// An item's node (`ROOT`: the library's root).
    fn node(&self, id: &str) -> Result<Uuid, Failed> {
        if id == ROOT {
            return Ok(self.pair.root);
        }
        id.parse()
            .map_err(|_| failed(Code::NoSuchItem, format!("no item {id}")))
    }

    fn entry(&self, node: Uuid) -> Result<Entry, Failed> {
        self.state
            .entry(self.pair.id, node)
            .map_err(|e| from_state(&e))?
            .ok_or_else(|| failed(Code::NoSuchItem, format!("no item {node}")))
    }

    fn root(&self) -> Item {
        Item {
            id: ROOT.into(),
            parent: ROOT.into(),
            name: self.pair.library_name.clone(),
            folder: true,
            size: 0,
            version: String::new(),
            modified: None,
            kept: false,
            kept_itself: false,
            writable: self.pair.writable,
        }
    }

    /// An entry as the system shows it: kept if it or a folder above is.
    fn shown(&self, e: &Entry, kept: &HashSet<Uuid>) -> Result<Item, Failed> {
        let is_kept = kept_here(self.state, self.pair, e, kept).map_err(|e| from_state(&e))?;
        Ok(Item {
            id: e.node.to_string(),
            parent: if e.parent == self.pair.root {
                ROOT.into()
            } else {
                e.parent.to_string()
            },
            name: e.name.clone(),
            folder: !e.is_file,
            size: u64::try_from(e.size).unwrap_or_default(),
            version: e.version.map(|v| v.to_string()).unwrap_or_default(),
            modified: (e.mtime != 0).then(|| e.mtime.div_euclid(1_000_000_000)),
            kept: is_kept,
            kept_itself: kept.contains(&e.node),
            writable: self.pair.writable,
        })
    }

    fn kept(&self) -> Result<HashSet<Uuid>, Failed> {
        self.state.kept(self.pair.id).map_err(|e| from_state(&e))
    }

    fn item(&self, id: &str) -> Result<Reply, Failed> {
        if id == ROOT {
            return Ok(Reply::Item(self.root()));
        }
        let e = self.entry(self.node(id)?)?;
        Ok(Reply::Item(self.shown(&e, &self.kept()?)?))
    }

    fn list(&self, container: &str) -> Result<Reply, Failed> {
        // The working set's changes say everything: it lists nothing.
        if container == WORKING_SET {
            return Ok(Reply::Items { items: Vec::new() });
        }
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

    fn anchor(&self) -> Result<Reply, Failed> {
        let (_, _, now) = self
            .state
            .changes_since(self.pair.id, i64::MAX)
            .map_err(|e| from_state(&e))?;
        Ok(Reply::Anchor {
            anchor: u64::try_from(now).unwrap_or_default(),
        })
    }

    fn changes(&self, anchor: u64) -> Result<Reply, Failed> {
        let from = i64::try_from(anchor).unwrap_or(i64::MAX);
        let (changed, gone, now) = self
            .state
            .changes_since(self.pair.id, from)
            .map_err(|e| from_state(&e))?;
        if now < from {
            // An anchor from before this state (a lost state, reinstalled):
            // the system lists again.
            return Err(failed(
                Code::SyncAnchorExpired,
                "an anchor this state never gave",
            ));
        }
        let kept = self.kept()?;
        let updated = changed
            .iter()
            .map(|e| self.shown(e, &kept))
            .collect::<Result<_, _>>()?;
        Ok(Reply::Changes {
            updated,
            deleted: gone.iter().map(Uuid::to_string).collect(),
            anchor: u64::try_from(now).unwrap_or_default(),
        })
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

    /// Finder's Keep Downloaded (or Don't): all the items, or none when one
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
