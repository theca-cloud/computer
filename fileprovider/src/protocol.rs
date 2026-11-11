//! Between the extension and the app: one JSON request per connection, one
//! JSON reply (`Reply`), on the socket `SOCKET` in the extension's container.
//! A domain is a drive, named after its library; an item is a node, the
//! root `ROOT`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The extension's bundle identifier (its container is
/// `~/Library/Containers/<it>/Data`).
pub const EXTENSION: &str = "rs.theca.computer.drive";
/// The socket's name in that container.
pub const SOCKET: &str = "theca.sock";
/// The system's name for a domain's root item.
pub const ROOT: &str = "NSFileProviderRootContainerItemIdentifier";
/// The system's working set: what it asks changes for.
pub const WORKING_SET: &str = "NSFileProviderWorkingSetContainerItemIdentifier";
/// Finder's actions (the extension's `Info.plist`): Always Keep on This Mac,
/// and Free Up Space.
pub const KEEP: &str = "rs.theca.computer.drive.keep";
pub const FREE: &str = "rs.theca.computer.drive.free";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub enum Request {
    /// One item.
    Item { domain: String, id: String },
    /// A folder's items (`ROOT` for the root; the working set lists none:
    /// its changes say everything).
    List { domain: String, container: String },
    /// The anchor now: the drive's counter.
    Anchor { domain: String },
    /// What changed past `anchor`.
    Changes { domain: String, anchor: u64 },
    /// An item's content (its version as the item shows it) written to
    /// `into`, a file the extension can read, then handed to the system.
    Fetch {
        domain: String,
        id: String,
        into: PathBuf,
    },
    /// A file or folder made locally; a file's content in `contents`.
    Create {
        domain: String,
        parent: String,
        name: String,
        folder: bool,
        contents: Option<PathBuf>,
    },
    /// An item changed locally: its content (on the version `base`), its
    /// name or folder, or its last use (Unix seconds).
    Modify {
        domain: String,
        id: String,
        base: Option<String>,
        name: Option<String>,
        parent: Option<String>,
        contents: Option<PathBuf>,
        used: Option<i64>,
    },
    /// An item deleted locally (to the server's trash, its children with it).
    Delete { domain: String, id: String },
    /// The system's materialized items (downloaded), all of them.
    Materialized { domain: String, ids: Vec<String> },
    /// Items kept on this Mac (Finder's Always Keep on This Mac), as `theca
    /// keep` does.
    Keep { domain: String, ids: Vec<String> },
    /// Items freed (Finder's Free Up Space), as `theca free` does: not kept
    /// anymore, their files evicted.
    Free { domain: String, ids: Vec<String> },
}

impl Request {
    pub fn domain(&self) -> &str {
        match self {
            Request::Item { domain, .. }
            | Request::List { domain, .. }
            | Request::Anchor { domain }
            | Request::Changes { domain, .. }
            | Request::Fetch { domain, .. }
            | Request::Create { domain, .. }
            | Request::Modify { domain, .. }
            | Request::Delete { domain, .. }
            | Request::Materialized { domain, .. }
            | Request::Keep { domain, .. }
            | Request::Free { domain, .. } => domain,
        }
    }
}

/// An item as the system shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub parent: String,
    pub name: String,
    pub folder: bool,
    pub size: u64,
    /// Its content's version (the server's); empty for a folder.
    pub version: String,
    /// Unix seconds.
    pub modified: Option<i64>,
    /// Kept downloaded (it, or a folder above it).
    pub kept: bool,
    /// Kept itself (not only through a folder above): what Free Up
    /// Space undoes.
    pub kept_itself: bool,
    /// Writable (a read-only library's aren't).
    pub writable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Item(Item),
    Items {
        items: Vec<Item>,
    },
    Anchor {
        anchor: u64,
    },
    Changes {
        updated: Vec<Item>,
        deleted: Vec<String>,
        anchor: u64,
    },
    Done,
    Error {
        code: Code,
        message: String,
    },
}

/// What the extension tells the system when a call fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    NoSuchItem,
    /// Offline, or the app not answering: tried again by the system.
    ServerUnreachable,
    /// Refused for good (a read-only library, a name the server refuses).
    CannotSynchronize,
    /// The anchor isn't one this state gave (a state lost, made again): the
    /// system lists again.
    SyncAnchorExpired,
    NotAuthenticated,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What goes over the socket reads back as it was.
    #[test]
    fn requests_and_replies_round_trip() {
        let request = Request::Modify {
            domain: "d".into(),
            id: "n".into(),
            base: Some("v".into()),
            name: None,
            parent: Some(ROOT.into()),
            contents: Some("/tmp/x".into()),
            used: Some(7),
        };
        let line = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), request);
        assert_eq!(request.domain(), "d");
        let reply = Reply::Error {
            code: Code::SyncAnchorExpired,
            message: "relist".into(),
        };
        let line = serde_json::to_string(&reply).unwrap();
        assert!(line.contains("\"sync_anchor_expired\""), "{line}");
        assert_eq!(serde_json::from_str::<Reply>(&line).unwrap(), reply);
    }
}
