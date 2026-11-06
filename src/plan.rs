//! Reconciliation, three-way and pure: what was last in sync (the base), what
//! the local folder holds now (a scan), what the server holds now (the base
//! with the remote states received since) give the actions that make both
//! sides identical (docs/decisions.md, Sync).
//!
//! - One side changed: it wins.
//! - Both changed a file: the local content is uploaded on the version it was
//!   based on; the server forks the version tree and makes it current.
//! - Deleted on one side, modified on the other: the modification wins.
//! - Deletes go to the server's trash; a folder whose content changed
//!   remotely is kept.
//! - Names that can't go up are reported, never renamed.
//!
//! Actions name nodes, not paths: the executor finds a node's local path from
//! the base as it updates it, so earlier actions (a move) are taken into
//! account by later ones.

use std::collections::{HashMap, HashSet};

use theca_common::name::Name;
use uuid::Uuid;

use crate::local::{self, Item, Scan, key};
use crate::state::{Entry, RemoteState};

/// A folder on the server: one already there, or one this plan creates (by
/// the local path of the folder it mirrors).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Parent {
    Node(Uuid),
    New(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// A local folder created on the server (parents first).
    RemoteMkdir {
        path: Vec<String>,
        parent: Parent,
        name: String,
    },
    /// A local move or rename done on the server.
    RemoteMove {
        node: Uuid,
        parent: Parent,
        name: String,
    },
    /// A local file uploaded: a new file in `parent`, or a version of `node`.
    Upload { path: Vec<String>, to: UploadTo },
    /// A remote move or rename done locally.
    LocalMove {
        node: Uuid,
        parent: Uuid,
        name: String,
    },
    /// A remote folder created locally.
    LocalMkdir {
        node: Uuid,
        parent: Uuid,
        name: String,
    },
    /// A remote file's version written locally (new, or over the base's
    /// content, which the executor checks is still there).
    Download {
        node: Uuid,
        parent: Uuid,
        name: String,
        state: RemoteState,
    },
    /// A file on both sides without a base (a first sync, a lost state):
    /// identical content is adopted, different content becomes a version on
    /// the remote one.
    Merge {
        node: Uuid,
        path: Vec<String>,
        state: RemoteState,
    },
    /// A folder on both sides without a base, or a read-only library's file
    /// as it was downloaded: adopted, nothing uploaded.
    Adopt {
        node: Uuid,
        path: Vec<String>,
        state: RemoteState,
    },
    /// A local delete done on the server (to its trash).
    RemoteTrash { node: Uuid },
    /// A remote delete done locally: a file only if it still holds the base's
    /// content, a folder only once empty.
    LocalDelete { node: Uuid },
    /// A read-only library's file changed on both sides: the local copy kept
    /// aside as `<name> (local)` before the download.
    KeepAside { node: Uuid },
    /// Forget a node (gone on both sides).
    Forget { node: Uuid },
}

#[derive(Debug, Clone, PartialEq)]
pub enum UploadTo {
    New {
        parent: Parent,
        name: String,
    },
    /// `base`: the version the local content was based on (the server forks
    /// the tree when it's no longer current).
    Version {
        node: Uuid,
        base: Option<Uuid>,
    },
}

/// What a plan reports without acting.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub path: String,
    pub message: String,
    pub level: Level,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Level {
    Error,
    Info,
}

#[derive(Debug, Default)]
pub struct Plan {
    /// In the order to run them.
    pub actions: Vec<Action>,
    pub reports: Vec<Report>,
    /// Remote states needing nothing (the same as the base, gone on both
    /// sides): cleared at once. The others are cleared by their action.
    pub settled: Vec<Uuid>,
}

pub struct Input<'a> {
    pub root: Uuid,
    pub base: &'a [Entry],
    pub scan: &'a Scan,
    pub remote: &'a [(Uuid, Option<RemoteState>)],
    pub writable: bool,
}

/// A node's place and content, on one side.
#[derive(Debug, Clone, PartialEq)]
struct Node {
    parent: Uuid,
    name: String,
    is_file: bool,
    version: Option<Uuid>,
}

/// What became of a base node locally.
#[derive(Debug, Clone, PartialEq)]
enum Local {
    Deleted,
    /// Where the scan couldn't look (a folder it couldn't read, a symbolic
    /// link, another disk): neither deleted nor changed, nothing done to it.
    Unseen,
    Present {
        item: usize,
        /// Its new place, when moved or renamed.
        moved: Option<(Parent, String)>,
        modified: bool,
    },
}

pub fn plan(input: &Input) -> Plan {
    Planner::new(input).run()
}

struct Planner<'a> {
    input: &'a Input<'a>,
    base: HashMap<Uuid, &'a Entry>,
    /// Remote view: the base with the remote states applied; absent: gone.
    remote: HashMap<Uuid, Node>,
    /// Pending remote states, by node.
    pending: HashMap<Uuid, Option<&'a RemoteState>>,
    /// Local item index → the base node it is.
    claimed: HashMap<usize, Uuid>,
    local: HashMap<Uuid, Local>,
    /// What the scan couldn't look into, by key path.
    unseen: Vec<Vec<String>>,
    plan: Plan,
}

/// A name as the server keeps them (valid, NFC): anything else came from a
/// server that shouldn't have sent it, and never reaches the disk (`..`, a
/// path, an absolute one would leave the folder).
fn sound(name: &str) -> bool {
    Name::new(name).is_ok_and(|n| n.as_str() == name)
}

/// Steps up a parent chain before it counts as a cycle: a guard a real tree
/// never reaches (so `>` or `>=` there, or a count that doesn't grow, changes
/// nothing a test can see).
const MAX_DEPTH: usize = 10_000;

fn nfc(name: &str) -> String {
    Name::new(name).map_or_else(|_| name.to_owned(), |n| n.as_str().to_owned())
}

impl<'a> Planner<'a> {
    fn new(input: &'a Input<'a>) -> Self {
        let base: HashMap<Uuid, &Entry> = input.base.iter().map(|e| (e.node, e)).collect();
        let pending: HashMap<Uuid, Option<&RemoteState>> =
            input.remote.iter().map(|(n, s)| (*n, s.as_ref())).collect();
        let mut remote: HashMap<Uuid, Node> = input
            .base
            .iter()
            .map(|e| {
                (
                    e.node,
                    Node {
                        parent: e.parent,
                        name: e.name.clone(),
                        is_file: e.is_file,
                        version: e.version,
                    },
                )
            })
            .collect();
        for (node, state) in &pending {
            match state {
                Some(s) => {
                    remote.insert(
                        *node,
                        Node {
                            parent: s.parent,
                            name: s.name.clone(),
                            is_file: s.is_file,
                            version: s.version,
                        },
                    );
                }
                None => {
                    remote.remove(node);
                }
            }
        }
        // A node whose parent chain doesn't reach the root is gone with it.
        let reachable: HashSet<Uuid> = remote
            .keys()
            .filter(|n| reaches_root(**n, input.root, &remote))
            .copied()
            .collect();
        remote.retain(|n, _| reachable.contains(n));
        let scan = input.scan;
        let unseen = scan
            .unreadable
            .iter()
            .map(|(path, _)| path)
            .chain(&scan.links)
            .chain(&scan.elsewhere)
            .map(|path| path.split('/').map(key).collect())
            .collect();
        Planner {
            input,
            base,
            remote,
            pending,
            claimed: HashMap::new(),
            local: HashMap::new(),
            unseen,
            plan: Plan::default(),
        }
    }

    fn items(&self) -> &'a [Item] {
        &self.input.scan.items
    }

    fn run(mut self) -> Plan {
        self.match_local();
        self.settle();
        self.base_nodes();
        self.remote_new();
        self.local_new();
        self.order();
        self.plan
    }

    /// Which local items are which base nodes: by name within their parent
    /// first (an atomic save changes the file ID, not the path), then by file
    /// ID for what isn't where the base had it (a move or rename).
    fn match_local(&mut self) {
        let items = self.items();
        let index: HashMap<Vec<String>, usize> = items
            .iter()
            .enumerate()
            .map(|(i, item)| (item.path.clone(), i))
            .collect();
        let mut children: HashMap<(Uuid, String), Uuid> = HashMap::new();
        for e in self.input.base {
            children.insert((e.parent, key(&e.name)), e.node);
        }
        // The node a local folder is (the root for top-level items).
        let parent_node = |claimed: &HashMap<usize, Uuid>, item: &Item| -> Option<Uuid> {
            match item.path.len() {
                1 => Some(self.input.root),
                _ => index
                    .get(&item.path[..item.path.len() - 1])
                    .and_then(|i| claimed.get(i).copied()),
            }
        };
        // Base nodes found where the base had them, by path from the root:
        // the synced file first, so that a variant beside it (`A.txt` by
        // `a.txt`, NFD by NFC, on a disk telling them apart) is the clash, never
        // the synced file. Parents come first, so one pass per depth.
        let mut in_place: HashSet<Uuid> = HashSet::new();
        let depths = items.iter().map(|i| i.path.len()).max().unwrap_or(0);
        for depth in 1..=depths {
            // The base's file ID first (the synced file whatever its spelling
            // now), then the spelling, then the key.
            for pass in 0..3 {
                for (i, item) in items.iter().enumerate() {
                    if item.path.len() != depth || self.claimed.contains_key(&i) {
                        continue;
                    }
                    let Some(parent) = parent_node(&self.claimed, item) else {
                        continue;
                    };
                    if let Some(&node) = children.get(&(parent, key(item.name())))
                        && self.base[&node].is_file == item.is_file
                        && !in_place.contains(&node)
                        && match pass {
                            0 => item.file_id.is_some() && item.file_id == self.base[&node].file_id,
                            1 => item.name() == self.base[&node].name,
                            _ => true,
                        }
                    {
                        self.claimed.insert(i, node);
                        in_place.insert(node);
                    }
                }
            }
        }
        // Moves: an unclaimed item whose file ID a base node elsewhere had,
        // a file with its size and mtime too (a rename keeps them; ext4 gives
        // a deleted file's inode to the next new one at once); then its content
        // matched by name within it, again. ponytail: a move and an edit
        // between two scans is a delete and a new file (its history lost, the
        // old one in the trash); a reused inode with the same size and mtime
        // on a coarse clock (FAT) still passes for a move.
        let by_id: HashMap<&str, Uuid> = self
            .input
            .base
            .iter()
            .filter(|e| !in_place.contains(&e.node))
            .filter_map(|e| e.file_id.as_deref().map(|id| (id, e.node)))
            .collect();
        let mut taken: HashSet<Uuid> = in_place.clone();
        for (i, item) in items.iter().enumerate() {
            if self.claimed.contains_key(&i) {
                continue;
            }
            let by_name = parent_node(&self.claimed, item).and_then(|parent| {
                children
                    .get(&(parent, key(item.name())))
                    .copied()
                    .filter(|n| self.base[n].is_file == item.is_file && !taken.contains(n))
            });
            let by_file = item
                .file_id
                .as_deref()
                .and_then(|id| by_id.get(id).copied())
                .filter(|n| {
                    let e = self.base[n];
                    e.is_file == item.is_file
                        && (!e.is_file || (e.size == item.size && e.mtime == item.mtime))
                        && !taken.contains(n)
                });
            if let Some(node) = by_name.or(by_file) {
                self.claimed.insert(i, node);
                taken.insert(node);
            }
        }
        let claimed_at: HashMap<Uuid, usize> = self.claimed.iter().map(|(i, n)| (*n, *i)).collect();
        for e in self.input.base {
            // A name the scan skips is never seen either (an ignored file
            // downloaded before: never trashed for being absent).
            let unseen = local::skipped(&e.name)
                || self.base_keys(e.node).is_some_and(|k| self.is_unseen(&k))
                || self
                    .local_keys(&claimed_at, e.node)
                    .is_some_and(|k| self.is_unseen(&k));
            let local = if unseen {
                Local::Unseen
            } else {
                Local::Deleted
            };
            self.local.insert(e.node, local);
        }
        for (&i, &node) in &self.claimed {
            let item = &items[i];
            let e = self.base[&node];
            let parent = match item.path.len() {
                1 => Parent::Node(self.input.root),
                _ => {
                    let up = &item.path[..item.path.len() - 1];
                    match index.get(up).and_then(|p| self.claimed.get(p)) {
                        Some(&p) => Parent::Node(p),
                        None => Parent::New(up.to_vec()),
                    }
                }
            };
            let name = nfc(item.name());
            let moved =
                (parent != Parent::Node(e.parent) || name != e.name).then_some((parent, name));
            // The fingerprint: size, mtime, and the file ID when known (a file
            // replaced by one of the same size and time).
            let modified = e.is_file
                && (item.size != e.size
                    || item.mtime != e.mtime
                    || (e.file_id.is_some() && item.file_id != e.file_id));
            self.local.insert(
                node,
                Local::Present {
                    item: i,
                    moved,
                    modified,
                },
            );
        }
    }

    /// Remote states that change nothing: the base already has them (an echo
    /// of this client's own change), or gone on both sides.
    fn settle(&mut self) {
        for (&node, state) in &self.pending {
            let same = match (state, self.base.get(&node)) {
                (Some(s), Some(e)) => {
                    s.parent == e.parent && s.name == e.name && s.version == e.version
                }
                (None, None) => true,
                _ => false,
            };
            if same {
                self.plan.settled.push(node);
            }
        }
        self.plan.settled.sort();
    }

    /// Nodes under `folder` in the remote view that are new or whose content
    /// changed remotely: they keep a locally deleted folder alive. (Where
    /// nothing is pending, the remote view is the base: nothing to compare.)
    fn remote_news_below(&self, folder: Uuid) -> bool {
        self.remote.iter().any(|(&n, r)| {
            (!self.base.contains_key(&n) || (r.is_file && self.base[&n].version != r.version))
                && self.is_below(n, folder)
        })
    }

    fn is_below(&self, mut node: Uuid, folder: Uuid) -> bool {
        let mut steps = 0;
        while let Some(r) = self.remote.get(&node) {
            if r.parent == folder {
                return true;
            }
            node = r.parent;
            steps += 1;
            if steps > MAX_DEPTH {
                return false;
            }
        }
        false
    }

    /// Local content to keep under a folder gone remotely: a modified file,
    /// or a new item.
    fn local_news_below(&self, folder: Uuid) -> bool {
        let items = self.items();
        let Some(Local::Present { item, .. }) = self.local.get(&folder) else {
            return false;
        };
        let prefix = &items[*item].path;
        // The folder itself too: renamed locally, it's a modification (it wins
        // over the remote delete).
        items.iter().enumerate().any(|(i, it)| {
            it.path.len() >= prefix.len()
                && it.path.starts_with(prefix)
                && match self.claimed.get(&i) {
                    None => true,
                    // Modified, or moved in from elsewhere (it would have
                    // nowhere to go on the server).
                    Some(n) => matches!(
                        self.local.get(n),
                        Some(Local::Present { modified: true, .. })
                            | Some(Local::Present { moved: Some(_), .. })
                    ),
                }
        })
    }

    fn report(&mut self, path: String, message: impl Into<String>, level: Level) {
        self.plan.reports.push(Report {
            path,
            message: message.into(),
            level,
        });
    }

    fn base_nodes(&mut self) {
        let writable = self.input.writable;
        let mut nodes: Vec<Uuid> = self.input.base.iter().map(|e| e.node).collect();
        nodes.sort();
        for node in nodes {
            let e = self.base[&node];
            if let Some(Some(s)) = self.pending.get(&node)
                && !sound(&s.name)
            {
                let path = self.base_path(node).join("/");
                self.report(
                    path,
                    format!("not synced: the server sent the name {:?}", s.name),
                    Level::Error,
                );
                continue;
            }
            let local = self.local[&node].clone();
            let remote = self.remote.get(&node).cloned();
            match (local, remote) {
                // Out of the scan's sight: what changed remotely waits.
                (Local::Unseen, _) => {}
                // Gone on both sides.
                (Local::Deleted, None) => {
                    self.plan.actions.push(Action::Forget { node });
                }
                // Deleted locally.
                (Local::Deleted, Some(r)) => {
                    let content = r.is_file && r.version != e.version;
                    if content || (!r.is_file && self.remote_news_below(node)) {
                        // The modification wins: back locally.
                        self.restore_locally(node, &r);
                    } else if !writable {
                        // Neither uploaded nor reverted: listed, once for a
                        // folder deleted with its content. Out of the base, so
                        // that a role made writable later trashes nothing; it
                        // comes back if it changes on the server.
                        let parent_deleted =
                            matches!(self.local.get(&e.parent), Some(Local::Deleted));
                        if !parent_deleted {
                            let path = self.remote_path(node).unwrap_or_default().join("/");
                            self.report(path, "not uploaded: read-only library", Level::Error);
                        }
                        self.plan.actions.push(Action::Forget { node });
                    } else {
                        self.plan.actions.push(Action::RemoteTrash { node });
                    }
                }
                // Gone remotely.
                (
                    Local::Present {
                        item,
                        moved,
                        modified,
                    },
                    None,
                ) => {
                    let keep =
                        (e.is_file && modified) || (!e.is_file && self.local_news_below(node));
                    if keep && writable {
                        // The modification wins: uploaded again, as new.
                        let path = self.items()[item].path.clone();
                        let (parent, name) =
                            moved.unwrap_or_else(|| (Parent::Node(e.parent), e.name.clone()));
                        let parent = self.alive(parent);
                        if e.is_file {
                            self.plan.actions.push(Action::Upload {
                                path,
                                to: UploadTo::New { parent, name },
                            });
                        } else {
                            self.plan
                                .actions
                                .push(Action::RemoteMkdir { path, parent, name });
                        }
                        self.plan.actions.push(Action::Forget { node });
                    } else if keep {
                        self.report(
                            self.items()[item].display(),
                            "not uploaded: read-only library",
                            Level::Error,
                        );
                    } else {
                        self.plan.actions.push(Action::LocalDelete { node });
                    }
                }
                (
                    Local::Present {
                        item,
                        moved,
                        modified,
                    },
                    Some(r),
                ) => {
                    let place = r.parent != e.parent || r.name != e.name;
                    // The remote view is the base where nothing changed.
                    let content = r.is_file && r.version != e.version;
                    let path = self.items()[item].path.clone();
                    // The place: a local move goes up (it wins over a remote
                    // one), a remote one comes down.
                    match (&moved, writable) {
                        (Some((parent, name)), true) => match Name::new(name) {
                            Ok(_) => {
                                let parent = self.alive(parent.clone());
                                self.plan.actions.push(Action::RemoteMove {
                                    node,
                                    parent,
                                    name: name.clone(),
                                });
                            }
                            // Renamed to a name the server refuses: it stays
                            // where it was there, and it's said why.
                            Err(e) => self.report(
                                self.items()[item].display(),
                                format!("not synced: {e}"),
                                Level::Error,
                            ),
                        },
                        (Some(_), false) => {
                            self.report(
                                self.items()[item].display(),
                                "not moved on the server: read-only library",
                                Level::Error,
                            );
                        }
                        // Never into what the scan can't see (through a link).
                        (None, _)
                            if place
                                && self.remote_path(node).is_some_and(|p| {
                                    self.is_unseen(&p.iter().map(|n| key(n)).collect::<Vec<_>>())
                                }) => {}
                        (None, _) if place => {
                            self.plan.actions.push(Action::LocalMove {
                                node,
                                parent: r.parent,
                                name: r.name.clone(),
                            });
                        }
                        (None, _) => {}
                    }
                    // The content.
                    let state = self.state(node, &r);
                    match (modified, content, writable) {
                        (true, _, true) => self.plan.actions.push(Action::Upload {
                            path,
                            // Based on what was synced: the server forks the
                            // tree if the remote changed meanwhile.
                            to: UploadTo::Version {
                                node,
                                base: e.version,
                            },
                        }),
                        (true, true, false) => {
                            self.plan.actions.push(Action::KeepAside { node });
                            self.plan.actions.push(Action::Download {
                                node,
                                parent: r.parent,
                                name: r.name.clone(),
                                state,
                            });
                        }
                        (true, false, false) => self.report(
                            self.items()[item].display(),
                            "not uploaded: read-only library",
                            Level::Error,
                        ),
                        (false, true, _) => self.plan.actions.push(Action::Download {
                            node,
                            parent: r.parent,
                            name: r.name.clone(),
                            state,
                        }),
                        // A remote place change is in LocalMove; a remote
                        // touch (only its time) was settled.
                        (false, false, _) => {}
                    }
                }
            }
        }
    }

    /// A node deleted locally and changed remotely comes back: a file is
    /// downloaded, a folder recreated (its children follow by their own rules).
    fn restore_locally(&mut self, node: Uuid, r: &Node) {
        let state = self.state(node, r);
        if r.is_file {
            self.plan.actions.push(Action::Download {
                node,
                parent: r.parent,
                name: r.name.clone(),
                state,
            });
        } else {
            self.plan.actions.push(Action::LocalMkdir {
                node,
                parent: r.parent,
                name: r.name.clone(),
            });
        }
    }

    /// The remote state of a node in the remote view (its pending state when
    /// there is one: size and time).
    fn state(&self, node: Uuid, r: &Node) -> RemoteState {
        match self.pending.get(&node) {
            Some(Some(s)) => (*s).clone(),
            _ => {
                let e = self.base.get(&node);
                RemoteState {
                    parent: r.parent,
                    name: r.name.clone(),
                    is_file: r.is_file,
                    version: r.version,
                    size: e.map_or(0, |e| e.size),
                    modified: None,
                }
            }
        }
    }

    /// A parent still on the server, or the local folder to recreate there.
    fn alive(&self, parent: Parent) -> Parent {
        match parent {
            Parent::Node(p) if p != self.input.root && !self.remote.contains_key(&p) => {
                match self.local.get(&p) {
                    Some(Local::Present { item, .. }) => {
                        Parent::New(self.items()[*item].path.clone())
                    }
                    _ => Parent::Node(p),
                }
            }
            other => other,
        }
    }

    /// Nodes on the server the base doesn't know: created locally, or met by
    /// a local item at the same place (a first sync).
    fn remote_new(&mut self) {
        let items = self.items();
        let mut news: Vec<(Uuid, Node)> = self
            .remote
            .iter()
            .filter(|(n, _)| !self.base.contains_key(n))
            .map(|(n, r)| (*n, r.clone()))
            .collect();
        // Parents first.
        news.sort_by_key(|(n, _)| self.depth(*n));
        // Where a remote node lands locally, by key path.
        let mut local_paths: HashMap<Vec<String>, usize> = HashMap::new();
        for (i, item) in items.iter().enumerate() {
            if !self.claimed.contains_key(&i) {
                local_paths.insert(item.path.iter().map(|n| key(n)).collect(), i);
            }
        }
        for (node, r) in news {
            let Some(path) = self.remote_path(node) else {
                continue;
            };
            if let Some(bad) = path.iter().find(|n| !sound(n)) {
                if !sound(&r.name) {
                    self.report(
                        format!("{bad:?}"),
                        "not synced: the server sent this name",
                        Level::Error,
                    );
                }
                continue;
            }
            let keys: Vec<String> = path.iter().map(|n| key(n)).collect();
            // Never through a link, nor into what the scan couldn't read; and
            // never a name the scan would skip (it would look deleted next).
            if self.is_unseen(&keys) || local::skipped(&r.name) {
                continue;
            }
            let state = self.state(node, &r);
            match local_paths.get(&keys).copied() {
                // Read-only: nothing can go up. What this client downloaded
                // (the version's size and time, set by the download) is
                // adopted; anything else stays, listed, never replaced.
                // ponytail: different bytes with the same size and time to the
                // nanosecond pass for the same.
                Some(i) if r.is_file && items[i].is_file && !self.input.writable => {
                    self.claimed.insert(i, node);
                    let item = &items[i];
                    let same = item.size == state.size
                        && state.modified.and_then(|m| m.timestamp_nanos_opt()) == Some(item.mtime);
                    if same {
                        let path = item.path.clone();
                        self.plan.actions.push(Action::Adopt { node, path, state });
                    } else {
                        self.report(
                            item.display(),
                            "not synced: another file has this name (read-only library)",
                            Level::Error,
                        );
                    }
                }
                Some(i) if items[i].is_file == r.is_file => {
                    self.claimed.insert(i, node);
                    let path = items[i].path.clone();
                    self.plan.actions.push(if r.is_file {
                        Action::Merge { node, path, state }
                    } else {
                        Action::Adopt { node, path, state }
                    });
                }
                Some(i) => {
                    // A file where the other side has a folder: the local one
                    // can't go up; the remote one waits.
                    self.claimed.insert(i, node);
                    self.report(
                        items[i].display(),
                        "a folder and a file have this name: rename one",
                        Level::Error,
                    );
                }
                None if r.is_file => self.plan.actions.push(Action::Download {
                    node,
                    parent: r.parent,
                    name: r.name.clone(),
                    state,
                }),
                None => self.plan.actions.push(Action::LocalMkdir {
                    node,
                    parent: r.parent,
                    name: r.name.clone(),
                }),
            }
        }
    }

    fn depth(&self, mut node: Uuid) -> usize {
        let mut depth = 0;
        while let Some(r) = self.remote.get(&node) {
            node = r.parent;
            depth += 1;
            if depth > MAX_DEPTH {
                break;
            }
        }
        depth
    }

    /// A base node's names from the root.
    fn base_path(&self, mut node: Uuid) -> Vec<String> {
        let mut names = Vec::new();
        while let Some(e) = self.base.get(&node) {
            names.push(e.name.clone());
            node = e.parent;
            if names.len() > MAX_DEPTH {
                break;
            }
        }
        names.reverse();
        names
    }

    /// A base node's path from the root, by key.
    fn base_keys(&self, mut node: Uuid) -> Option<Vec<String>> {
        let mut keys = Vec::new();
        while node != self.input.root {
            let e = self.base.get(&node)?;
            keys.push(key(&e.name));
            node = e.parent;
            if keys.len() > MAX_DEPTH {
                return None;
            }
        }
        keys.reverse();
        Some(keys)
    }

    /// Where a base node is locally now, by key: below the nearest ancestor
    /// claimed by a local item (moved or renamed there), else as in the base.
    fn local_keys(&self, claimed_at: &HashMap<Uuid, usize>, mut node: Uuid) -> Option<Vec<String>> {
        let mut below = Vec::new();
        while node != self.input.root {
            if let Some(&i) = claimed_at.get(&node) {
                let mut keys: Vec<String> = self.items()[i].path.iter().map(|n| key(n)).collect();
                keys.extend(below.into_iter().rev());
                return Some(keys);
            }
            let e = self.base.get(&node)?;
            below.push(key(&e.name));
            node = e.parent;
            if below.len() > MAX_DEPTH {
                return None;
            }
        }
        below.reverse();
        Some(below)
    }

    /// At or below a path the scan couldn't look into.
    fn is_unseen(&self, keys: &[String]) -> bool {
        self.unseen.iter().any(|u| keys.starts_with(u))
    }

    /// A node's names from the root, in the remote view.
    fn remote_path(&self, mut node: Uuid) -> Option<Vec<String>> {
        let mut names = Vec::new();
        while node != self.input.root {
            let r = self.remote.get(&node)?;
            names.push(r.name.clone());
            node = r.parent;
            if names.len() > MAX_DEPTH {
                return None;
            }
        }
        names.reverse();
        Some(names)
    }

    /// Local items no node is: new folders and files, created on the server
    /// (names checked first).
    fn local_new(&mut self) {
        let items = self.items();
        let writable = self.input.writable;
        // Rejected items: their content waits with them.
        let mut rejected: Vec<Vec<String>> = Vec::new();
        // Names taken in each parent: the remote view's, then the new ones'.
        let mut taken: HashSet<(Parent, String)> = self
            .remote
            .values()
            .map(|r| (Parent::Node(r.parent), key(&r.name)))
            .collect();
        for (i, item) in items.iter().enumerate() {
            if self.claimed.contains_key(&i) {
                continue;
            }
            if rejected.iter().any(|r| item.path.starts_with(r)) {
                continue;
            }
            let display = item.display();
            let name = match Name::new(item.name()) {
                Ok(name) => name.as_str().to_owned(),
                Err(e) => {
                    self.report(display, format!("not synced: {e}"), Level::Error);
                    rejected.push(item.path.clone());
                    continue;
                }
            };
            if !writable {
                self.report(display, "not uploaded: read-only library", Level::Error);
                rejected.push(item.path.clone());
                continue;
            }
            let parent = match item.path.len() {
                1 => Parent::Node(self.input.root),
                _ => {
                    let up = item.path[..item.path.len() - 1].to_vec();
                    let claimed = items
                        .iter()
                        .position(|it| it.path == up)
                        .and_then(|p| self.claimed.get(&p).copied());
                    self.alive(claimed.map_or(Parent::New(up), Parent::Node))
                }
            };
            if !taken.insert((parent.clone(), key(&name))) {
                self.report(
                    display,
                    "not synced: another file has this name (names ignore case and accents' encoding)",
                    Level::Error,
                );
                rejected.push(item.path.clone());
                continue;
            }
            self.plan.actions.push(if item.is_file {
                Action::Upload {
                    path: item.path.clone(),
                    to: UploadTo::New { parent, name },
                }
            } else {
                Action::RemoteMkdir {
                    path: item.path.clone(),
                    parent,
                    name,
                }
            });
        }
    }

    /// The order the executor runs them in: folders found on both sides; the
    /// server's folders and moves
    /// (parents first), uploads; then locally moves, folders, downloads; then
    /// trashing on the server (after moves: what moved out survives); local
    /// deletes, deepest first; state only.
    fn order(&mut self) {
        let rank = |a: &Action| match a {
            Action::Adopt { path, .. } => (0, path.len()),
            Action::RemoteMkdir { path, .. } => (1, path.len()),
            Action::RemoteMove { .. } => (2, 0),
            Action::Upload { .. } | Action::Merge { .. } => (3, 0),
            // One rank, by depth: a folder created before what moves into it.
            Action::LocalMove { .. } | Action::LocalMkdir { .. } => (4, 0),
            Action::KeepAside { .. } => (6, 0),
            Action::Download { .. } => (7, 0),
            Action::RemoteTrash { .. } => (8, 0),
            Action::LocalDelete { .. } => (9, 0),
            Action::Forget { .. } => (10, 0),
        };
        // Stable: within a rank, the order they were planned in (parents
        // first where it matters); LocalMkdir and LocalMove sorted by depth.
        let depth = |this: &Self, a: &Action| match a {
            Action::LocalMkdir { node, .. } | Action::LocalMove { node, .. } => this.depth(*node),
            // Deepest first (`/` would order them the same: any decreasing
            // function of the depth does).
            Action::LocalDelete { node } => usize::MAX - this.base_depth(*node),
            _ => 0,
        };
        let mut actions = std::mem::take(&mut self.plan.actions);
        let this = &*self;
        actions.sort_by_cached_key(|a| (rank(a), depth(this, a)));
        self.plan.actions = actions;
    }

    fn base_depth(&self, mut node: Uuid) -> usize {
        let mut depth = 0;
        while let Some(e) = self.base.get(&node) {
            node = e.parent;
            depth += 1;
            if depth > MAX_DEPTH {
                break;
            }
        }
        depth
    }
}

fn reaches_root(mut node: Uuid, root: Uuid, remote: &HashMap<Uuid, Node>) -> bool {
    for _ in 0..MAX_DEPTH {
        match remote.get(&node) {
            Some(r) if r.parent == root => return true,
            Some(r) => node = r.parent,
            None => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: Uuid = Uuid::from_u128(1);

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(100 + n)
    }

    fn v(n: u128) -> Option<Uuid> {
        Some(Uuid::from_u128(1000 + n))
    }

    /// A base entry: a file (with a version) or a folder (`version: None`).
    fn entry(node: u128, parent: Uuid, name: &str, version: Option<Uuid>) -> Entry {
        Entry {
            node: id(node),
            parent,
            name: name.into(),
            is_file: version.is_some(),
            version,
            size: 10,
            mtime: 5,
            file_id: Some(format!("f{node}")),
        }
    }

    /// A local item matching an entry's fingerprint.
    fn item(path: &str, e: Option<&Entry>, is_file: bool) -> Item {
        Item {
            path: path.split('/').map(String::from).collect(),
            is_file,
            size: e.map_or(3, |e| e.size),
            mtime: e.map_or(7, |e| e.mtime),
            file_id: e.and_then(|e| e.file_id.clone()),
        }
    }

    fn remote(parent: Uuid, name: &str, version: Option<Uuid>) -> Option<RemoteState> {
        Some(RemoteState {
            parent,
            name: name.into(),
            is_file: version.is_some(),
            version,
            size: 20,
            modified: None,
        })
    }

    fn run(base: &[Entry], items: Vec<Item>, pending: &[(Uuid, Option<RemoteState>)]) -> Plan {
        run_on(base, items, pending, true)
    }

    fn run_on(
        base: &[Entry],
        items: Vec<Item>,
        pending: &[(Uuid, Option<RemoteState>)],
        writable: bool,
    ) -> Plan {
        let scan = Scan {
            items,
            ..Default::default()
        };
        plan(&Input {
            root: ROOT,
            base,
            scan: &scan,
            remote: pending,
            writable,
        })
    }

    /// A folder `d` (1) with `a.txt` (2), in sync.
    fn synced() -> (Vec<Entry>, Vec<Item>) {
        let d = entry(1, ROOT, "d", None);
        let a = entry(2, id(1), "a.txt", v(1));
        let items = vec![item("d", Some(&d), false), item("d/a.txt", Some(&a), true)];
        (vec![d, a], items)
    }

    fn plan_scan(base: &[Entry], scan: Scan, pending: &[(Uuid, Option<RemoteState>)]) -> Plan {
        plan(&Input {
            root: ROOT,
            base,
            scan: &scan,
            remote: pending,
            writable: true,
        })
    }

    /// B1: a folder the scan couldn't read, or a file it couldn't stat, is
    /// not deleted: nothing goes to the trash, nothing is forgotten.
    #[test]
    fn what_the_scan_could_not_read_is_not_deleted() {
        let (base, items) = synced();
        for unreadable in ["d", "d/a.txt"] {
            let scan = Scan {
                items: items[..1].to_vec(),
                unreadable: vec![(unreadable.into(), "Permission denied".into())],
                ..Default::default()
            };
            let plan = plan_scan(&base, scan, &[]);
            assert_eq!(plan.actions, [], "{unreadable}");
        }
    }

    /// B16: a synced folder replaced by a symbolic link is not deleted, and
    /// nothing new is written through the link.
    #[test]
    fn a_folder_replaced_by_a_link_is_not_deleted() {
        let (base, _) = synced();
        let scan = Scan {
            links: vec!["d".into()],
            ..Default::default()
        };
        let new = remote(id(1), "b.txt", v(9));
        let plan = plan_scan(&base, scan, &[(id(3), new)]);
        assert_eq!(plan.actions, []);
    }

    /// B11: names the scan skips (what operating systems leave, a part file)
    /// are never downloaded from the server, nor trashed for being absent.
    #[test]
    fn ignored_names_on_the_server_are_left_alone() {
        let base = [entry(1, ROOT, ".DS_Store", v(1))];
        let pending = [
            (id(2), remote(ROOT, "desktop.ini", v(2))),
            (id(3), remote(ROOT, ".x.txt.0123abcd.theca-part", v(3))),
        ];
        let plan = run(&base, vec![], &pending);
        assert_eq!(plan.actions, []);
    }

    /// B12: beside a synced `a.txt`, a new `A.txt` (or an NFD spelling) on a
    /// case-sensitive disk is the clash, never the synced file.
    #[test]
    fn a_case_variant_never_takes_over_the_synced_file() {
        for (synced, variant) in [("a.txt", "A.txt"), ("caf\u{e9}.txt", "cafe\u{301}.txt")] {
            let e = entry(2, ROOT, synced, v(1));
            let items = vec![item(variant, None, true), item(synced, Some(&e), true)];
            let plan = run(std::slice::from_ref(&e), items, &[]);
            assert_eq!(plan.actions, [], "{variant}");
            assert!(
                plan.reports.iter().any(|r| r.path == variant),
                "{:?}",
                plan.reports
            );
        }
    }

    /// B17: a file deleted locally in a read-only library leaves the base
    /// (still listed): if the role becomes writable, it isn't trashed.
    #[test]
    fn a_read_only_delete_leaves_the_base() {
        let (base, items) = synced();
        let plan = run_on(&base, items[..1].to_vec(), &[], false);
        assert_eq!(plan.actions, [Action::Forget { node: id(2) }]);
        assert_eq!(plan.reports.len(), 1);
    }

    /// C5: a name the server should never have sent (`..`, a path, an
    /// absolute one) never reaches the disk, nor reads as a deletion.
    #[test]
    fn names_that_leave_the_folder_are_refused() {
        let (base, items) = synced();
        for bad in [
            "..",
            "../escape.txt",
            "/tmp/abs.txt",
            "a/b.txt",
            "C:\\x",
            "con.txt",
        ] {
            let pending = [
                (id(9), remote(ROOT, bad, v(9))),
                (id(2), remote(id(1), bad, v(1))),
            ];
            let plan = run(&base, items.clone(), &pending);
            assert_eq!(plan.actions, [], "{bad}");
            let paths: Vec<&str> = plan.reports.iter().map(|r| r.path.as_str()).collect();
            assert_eq!(paths.len(), 2, "{bad}: {paths:?}");
            assert!(paths.contains(&"d/a.txt"), "{paths:?}");
        }
    }

    /// C8: a file moved locally into a folder the server deleted keeps the
    /// folder: both go up, the folder new.
    #[test]
    fn a_file_moved_into_a_folder_deleted_remotely_keeps_it() {
        let e = entry(1, ROOT, "e", None);
        let x = entry(2, ROOT, "x", v(1));
        let items = vec![item("e", Some(&e), false), item("e/x", Some(&x), true)];
        let plan = run(&[e, x], items, &[(id(1), None)]);
        assert!(
            !plan.actions.contains(&Action::LocalDelete { node: id(1) }),
            "{:?}",
            plan.actions
        );
        assert!(
            plan.actions
                .iter()
                .any(|a| matches!(a, Action::RemoteMkdir { name, .. } if name == "e"))
        );
        assert!(plan.actions.contains(&Action::RemoteMove {
            node: id(2),
            parent: Parent::New(vec!["e".into()]),
            name: "x".into()
        }));
    }

    /// C11: a read-only library's first sync (or a lost state) never plans
    /// an upload the server refuses: a local file with the server version's
    /// size and time is adopted, another one is listed and kept.
    #[test]
    fn a_read_only_first_sync_uploads_nothing() {
        let at = chrono::DateTime::from_timestamp_nanos(7);
        let state = |size| RemoteState {
            parent: ROOT,
            name: "a.txt".into(),
            is_file: true,
            version: v(1),
            size,
            modified: Some(at),
        };
        let same = run_on(
            &[],
            vec![item("a.txt", None, true)],
            &[(id(1), Some(state(3)))],
            false,
        );
        assert!(
            matches!(same.actions[..], [Action::Adopt { .. }]),
            "{:?}",
            same.actions
        );
        let other = run_on(
            &[],
            vec![item("a.txt", None, true)],
            &[(id(1), Some(state(4)))],
            false,
        );
        assert_eq!(other.actions, []);
        assert_eq!(other.reports.len(), 1);
    }

    /// An atomic save (a new file, renamed over the old one) at the root is
    /// an edit of it, not a delete and a new file.
    #[test]
    fn an_atomic_save_at_the_root_is_an_edit() {
        let a = entry(2, ROOT, "a.txt", v(1));
        let mut saved = item("a.txt", Some(&a), true);
        saved.file_id = Some("new".into());
        saved.size = 77;
        let plan = run(std::slice::from_ref(&a), vec![saved], &[]);
        assert!(
            matches!(
                plan.actions[..],
                [Action::Upload {
                    to: UploadTo::Version { .. },
                    ..
                }]
            ),
            "{:?}",
            plan.actions
        );
    }

    /// A name the disk spells otherwise (NFD on an older Mac disk) is the
    /// synced file.
    #[test]
    fn another_spelling_on_disk_is_the_synced_file() {
        let e = entry(2, ROOT, "caf\u{e9}.txt", v(1));
        let plan = run(
            std::slice::from_ref(&e),
            vec![item("cafe\u{301}.txt", Some(&e), true)],
            &[],
        );
        assert_eq!(plan.actions, []);
    }

    /// In a folder renamed locally, a file saved atomically is still an edit.
    #[test]
    fn an_atomic_save_in_a_renamed_folder_is_an_edit() {
        let (base, _) = synced();
        let mut saved = item("e/a.txt", Some(&base[1]), true);
        saved.file_id = Some("new".into());
        saved.size = 77;
        let plan = run(&base, vec![item("e", Some(&base[0]), false), saved], &[]);
        assert!(plan.actions.contains(&Action::RemoteMove {
            node: id(1),
            parent: Parent::Node(ROOT),
            name: "e".into()
        }));
        assert!(plan.actions.iter().any(|a| matches!(a, Action::Upload { to: UploadTo::Version { node, .. }, .. } if *node == id(2))), "{:?}", plan.actions);
    }

    /// Gone remotely, never known locally: nothing to do, settled.
    #[test]
    fn a_node_gone_that_nobody_knew_is_settled() {
        let plan = run(&[], vec![], &[(id(9), None)]);
        assert_eq!(plan.settled, [id(9)]);
    }

    /// A folder deleted locally stays deleted (trashed) unless something
    /// inside it changed remotely: a new file or a new version, not a rename,
    /// and not a change elsewhere.
    #[test]
    fn only_news_inside_keep_a_deleted_folder() {
        let (mut base, _) = synced();
        base.push(entry(3, ROOT, "b.txt", v(3)));
        let b = item("b.txt", Some(&base[2]), true);
        let trashed = |pending: &[(Uuid, Option<RemoteState>)]| {
            run(&base, vec![b.clone()], pending)
                .actions
                .contains(&Action::RemoteTrash { node: id(1) })
        };
        assert!(
            trashed(&[(id(3), remote(ROOT, "b.txt", v(4)))]),
            "a change elsewhere"
        );
        assert!(
            trashed(&[(id(2), remote(id(1), "renamed.txt", v(1)))]),
            "a rename inside"
        );
        assert!(
            !trashed(&[(id(2), remote(id(1), "a.txt", v(2)))]),
            "a new version inside"
        );
        assert!(
            !trashed(&[(id(5), remote(id(1), "new.txt", v(5)))]),
            "a new file inside"
        );
    }

    /// A remote rename within its folder comes down.
    #[test]
    fn a_remote_rename_comes_down() {
        let (base, items) = synced();
        let plan = run(&base, items, &[(id(2), remote(id(1), "b.txt", v(1)))]);
        assert_eq!(
            plan.actions,
            [Action::LocalMove {
                node: id(2),
                parent: id(1),
                name: "b.txt".into()
            }]
        );
    }

    /// A local file where the server has a new folder of that name: listed,
    /// neither merged nor adopted.
    #[test]
    fn a_file_and_a_folder_of_one_name_are_listed() {
        let plan = run(
            &[],
            vec![item("x", None, true)],
            &[(id(5), remote(ROOT, "x", None))],
        );
        assert_eq!(plan.actions, []);
        assert!(
            plan.reports
                .iter()
                .any(|r| r.message.contains("a folder and a file"))
        );
    }

    /// Folders come before what moves into them, and deletes go deepest
    /// first.
    #[test]
    fn folders_first_deletes_deepest_first() {
        let (base, items) = synced();
        let pending = [
            (id(5), remote(ROOT, "n", None)),
            (id(2), remote(id(5), "a.txt", v(1))),
        ];
        let plan = run(&base, items.clone(), &pending);
        let mkdir = plan
            .actions
            .iter()
            .position(|a| matches!(a, Action::LocalMkdir { node, .. } if *node == id(5)));
        let moved = plan
            .actions
            .iter()
            .position(|a| matches!(a, Action::LocalMove { node, .. } if *node == id(2)));
        assert!(mkdir.unwrap() < moved.unwrap(), "{:?}", plan.actions);
        let plan = run(&base, items, &[(id(1), None), (id(2), None)]);
        assert_eq!(
            plan.actions,
            [
                Action::LocalDelete { node: id(2) },
                Action::LocalDelete { node: id(1) }
            ]
        );
    }

    /// A folder renamed locally while the server deleted it: the rename is a
    /// change, it wins (the folder goes up again, new).
    #[test]
    fn a_folder_renamed_locally_survives_a_remote_delete() {
        let (base, _) = synced();
        let items = vec![
            item("e", Some(&base[0]), false),
            item("e/a.txt", Some(&base[1]), true),
        ];
        let plan = run(&base, items, &[(id(1), None), (id(2), None)]);
        // Its unchanged content follows the delete (in the server's trash).
        assert!(
            !plan.actions.contains(&Action::LocalDelete { node: id(1) }),
            "{:?}",
            plan.actions
        );
        assert!(
            plan.actions
                .iter()
                .any(|a| matches!(a, Action::RemoteMkdir { name, .. } if name == "e"))
        );
    }

    /// The exact spelling claims first at every depth, under a folder whose
    /// own spelling changed too.
    #[test]
    fn a_case_variant_deeper_down_is_the_clash() {
        let (base, _) = synced();
        let items = vec![
            item("D", Some(&base[0]), false),
            item("D/A.txt", None, true),
            item("D/a.txt", Some(&base[1]), true),
        ];
        let plan = run(&base, items, &[]);
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, Action::Upload { .. })),
            "{:?}",
            plan.actions
        );
        assert!(
            plan.reports.iter().any(|r| r.path == "D/A.txt"),
            "{:?}",
            plan.reports
        );
    }

    /// A remote move into a folder that is now a link is not followed: the
    /// file would leave the pair's folder through it.
    #[test]
    fn a_remote_move_never_goes_through_a_link() {
        let d = entry(1, ROOT, "d", None);
        let x = entry(2, ROOT, "x", v(1));
        let scan = Scan {
            items: vec![item("x", Some(&x), true)],
            links: vec!["d".into()],
            ..Default::default()
        };
        let plan = plan_scan(&[d, x], scan, &[(id(2), remote(id(1), "x", v(1)))]);
        assert_eq!(plan.actions, []);
    }

    /// Unseen where the folder is now: renamed locally, its unreadable
    /// subfolder is still not a deletion.
    #[test]
    fn unseen_below_a_folder_renamed_locally() {
        let d = entry(1, ROOT, "d", None);
        let sub = entry(3, id(1), "sub", None);
        let f = entry(4, id(3), "f", v(4));
        let scan = Scan {
            items: vec![item("e", Some(&d), false), item("e/sub", Some(&sub), false)],
            unreadable: vec![("e/sub".into(), "Permission denied".into())],
            ..Default::default()
        };
        let plan = plan_scan(&[d, sub, f], scan, &[]);
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, Action::RemoteTrash { .. })),
            "{:?}",
            plan.actions
        );
    }

    /// B12: the synced file renamed locally by case (`A.txt` to `a.txt`, its
    /// file ID kept) beside a new `A.txt`: the file ID says which is which.
    #[test]
    fn the_file_id_claims_before_the_spelling() {
        let e = entry(2, ROOT, "A.txt", v(1));
        let items = vec![item("A.txt", None, true), item("a.txt", Some(&e), true)];
        let plan = run(std::slice::from_ref(&e), items, &[]);
        assert_eq!(
            plan.actions,
            [Action::RemoteMove {
                node: id(2),
                parent: Parent::Node(ROOT),
                name: "a.txt".into()
            }]
        );
        assert!(
            plan.reports.iter().any(|r| r.path == "A.txt"),
            "{:?}",
            plan.reports
        );
    }

    #[test]
    fn in_sync_does_nothing() {
        let (base, items) = synced();
        let plan = run(&base, items, &[]);
        assert!(
            plan.actions.is_empty() && plan.reports.is_empty(),
            "{plan:?}"
        );
    }

    #[test]
    fn an_echo_settles() {
        let (base, items) = synced();
        let plan = run(&base, items, &[(id(2), remote(id(1), "a.txt", v(1)))]);
        assert!(plan.actions.is_empty());
        assert_eq!(plan.settled, [id(2)]);
    }

    #[test]
    fn local_changes_go_up() {
        let (base, mut items) = synced();
        // Modified, then a new file and a new folder with a file.
        items[1].size = 99;
        items.push(item("d/new.txt", None, true));
        items.push(item("e", None, false));
        items.push(item("e/f.txt", None, true));
        let plan = run(&base, items, &[]);
        assert_eq!(
            plan.actions,
            [
                Action::RemoteMkdir {
                    path: vec!["e".into()],
                    parent: Parent::Node(ROOT),
                    name: "e".into()
                },
                Action::Upload {
                    path: vec!["d".into(), "a.txt".into()],
                    to: UploadTo::Version {
                        node: id(2),
                        base: v(1)
                    }
                },
                Action::Upload {
                    path: vec!["d".into(), "new.txt".into()],
                    to: UploadTo::New {
                        parent: Parent::Node(id(1)),
                        name: "new.txt".into()
                    }
                },
                Action::Upload {
                    path: vec!["e".into(), "f.txt".into()],
                    to: UploadTo::New {
                        parent: Parent::New(vec!["e".into()]),
                        name: "f.txt".into()
                    }
                },
            ]
        );
    }

    #[test]
    fn remote_changes_come_down() {
        let (base, items) = synced();
        let plan = run(
            &base,
            items,
            &[
                (id(2), remote(id(1), "a.txt", v(2))),
                (id(3), remote(ROOT, "g", None)),
                (id(4), remote(id(3), "h.txt", v(3))),
            ],
        );
        let kinds: Vec<_> = plan
            .actions
            .iter()
            .map(|a| match a {
                Action::LocalMkdir { node, .. } => format!("mkdir {node}"),
                Action::Download { node, .. } => format!("download {node}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                format!("mkdir {}", id(3)),
                format!("download {}", id(2)),
                format!("download {}", id(4))
            ]
        );
    }

    #[test]
    fn both_changed_uploads_on_the_base() {
        let (base, mut items) = synced();
        items[1].mtime = 99;
        let plan = run(&base, items, &[(id(2), remote(id(1), "a.txt", v(2)))]);
        assert_eq!(
            plan.actions,
            [Action::Upload {
                path: vec!["d".into(), "a.txt".into()],
                to: UploadTo::Version {
                    node: id(2),
                    base: v(1)
                }
            }]
        );
    }

    #[test]
    fn moves_by_file_id_and_renames_by_case() {
        let (base, mut items) = synced();
        // d renamed to D2, a.txt (inside) renamed to A.TXT: case-only.
        items[0].path = vec!["D2".into()];
        items[1].path = vec!["D2".into(), "A.TXT".into()];
        let plan = run(&base, items, &[]);
        assert_eq!(
            plan.actions,
            [
                Action::RemoteMove {
                    node: id(1),
                    parent: Parent::Node(ROOT),
                    name: "D2".into()
                },
                Action::RemoteMove {
                    node: id(2),
                    parent: Parent::Node(id(1)),
                    name: "A.TXT".into()
                },
            ]
        );
    }

    #[test]
    fn a_reused_inode_is_no_move() {
        let (base, mut items) = synced();
        // a.txt deleted, and ext4 gives its inode to a new file at once.
        items[1].path = vec!["b.txt".into()];
        items[1].mtime = 9;
        let plan = run(&base, items, &[]);
        assert!(
            matches!(
                plan.actions[..],
                [
                    Action::Upload {
                        to: UploadTo::New { .. },
                        ..
                    },
                    Action::RemoteTrash { node }
                ] if node == id(2)
            ),
            "{plan:?}"
        );
    }

    #[test]
    fn an_atomic_save_is_the_same_file() {
        let (base, mut items) = synced();
        items[1].file_id = Some("another inode".into());
        items[1].size = 11;
        let plan = run(&base, items.clone(), &[]);
        assert!(matches!(
            plan.actions[..],
            [Action::Upload {
                to: UploadTo::Version { .. },
                ..
            }]
        ));
        // Replaced by one of the same size and time: the file ID tells.
        items[1].size = base[1].size;
        let plan = run(&base, items, &[]);
        assert!(matches!(plan.actions[..], [Action::Upload { .. }]));
    }

    #[test]
    fn remote_moves_and_renames_come_down() {
        let (base, items) = synced();
        let plan = run(&base, items, &[(id(2), remote(ROOT, "b.txt", v(1)))]);
        assert_eq!(
            plan.actions,
            [Action::LocalMove {
                node: id(2),
                parent: ROOT,
                name: "b.txt".into()
            }]
        );
    }

    #[test]
    fn local_deletes_go_to_the_trash() {
        let (base, items) = synced();
        // The whole folder deleted: trashed (the server takes its content).
        let plan = run(&base, vec![], &[]);
        assert_eq!(
            plan.actions,
            [
                Action::RemoteTrash { node: id(1) },
                Action::RemoteTrash { node: id(2) }
            ]
        );
        // Only the file.
        let plan = run(&base, items[..1].to_vec(), &[]);
        assert_eq!(plan.actions, [Action::RemoteTrash { node: id(2) }]);
    }

    #[test]
    fn a_remote_modification_wins_over_a_local_delete() {
        let (base, items) = synced();
        let plan = run(
            &base,
            items[..1].to_vec(),
            &[(id(2), remote(id(1), "a.txt", v(2)))],
        );
        assert!(matches!(plan.actions[..], [Action::Download { node, .. }] if node == id(2)));
        // The folder deleted locally while a file came into it: kept.
        let plan = run(&base, vec![], &[(id(5), remote(id(1), "x.txt", v(5)))]);
        assert!(plan.actions.contains(&Action::LocalMkdir {
            node: id(1),
            parent: ROOT,
            name: "d".into()
        }));
        assert!(plan.actions.contains(&Action::RemoteTrash { node: id(2) }));
        assert!(!plan.actions.contains(&Action::RemoteTrash { node: id(1) }));
    }

    #[test]
    fn remote_deletes_come_down_unless_modified_locally() {
        let (base, mut items) = synced();
        let plan = run(&base, items.clone(), &[(id(1), None)]);
        assert_eq!(
            plan.actions,
            [
                Action::LocalDelete { node: id(2) },
                Action::LocalDelete { node: id(1) }
            ],
            "deepest first"
        );
        // The file modified: uploaded again as new, its folder recreated.
        items[1].size = 50;
        let plan = run(&base, items, &[(id(1), None)]);
        assert_eq!(
            plan.actions,
            [
                Action::RemoteMkdir {
                    path: vec!["d".into()],
                    parent: Parent::Node(ROOT),
                    name: "d".into()
                },
                Action::Upload {
                    path: vec!["d".into(), "a.txt".into()],
                    to: UploadTo::New {
                        parent: Parent::New(vec!["d".into()]),
                        name: "a.txt".into()
                    }
                },
                Action::Forget { node: id(1) },
                Action::Forget { node: id(2) },
            ]
        );
    }

    #[test]
    fn a_first_sync_merges_by_path() {
        let items = vec![
            item("d", None, false),
            item("d/same.txt", None, true),
            item("mine.txt", None, true),
        ];
        let plan = run(
            &[],
            items,
            &[
                (id(1), remote(ROOT, "D", None)),
                (id(2), remote(id(1), "same.txt", v(1))),
                (id(3), remote(ROOT, "theirs.txt", v(3))),
            ],
        );
        let st = |p: Uuid, n: &str, ver| remote(p, n, ver).unwrap();
        assert_eq!(
            plan.actions,
            [
                Action::Adopt {
                    node: id(1),
                    path: vec!["d".into()],
                    state: st(ROOT, "D", None)
                },
                Action::Merge {
                    node: id(2),
                    path: vec!["d".into(), "same.txt".into()],
                    state: st(id(1), "same.txt", v(1))
                },
                Action::Upload {
                    path: vec!["mine.txt".into()],
                    to: UploadTo::New {
                        parent: Parent::Node(ROOT),
                        name: "mine.txt".into()
                    }
                },
                Action::Download {
                    node: id(3),
                    parent: ROOT,
                    name: "theirs.txt".into(),
                    state: st(ROOT, "theirs.txt", v(3))
                },
            ]
        );
    }

    #[test]
    fn names_that_cant_go_up_are_reported() {
        let items = vec![
            item("a:b.txt", None, true),
            item("A.txt", None, true),
            item("a.txt", None, true),
            item("bad.", None, false),
            item("bad./inside.txt", None, true),
        ];
        let plan = run(&[], items, &[]);
        assert_eq!(plan.actions.len(), 1, "{:?}", plan.actions);
        let mut paths: Vec<&str> = plan.reports.iter().map(|r| r.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["a.txt", "a:b.txt", "bad."]);
        // A synced file renamed to such a name: reported, left where it was.
        let (base, mut items) = synced();
        items[1] = item("d/a:b.txt", Some(&base[1]), true);
        let plan = run(&base, items, &[]);
        assert!(plan.actions.is_empty(), "{:?}", plan.actions);
        assert_eq!(plan.reports[0].path, "d/a:b.txt");
    }

    #[test]
    fn a_read_only_library_only_comes_down() {
        let (base, mut items) = synced();
        items[1].size = 77;
        items.push(item("new.txt", None, true));
        let plan = run_on(&base, items.clone(), &[], false);
        assert!(plan.actions.is_empty());
        assert_eq!(plan.reports.len(), 2);
        // Changed on both sides: the local copy kept aside.
        let plan = run_on(
            &base,
            items,
            &[(id(2), remote(id(1), "a.txt", v(2)))],
            false,
        );
        assert!(matches!(
            plan.actions[..],
            [Action::KeepAside { .. }, Action::Download { .. }]
        ));
        // Deleted locally: neither trashed nor back, listed, out of the base.
        let (base, items) = synced();
        let plan = run_on(&base, items[..1].to_vec(), &[], false);
        assert_eq!(plan.actions, [Action::Forget { node: id(2) }]);
        assert_eq!(plan.reports[0].path, "d/a.txt");
        // The folder with it: listed once.
        let plan = run_on(&base, vec![], &[], false);
        assert_eq!(plan.actions.len(), 2, "{:?}", plan.actions);
        assert_eq!(plan.reports.len(), 1);
        // Changed on the server meanwhile: it comes down.
        let plan = run_on(
            &base,
            items[..1].to_vec(),
            &[(id(2), remote(id(1), "a.txt", v(2)))],
            false,
        );
        assert!(matches!(plan.actions[..], [Action::Download { .. }]));
    }

    #[test]
    fn gone_on_both_sides_is_forgotten() {
        let (base, items) = synced();
        let plan = run(&base, items[..1].to_vec(), &[(id(2), None)]);
        assert_eq!(plan.actions, [Action::Forget { node: id(2) }]);
    }
}
