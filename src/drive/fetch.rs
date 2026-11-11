//! Fetching a drive's files when opened, beside the account's rounds: the
//! file system asks from its own threads and waits; the account's engine
//! serves a few at a time, each in its own task.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use theca_client::{Cancel, Client, DownloadOptions, Downloader, Sink};
use tokio::sync::{Semaphore, mpsc, oneshot};
use uuid::Uuid;

use crate::state::{Entry, Pair, State};

/// Fetches running at once, per account.
const AT_ONCE: usize = 4;

struct Request {
    pair: Pair,
    /// Below the pair's backing folder.
    path: PathBuf,
    /// A fetch, waited for; none: only its `used` stamped (an open of a
    /// downloaded file).
    done: Option<oneshot::Sender<Result<(), String>>>,
    /// Where the content goes: a node's version into what Windows hands
    /// over; none: landed in the backing folder (FUSE).
    into: Option<(Uuid, Box<dyn Sink + Send>)>,
}

/// What a drive's file system asks fetches of.
#[derive(Clone)]
pub struct Fetcher(mpsc::UnboundedSender<Request>);

impl Fetcher {
    /// The file at `path` (below the backing folder of `pair`, as its rounds
    /// see it) fetched, waiting for it: from the file system's threads, never
    /// from the runtime's. The caller holds the file meanwhile (`Busy`).
    pub fn fetch(&self, pair: &Pair, path: &Path) -> Result<(), String> {
        let (done, fetched) = oneshot::channel();
        self.request(Request {
            pair: pair.clone(),
            path: path.to_owned(),
            done: Some(done),
            into: None,
        })?;
        fetched
            .blocking_recv()
            .unwrap_or_else(|_| Err("Theca is stopping".into()))
    }

    /// A node's version (the entry's, as its placeholder shows it) written
    /// into `sink` (Windows' transfer), waiting for it; `path` for its
    /// notices and `used`.
    pub fn fetch_node(
        &self,
        pair: &Pair,
        node: Uuid,
        path: &Path,
        sink: Box<dyn Sink + Send>,
    ) -> Result<(), String> {
        let (done, fetched) = oneshot::channel();
        self.request(Request {
            pair: pair.clone(),
            path: path.to_owned(),
            done: Some(done),
            into: Some((node, sink)),
        })?;
        fetched
            .blocking_recv()
            .unwrap_or_else(|_| Err("Theca is stopping".into()))
    }

    fn request(&self, r: Request) -> Result<(), String> {
        self.0.send(r).map_err(|_| "Theca is stopping".to_owned())
    }

    /// A downloaded file opened: its `used` stamped, not waited for.
    pub fn used(&self, pair: &Pair, path: &Path) {
        let _ = self.0.send(Request {
            pair: pair.clone(),
            path: path.to_owned(),
            done: None,
            into: None,
        });
    }
}

/// The fetcher, and what serves it: run by the account's engine, until the
/// last fetcher is dropped.
/// `fetched` is raised after each fetch (the drive limit is checked then).
pub fn service(
    state: Arc<State>,
    client: Client,
    cancel: Cancel,
    fetched: Arc<AtomicBool>,
) -> (Fetcher, impl Future<Output = ()>) {
    let (send, mut requests) = mpsc::unbounded_channel::<Request>();
    let serve = async move {
        let slots = Arc::new(Semaphore::new(AT_ONCE));
        while let Some(r) = requests.recv().await {
            let (state, client, cancel, slots, fetched) = (
                state.clone(),
                client.clone(),
                cancel.clone(),
                slots.clone(),
                fetched.clone(),
            );
            tokio::spawn(async move {
                let Some(done) = r.done else {
                    let _ = stamp(&state, &r.pair, &r.path);
                    return;
                };
                let _slot = slots.acquire().await;
                let result = match r.into {
                    Some((node, sink)) => {
                        fetch_into(&state, &client, &r.pair, node, &r.path, &*sink, &cancel).await
                    }
                    None => fetch(&state, &client, &r.pair, &r.path, &cancel).await,
                };
                fetched.store(true, Ordering::Relaxed);
                let _ = done.send(result);
            });
        }
    };
    (Fetcher(send), serve)
}

fn names(path: &Path) -> Vec<String> {
    path.iter()
        .map(|n| n.to_string_lossy().into_owned())
        .collect()
}

/// A drive's file used now.
fn stamp(state: &State, pair: &Pair, path: &Path) -> crate::state::Result<()> {
    if let Some(e) = state.entry_at(pair.id, pair.root, &names(path))? {
        state.set_used(pair.id, e.node, chrono::Utc::now().timestamp())?;
    }
    Ok(())
}

/// A node's version, as its entry has it, downloaded into `sink`.
async fn fetch_into(
    state: &State,
    client: &Client,
    pair: &Pair,
    node: Uuid,
    path: &Path,
    sink: &dyn Sink,
    cancel: &Cancel,
) -> Result<(), String> {
    let display = names(path).join("/");
    // A placeholder shows a moment before its round records its entry: an
    // open right then waits for it.
    let mut entry = None;
    for _ in 0..50 {
        entry = state.entry(pair.id, node).map_err(|e| e.to_string())?;
        if entry.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let entry = entry.ok_or_else(|| format!("{display}: not in the library (yet)"))?;
    let options = DownloadOptions::default();
    let (downloader, run) = Downloader::new(client, &options, cancel, |_| {});
    let fetched = downloader.submit(node, entry.version, sink);
    drop(downloader);
    let (_, result) = futures::join!(run, fetched);
    match result {
        Ok(_) => state
            .set_used(pair.id, node, chrono::Utc::now().timestamp())
            .map_err(|e| e.to_string()),
        Err(e) => Err(not_opened(state, pair, &display, &e)),
    }
}

/// Why a file didn't open, said in a notice.
fn not_opened(state: &State, pair: &Pair, display: &str, e: &theca_client::Error) -> String {
    let message = if e.transient() {
        "not opened: it isn't downloaded, and the server can't be reached".to_owned()
    } else {
        format!("not opened: {e}")
    };
    let _ = state.notify(
        Some(pair.account),
        Some(pair.id),
        "error",
        Some(display),
        &message,
        None,
    );
    message
}

async fn fetch(
    state: &State,
    client: &Client,
    pair: &Pair,
    path: &Path,
    cancel: &Cancel,
) -> Result<(), String> {
    let names = names(path);
    let display = names.join("/");
    let target = pair.path.join(path);
    // The entry at that path, if the placeholder there is its own file: one
    // renamed over it waits for the round recording that (a moment), as a
    // placeholder shown before its entry does.
    let here = crate::local::file_id(&target);
    let ours = |e: &Entry| e.is_file && (e.file_id.is_none() || e.file_id == here);
    let mut entry = None;
    for _ in 0..50 {
        entry = state
            .entry_at(pair.id, pair.root, &names)
            .map_err(|e| e.to_string())?
            .filter(ours);
        if entry.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let entry = entry.ok_or_else(|| format!("{display}: not in the library (yet)"))?;
    match crate::exec::fetch(client, state, pair, entry, &target, cancel).await {
        Ok(e) => {
            let now = chrono::Utc::now().timestamp();
            state
                .set_used(pair.id, e.node, now)
                .map_err(|e| e.to_string())
        }
        Err(e) => Err(not_opened(state, pair, &display, &e)),
    }
}
