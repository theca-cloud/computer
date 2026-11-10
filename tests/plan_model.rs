//! `plan.rs` against a model of both sides, over random histories of changes
//! (proptest): no local content is lost unless the server ever held it (a
//! version, the trash). The model's executor follows `exec.rs`'s rules for
//! what each action records: a failure here is checked against `exec.rs`
//! before it counts. Its reach is the model's: two names at the root and in
//! one folder; case, normalization, timing, `reset` and disk errors are the
//! unit tests' and the scenarios'. `PROPTEST_CASES` for more cases.

use std::collections::{BTreeMap, HashSet};

use proptest::prelude::*;
use theca::local::{Item, Scan};
use theca::plan::{Action, Input, Parent, UploadTo, plan};
use theca::state::{Entry, RemoteState};
use uuid::Uuid;

const ROOT: Uuid = Uuid::from_u128(1);
const PATHS: [&str; 5] = ["a", "b", "d/a", "d/b", "d"];

#[derive(Clone, Debug)]
struct SNode {
    parent: Uuid,
    name: String,
    is_file: bool,
    version: Option<Uuid>,
    trashed: bool,
}

#[derive(Clone, Debug)]
struct LFile {
    is_file: bool,
    content: u32,
    fid: u32,
}

#[derive(Default, Debug)]
struct World {
    server: BTreeMap<Uuid, SNode>,
    versions: BTreeMap<Uuid, u32>,
    ever: HashSet<u32>,
    local: BTreeMap<Vec<String>, LFile>,
    base: Vec<Entry>,
    next: u32,
}

fn p(s: &str) -> Vec<String> {
    s.split('/').map(String::from).collect()
}

impl World {
    fn fresh(&mut self) -> u32 {
        self.next += 1;
        self.next
    }
    fn uuid(&mut self) -> Uuid {
        let n = self.fresh();
        Uuid::from_u128(1_000_000 + n as u128)
    }
    fn new_version(&mut self, content: u32) -> Uuid {
        let v = self.uuid();
        self.versions.insert(v, content);
        self.ever.insert(content);
        v
    }
    fn server_path(&self, mut n: Uuid) -> Option<Vec<String>> {
        let mut out = vec![];
        while n != ROOT {
            let s = self.server.get(&n)?;
            if s.trashed {
                return None;
            }
            out.push(s.name.clone());
            n = s.parent;
        }
        out.reverse();
        Some(out)
    }
    fn server_child(&self, parent: Uuid, name: &str) -> Option<Uuid> {
        self.server
            .iter()
            .find(|(_, s)| !s.trashed && s.parent == parent && s.name == name)
            .map(|(k, _)| *k)
    }
    fn server_at(&self, path: &[String]) -> Option<Uuid> {
        let mut n = ROOT;
        for name in path {
            n = self.server_child(n, name)?;
        }
        Some(n)
    }
    fn base_entry(&self, n: Uuid) -> Option<&Entry> {
        self.base.iter().find(|e| e.node == n)
    }
    fn base_path(&self, mut n: Uuid) -> Option<Vec<String>> {
        let mut out = vec![];
        while n != ROOT {
            let e = self.base_entry(n)?;
            out.push(e.name.clone());
            n = e.parent;
        }
        out.reverse();
        Some(out)
    }
    fn put(
        &mut self,
        node: Uuid,
        parent: Uuid,
        name: &str,
        is_file: bool,
        version: Option<Uuid>,
        at: &[String],
    ) {
        let (size, mtime, file_id) = match self.local.get(at) {
            Some(l) => (
                l.content as i64,
                l.content as i64,
                Some(format!("f{}", l.fid)),
            ),
            None => (0, 0, None),
        };
        self.base.retain(|e| e.node != node);
        self.base.push(Entry {
            node,
            parent,
            name: name.into(),
            is_file,
            version,
            size,
            mtime,
            file_id,
        });
    }
    fn forget_tree(&mut self, n: Uuid) {
        let kids: Vec<Uuid> = self
            .base
            .iter()
            .filter(|e| e.parent == n)
            .map(|e| e.node)
            .collect();
        for k in kids {
            self.forget_tree(k);
        }
        self.base.retain(|e| e.node != n);
    }
    fn trash_tree(&mut self, n: Uuid) {
        if let Some(s) = self.server.get_mut(&n) {
            s.trashed = true;
        }
    }

    fn synced(files: &[&str]) -> World {
        let mut w = World::default();
        for f in files {
            let path = p(f);
            if path.len() == 2 && w.server_at(&path[..1]).is_none() {
                let d = w.uuid();
                w.server.insert(
                    d,
                    SNode {
                        parent: ROOT,
                        name: "d".into(),
                        is_file: false,
                        version: None,
                        trashed: false,
                    },
                );
                w.local.insert(
                    p("d"),
                    LFile {
                        is_file: false,
                        content: 0,
                        fid: 0,
                    },
                );
                w.put(d, ROOT, "d", false, None, &p("d"));
            }
            let parent = if path.len() == 2 {
                w.server_at(&path[..1]).unwrap()
            } else {
                ROOT
            };
            let c = w.fresh();
            let v = w.new_version(c);
            let n = w.uuid();
            w.server.insert(
                n,
                SNode {
                    parent,
                    name: path.last().unwrap().clone(),
                    is_file: true,
                    version: Some(v),
                    trashed: false,
                },
            );
            let fid = w.fresh();
            w.local.insert(
                path.clone(),
                LFile {
                    is_file: true,
                    content: c,
                    fid,
                },
            );
            w.put(n, parent, path.last().unwrap(), true, Some(v), &path);
        }
        w
    }

    fn scan(&self) -> Scan {
        let items = self
            .local
            .iter()
            .map(|(path, l)| Item {
                path: path.clone(),
                is_file: l.is_file,
                size: if l.is_file { l.content as i64 } else { 0 },
                mtime: if l.is_file { l.content as i64 } else { 0 },
                file_id: Some(format!("f{}", l.fid)),
                ignored: false,
            })
            .collect();
        Scan {
            items,
            ..Default::default()
        }
    }

    fn pending(&self) -> Vec<(Uuid, Option<RemoteState>)> {
        let mut out = vec![];
        for (n, s) in &self.server {
            let state = (!s.trashed && self.server_path(*n).is_some()).then(|| RemoteState {
                parent: s.parent,
                name: s.name.clone(),
                is_file: s.is_file,
                version: s.version,
                size: s.version.map_or(0, |v| self.versions[&v] as i64),
                modified: None,
            });
            let same = match (self.base_entry(*n), &state) {
                (Some(e), Some(r)) => {
                    e.parent == r.parent && e.name == r.name && e.version == r.version
                }
                (None, None) => true,
                _ => false,
            };
            if !same {
                out.push((*n, state));
            }
        }
        out
    }

    fn local_parent(&self, parent: Uuid) -> Option<Vec<String>> {
        if parent == ROOT {
            Some(vec![])
        } else {
            self.base_path(parent)
        }
    }

    fn parent_node(&self, parent: &Parent, made: &BTreeMap<Vec<String>, Uuid>) -> Option<Uuid> {
        match parent {
            Parent::Node(n) => Some(*n),
            Parent::New(path) => made.get(path).copied(),
        }
    }

    fn fingerprint_matches(&self, path: &[String], e: &Entry) -> bool {
        match self.local.get(path) {
            Some(l) => l.content as i64 == e.size && l.content as i64 == e.mtime,
            None => true,
        }
    }

    fn apply(&mut self, a: &Action, made: &mut BTreeMap<Vec<String>, Uuid>) {
        match a {
            Action::RemoteMkdir { path, parent, name } => {
                let Some(par) = self.parent_node(parent, made) else {
                    return;
                };
                if self.server_child(par, name).is_some() {
                    return;
                }
                let n = self.uuid();
                self.server.insert(
                    n,
                    SNode {
                        parent: par,
                        name: name.clone(),
                        is_file: false,
                        version: None,
                        trashed: false,
                    },
                );
                made.insert(path.clone(), n);
                self.put(n, par, name, false, None, path);
            }
            Action::RemoteMove { node, parent, name } => {
                let Some(par) = self.parent_node(parent, made) else {
                    return;
                };
                if self.server_child(par, name).is_some_and(|c| c != *node) {
                    return;
                }
                let s = self.server.get_mut(node).unwrap();
                s.parent = par;
                s.name = name.clone();
                let at = self.local_parent(par).map(|mut l| {
                    l.push(name.clone());
                    l
                });
                let fid = at.and_then(|at| self.local.get(&at).map(|l| format!("f{}", l.fid)));
                if let Some(e) = self.base.iter_mut().find(|e| e.node == *node) {
                    e.parent = par;
                    e.name = name.clone();
                    e.file_id = fid.or(e.file_id.take());
                }
            }
            Action::Upload { path, to } => {
                let Some(l) = self.local.get(path).cloned() else {
                    return;
                };
                match to {
                    UploadTo::New { parent, name } => {
                        let Some(par) = self.parent_node(parent, made) else {
                            return;
                        };
                        if self.server_child(par, name).is_some() {
                            return;
                        }
                        let v = self.new_version(l.content);
                        let n = self.uuid();
                        self.server.insert(
                            n,
                            SNode {
                                parent: par,
                                name: name.clone(),
                                is_file: true,
                                version: Some(v),
                                trashed: false,
                            },
                        );
                        self.put(n, par, name, true, Some(v), path);
                    }
                    UploadTo::Version { node, base } => {
                        let Some(s) = self.server.get(node).cloned() else {
                            return;
                        };
                        if s.trashed {
                            return;
                        }
                        // The server's "unchanged": the current version's
                        // content, or a stale base's (the newer stays current).
                        let content = |v: Option<Uuid>| v.map(|v| self.versions[&v]);
                        let v = if content(s.version) == Some(l.content) {
                            s.version.unwrap()
                        } else if *base != s.version && content(*base) == Some(l.content) {
                            base.unwrap()
                        } else {
                            let v = self.new_version(l.content);
                            self.server.get_mut(node).unwrap().version = Some(v);
                            v
                        };
                        let e = self.base_entry(*node).cloned().unwrap();
                        self.put(*node, e.parent, &e.name, true, Some(v), path);
                    }
                }
            }
            Action::LocalMove { node, parent, name } => {
                let (Some(from), Some(mut to)) =
                    (self.base_path(*node), self.local_parent(*parent))
                else {
                    return;
                };
                to.push(name.clone());
                if from != to && self.local.contains_key(&to) {
                    return;
                }
                let moved: Vec<_> = self
                    .local
                    .keys()
                    .filter(|k| k.starts_with(&from))
                    .cloned()
                    .collect();
                for k in moved {
                    let v = self.local.remove(&k).unwrap();
                    let mut nk = to.clone();
                    nk.extend_from_slice(&k[from.len()..]);
                    self.local.insert(nk, v);
                }
                let e = self.base.iter_mut().find(|e| e.node == *node).unwrap();
                e.parent = *parent;
                e.name = name.clone();
            }
            Action::LocalMkdir { node, parent, name } => {
                let Some(mut at) = self.local_parent(*parent) else {
                    return;
                };
                at.push(name.clone());
                if !self.local.contains_key(&at) {
                    let fid = self.fresh();
                    self.local.insert(
                        at.clone(),
                        LFile {
                            is_file: false,
                            content: 0,
                            fid,
                        },
                    );
                }
                self.put(*node, *parent, name, false, None, &at);
            }
            Action::Download {
                node,
                parent,
                name,
                state,
            } => {
                let target = match self.base_entry(*node) {
                    Some(_) => self.base_path(*node),
                    None => self.local_parent(*parent).map(|mut l| {
                        l.push(name.clone());
                        l
                    }),
                };
                let Some(target) = target else { return };
                let ok = match self.base_entry(*node) {
                    Some(e) => self.fingerprint_matches(&target, &e.clone()),
                    None => !self.local.contains_key(&target),
                };
                if !ok || target.len() > 1 && !self.local.contains_key(&target[..target.len() - 1])
                {
                    return;
                }
                let content = self.versions[&state.version.unwrap()];
                let fid = self.fresh();
                self.local.insert(
                    target.clone(),
                    LFile {
                        is_file: true,
                        content,
                        fid,
                    },
                );
                let e = self.base_entry(*node).cloned();
                let (par, nm) = e.map_or((*parent, name.clone()), |e| (e.parent, e.name));
                self.put(*node, par, &nm, true, state.version, &target);
            }
            Action::Merge { node, path, state } => {
                let Some(l) = self.local.get(path).cloned() else {
                    return;
                };
                let cur = state.version.map(|v| self.versions[&v]);
                let v = if cur == Some(l.content) {
                    state.version.unwrap()
                } else {
                    self.new_version(l.content)
                };
                self.server.get_mut(node).unwrap().version = Some(v);
                self.put(*node, state.parent, &state.name, true, Some(v), path);
            }
            Action::Adopt { node, path, state } => {
                let version = state.version.filter(|_| state.is_file);
                self.put(
                    *node,
                    state.parent,
                    &state.name,
                    state.is_file,
                    version,
                    path,
                );
            }
            Action::RemoteTrash { node } => {
                self.trash_tree(*node);
                self.forget_tree(*node);
            }
            Action::LocalDelete { node } => {
                let Some(e) = self.base_entry(*node).cloned() else {
                    return;
                };
                let Some(path) = self.base_path(*node) else {
                    return;
                };
                if e.is_file {
                    if !self.fingerprint_matches(&path, &e) {
                        return;
                    }
                    self.local.remove(&path);
                } else if self
                    .local
                    .keys()
                    .any(|k| k.len() > path.len() && k.starts_with(&path))
                {
                    return;
                } else {
                    self.local.remove(&path);
                }
                self.base.retain(|x| x.node != *node);
            }
            Action::KeepAside { node } => {
                let Some(path) = self.base_path(*node) else {
                    return;
                };
                if let Some(l) = self.local.remove(&path) {
                    let mut aside = path.clone();
                    let mut k = 1;
                    loop {
                        let suffix = if k == 1 {
                            String::new()
                        } else {
                            format!(" {k}")
                        };
                        *aside.last_mut().unwrap() =
                            format!("{} (local{suffix})", path.last().unwrap());
                        if !self.local.contains_key(&aside) {
                            break;
                        }
                        k += 1;
                    }
                    self.local.insert(aside, l);
                }
            }
            Action::Forget { node } => self.base.retain(|e| e.node != *node),
        }
    }

    fn round(&mut self, writable: bool) {
        let scan = self.scan();
        let pending = self.pending();
        let base = self.base.clone();
        let plan = plan(&Input {
            root: ROOT,
            base: &base,
            scan: &scan,
            remote: &pending,
            writable,
        });
        let mut made = BTreeMap::new();
        for a in &plan.actions {
            self.apply(a, &mut made);
        }
    }

    fn local_contents(&self) -> HashSet<u32> {
        self.local
            .values()
            .filter(|l| l.is_file)
            .map(|l| l.content)
            .collect()
    }
}

#[derive(Clone, Debug)]
enum Op {
    Edit(usize),
    Delete(usize),
    Rename(usize, usize),
    Create(usize),
}

fn op() -> impl Strategy<Value = (bool, Op)> {
    // Edits weigh most: a loss needs them to meet (on both sides, again).
    let o = prop_oneof![
        4 => (0..5usize).prop_map(Op::Edit),
        1 => (0..5usize).prop_map(Op::Delete),
        1 => (0..5usize, 0..5usize).prop_map(|(a, b)| Op::Rename(a, b)),
        1 => (0..5usize).prop_map(Op::Create),
    ];
    (any::<bool>(), o)
}

fn local_op(w: &mut World, op: &Op) {
    let keys: Vec<_> = w.local.keys().cloned().collect();
    let pick = |i: usize| keys.get(i % keys.len().max(1)).cloned();
    match op {
        Op::Edit(i) => {
            if let Some(k) = pick(*i) {
                let c = w.fresh();
                if let Some(l) = w.local.get_mut(&k).filter(|l| l.is_file) {
                    l.content = c;
                }
            }
        }
        Op::Delete(i) => {
            if let Some(k) = pick(*i) {
                w.local.retain(|x, _| !x.starts_with(&k));
            }
        }
        Op::Rename(i, t) => {
            let to = p(PATHS[*t]);
            if let Some(from) = pick(*i)
                && !w.local.contains_key(&to)
                && (to.len() == 1 || w.local.contains_key(&to[..1]))
                && !to.starts_with(&from)
            {
                let moved: Vec<_> = w
                    .local
                    .keys()
                    .filter(|k| k.starts_with(&from))
                    .cloned()
                    .collect();
                for k in moved {
                    let v = w.local.remove(&k).unwrap();
                    let mut nk = to.clone();
                    nk.extend_from_slice(&k[from.len()..]);
                    w.local.insert(nk, v);
                }
            }
        }
        Op::Create(t) => {
            let to = p(PATHS[*t]);
            if !w.local.contains_key(&to)
                && (to.len() == 1 || w.local.get(&to[..1]).is_some_and(|l| !l.is_file))
            {
                let c = w.fresh();
                let fid = w.fresh();
                w.local.insert(
                    to.clone(),
                    LFile {
                        is_file: PATHS[*t] != "d",
                        content: c,
                        fid,
                    },
                );
            }
        }
    }
}

fn remote_op(w: &mut World, op: &Op) {
    let live: Vec<Uuid> = w
        .server
        .keys()
        .copied()
        .filter(|n| w.server_path(*n).is_some())
        .collect();
    let pick = |i: usize| live.get(i % live.len().max(1)).copied();
    match op {
        Op::Edit(i) => {
            if let Some(n) = pick(*i)
                && w.server[&n].is_file
            {
                let c = w.fresh();
                let v = w.new_version(c);
                w.server.get_mut(&n).unwrap().version = Some(v);
            }
        }
        Op::Delete(i) => {
            if let Some(n) = pick(*i) {
                w.trash_tree(n);
            }
        }
        Op::Rename(i, t) => {
            let to = p(PATHS[*t]);
            let parent = if to.len() == 1 {
                Some(ROOT)
            } else {
                w.server_at(&to[..1])
            };
            if let (Some(n), Some(par)) = (pick(*i), parent)
                && w.server_at(&to).is_none()
                && par != n
                && !w
                    .server_path(par)
                    .unwrap_or_default()
                    .starts_with(&w.server_path(n).unwrap())
            {
                let s = w.server.get_mut(&n).unwrap();
                s.parent = par;
                s.name = to.last().unwrap().clone();
            }
        }
        Op::Create(t) => {
            let to = p(PATHS[*t]);
            let parent = if to.len() == 1 {
                Some(ROOT)
            } else {
                w.server_at(&to[..1])
            };
            if let Some(par) = parent
                && w.server_at(&to).is_none()
            {
                let is_file = PATHS[*t] != "d";
                let version = is_file.then(|| {
                    let c = w.fresh();
                    w.new_version(c)
                });
                let n = w.uuid();
                w.server.insert(
                    n,
                    SNode {
                        parent: par,
                        name: to.last().unwrap().clone(),
                        is_file,
                        version,
                        trashed: false,
                    },
                );
            }
        }
    }
}

/// Cases per run: enough to find a loss two edits deep on both sides in
/// seconds (`PROPTEST_CASES` for more).
fn config() -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|n| n.parse().ok());
    ProptestConfig {
        cases: cases.unwrap_or(5_000),
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn no_local_content_lost(files in proptest::sample::subsequence(vec!["a", "b", "d/a", "d/b"], 0..=4),
                             phases in proptest::collection::vec(proptest::collection::vec(op(), 0..4), 1..6),
                             writable in any::<bool>()) {
        let mut w = World::synced(&files);
        for ops in &phases {
            for (local, o) in ops {
                if *local { local_op(&mut w, o) } else { remote_op(&mut w, o) }
            }
            for round in 0..3 {
                let before = w.local_contents();
                w.round(writable);
                let after = w.local_contents();
                for c in before {
                    prop_assert!(after.contains(&c) || w.ever.contains(&c),
                        "round {round}: local content {c} lost; world: {w:#?}");
                }
            }
        }
    }
}
