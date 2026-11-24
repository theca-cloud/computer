//! macOS's drive, the app's side, on every OS: the extension's calls
//! (`protocol::Request`) answered as the socket answers them, against the
//! real server. A drive's entries come from the feed (here a listing,
//! applied as a macOS drive's round is).

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use common::*;
use theca::drive::provider::{Provider, apply_remote, domain_of, item_id, kept_files, still_kept};
use theca_common::api;
use theca_fileprovider::protocol::{Item, ROOT, Reply, Request};

fn names(items: &[Item]) -> BTreeSet<String> {
    items.iter().map(|i| i.name.clone()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_app_answers_the_extension() {
    let w = world().await;
    let s = w.device("sync").await;
    s.write("a.txt", "hello");
    s.write("d/b.txt", "inside");
    w.converge(&[&s]).await;

    // A Mac's drive: no engine of its own here, its entries from a listing.
    let d = w.device_for("mac", &w.library, false).await;
    let state = Arc::new(d.state());
    let pair = d.pair();
    let client = theca::ops::client(&d.account).unwrap();
    let listed = theca::engine::tree(&client, pair.root).await.unwrap();
    state
        .record_remote(pair.id, &listed, Some(api::Position::Seq(1)))
        .unwrap();
    assert!(apply_remote(&state, &pair).unwrap());
    let provider = Provider::default();
    let uuid = state.pair_uuid(pair.id).unwrap().unwrap();
    provider.serve(&pair, uuid, client, state.clone(), Default::default(), true);
    let domain = domain_of(&state).unwrap();
    let ask = |r: Request| provider.answer(r);

    // The domain's root lists the drives, each a folder named as its own.
    let Reply::Items { items: drives } = ask(Request::List {
        domain: domain.clone(),
        container: ROOT.into(),
    })
    .await
    else {
        panic!("no drives listed");
    };
    assert_eq!(drives.len(), 1);
    let drive = drives[0].clone();
    assert_eq!(drive.id, item_id(uuid, None));
    assert_eq!(drive.name, pair.path.file_name().unwrap().to_string_lossy());
    assert!(drive.folder && drive.fixed && drive.parent == ROOT);
    let Reply::Items { items: root } = ask(Request::List {
        domain: domain.clone(),
        container: drive.id.clone(),
    })
    .await
    else {
        panic!("no listing");
    };
    assert_eq!(names(&root), ["a.txt", "d"].map(String::from).into());
    assert!(
        root.iter()
            .all(|i| !i.fixed && i.id.starts_with(&format!("{uuid}/")))
    );
    let a = root.iter().find(|i| i.name == "a.txt").unwrap().clone();
    let folder = root.iter().find(|i| i.name == "d").unwrap().clone();
    assert!(!a.folder && a.size == 5 && folder.folder);
    let Reply::Anchor { anchor } = ask(Request::Anchor {
        domain: domain.clone(),
    })
    .await
    else {
        panic!("no anchor");
    };

    // Finder's Always Keep on This Mac on a folder: kept itself, what is below kept;
    // the drive itself, or an item not in the library, refused, and then
    // nothing is kept.
    let keep = |ids: Vec<String>, kept: bool| {
        let domain = domain.clone();
        if kept {
            ask(Request::Keep { domain, ids })
        } else {
            ask(Request::Free { domain, ids })
        }
    };
    let listed = |container: String| {
        let ask = &ask;
        let domain = domain.clone();
        async move {
            match ask(Request::List { domain, container }).await {
                Reply::Items { items } => items,
                other => panic!("no listing: {other:?}"),
            }
        }
    };
    for (refused, why) in [
        (drive.id.clone(), "not the drive itself"),
        (ROOT.to_owned(), "no item"),
        (uuid::Uuid::now_v7().to_string(), "no item"),
    ] {
        let reply = keep(vec![a.id.clone(), refused.clone()], true).await;
        assert!(
            matches!(&reply, Reply::Error { message, .. } if message.contains(why)),
            "{refused}: {reply:?}"
        );
    }
    assert!(
        listed(drive.id.clone())
            .await
            .iter()
            .all(|i| !i.kept && !i.kept_itself)
    );
    assert_eq!(keep(vec![folder.id.clone()], true).await, Reply::Done);
    let shown = listed(drive.id.clone()).await;
    let d = shown.iter().find(|i| i.name == "d").unwrap();
    assert!(d.kept && d.kept_itself, "{d:?}");
    assert!(shown.iter().any(|i| i.name == "a.txt" && !i.kept));
    let below = listed(folder.id.clone()).await;
    assert!(below[0].kept && !below[0].kept_itself, "{below:?}");
    let Reply::Changes { updated, .. } = ask(Request::Changes {
        domain: domain.clone(),
        anchor: anchor.clone(),
    })
    .await
    else {
        panic!("no changes");
    };
    assert_eq!(names(&updated), ["d".to_owned()].into());
    // What the engine downloads then: the kept folder's file, by its path in
    // the drive; not once freed.
    let b: uuid::Uuid = below[0].id.split_once('/').unwrap().1.parse().unwrap();
    assert_eq!(
        kept_files(&state, &pair).unwrap(),
        [(b, pair.path.join("d").join("b.txt"))]
    );
    assert!(still_kept(&state, &pair, b).unwrap());
    assert_eq!(keep(vec![folder.id.clone()], false).await, Reply::Done);
    assert!(kept_files(&state, &pair).unwrap().is_empty());
    assert!(!still_kept(&state, &pair, b).unwrap());
    assert!(listed(folder.id.clone()).await.iter().all(|i| !i.kept));

    // Fetched into the file the extension names.
    let dir = tempfile::tempdir().unwrap();
    let into = dir.path().join("a.fetched");
    let fetched = ask(Request::Fetch {
        domain: domain.clone(),
        id: a.id.clone(),
        into: into.clone(),
    })
    .await;
    assert!(matches!(fetched, Reply::Item(_)), "{fetched:?}");
    assert_eq!(std::fs::read_to_string(&into).unwrap(), "hello");

    // Made, edited, renamed, deleted on the Mac: on the server.
    let made = dir.path().join("new.txt");
    std::fs::write(&made, "made on a Mac").unwrap();
    let created = ask(Request::Create {
        domain: domain.clone(),
        parent: drive.id.clone(),
        name: "new.txt".into(),
        folder: false,
        contents: Some(made),
    })
    .await;
    let Reply::Item(new) = created else {
        panic!("not created: {created:?}");
    };
    let edit = dir.path().join("a.edited");
    std::fs::write(&edit, "hello, edited").unwrap();
    let modified = ask(Request::Modify {
        domain: domain.clone(),
        id: a.id.clone(),
        base: Some(a.version.clone()),
        name: None,
        parent: None,
        contents: Some(edit),
        used: None,
    })
    .await;
    assert!(matches!(modified, Reply::Item(_)), "{modified:?}");
    let Reply::Items { items: inside } = ask(Request::List {
        domain: domain.clone(),
        container: folder.id.clone(),
    })
    .await
    else {
        panic!("no listing");
    };
    let b = inside[0].clone();
    let renamed = ask(Request::Modify {
        domain: domain.clone(),
        id: b.id.clone(),
        base: None,
        name: Some("c.txt".into()),
        parent: Some(drive.id.clone()),
        contents: None,
        used: Some(1_800_000_000),
    })
    .await;
    assert!(
        matches!(&renamed, Reply::Item(i) if i.name == "c.txt" && i.parent == drive.id),
        "{renamed:?}"
    );
    let deleted = ask(Request::Delete {
        domain: domain.clone(),
        id: new.id.clone(),
    })
    .await;
    assert_eq!(deleted, Reply::Done);
    eventually("the Mac's changes on the server", || async {
        read(&s.path("a.txt")) == "hello, edited"
            && read(&s.path("c.txt")) == "inside"
            && !s.path("d/b.txt").exists()
            && !s.path("new.txt").exists()
    })
    .await;

    // What changed past the first anchor, as the system asks.
    let Reply::Changes {
        updated,
        deleted,
        anchor: now,
    } = ask(Request::Changes {
        domain: domain.clone(),
        anchor: anchor.clone(),
    })
    .await
    else {
        panic!("no changes");
    };
    assert_ne!(now, anchor);
    assert_eq!(
        names(&updated),
        ["a.txt", "c.txt", "d"].map(String::from).into()
    );
    assert_eq!(deleted, [new.id]);
}

fn read(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// A Mac's drive of the state, its entries from a listing (as a macOS drive's
/// round applies them), served by `provider`: its pair and UUID.
async fn drive_of(
    w: &World,
    state: &Arc<theca::state::State>,
    account: &theca::state::Account,
    library: &api::Library,
    path: std::path::PathBuf,
    provider: &Provider,
) -> (theca::state::Pair, uuid::Uuid) {
    let client = theca::ops::client(account).unwrap();
    let listed = theca::engine::tree(&client, library.root).await.unwrap();
    let mut pair = theca::state::Pair {
        id: 0,
        account: account.id,
        library: library.id,
        root: library.root,
        library_name: library.name.clone(),
        path,
        root_file_id: None,
        position: None,
        position_event: None,
        paused: false,
        stop: None,
        writable: true,
        activity: String::new(),
        error: None,
        synced_at: None,
        mode: theca::state::Mode::Drive,
    };
    pair.id = state.add_pair(&pair).unwrap();
    state
        .record_remote(pair.id, &listed, Some(api::Position::Seq(1)))
        .unwrap();
    assert!(apply_remote(state, &pair).unwrap());
    let uuid = state.pair_uuid(pair.id).unwrap().unwrap();
    provider.serve(&pair, uuid, client, state.clone(), Default::default(), true);
    let _ = w;
    (pair, uuid)
}

/// Two drives in one domain (theca/computer#94): the same library under two
/// accounts, each its own folder of the drives folder, its items' identifiers
/// its own; the drives folder and a drive's folder take no keep, free,
/// rename nor delete; an item doesn't leave its drive; a drive added after the
/// anchor comes whole, one removed leaves its folder deleted; a drive paused
/// still lists but fetches nothing.
#[tokio::test(flavor = "multi_thread")]
async fn two_drives_are_folders_of_one_domain() {
    let w = world().await;
    let notes = w
        .admin
        .create_library(w.organization, "Notes")
        .await
        .unwrap();
    w.upload(&notes, "n.txt", b"notes").await;
    let (dir, state_path, first) = w.logged_in("provider").await;
    let state = Arc::new(theca::state::State::open(&state_path).unwrap());
    let second = w
        .second_account(&state, dir.path(), "two@test.theca", &notes)
        .await;
    let provider = Provider::default();
    let domain = domain_of(&state).unwrap();
    let ask = |r: Request| provider.answer(r);
    let (one, one_id) = drive_of(
        &w,
        &state,
        &first,
        &notes,
        dir.path().join("Notes"),
        &provider,
    )
    .await;
    // A drive the system was never told of isn't in the anchor, so that the
    // first changes it asks for bring the drive's folder (a system that
    // doesn't list the domain gets it that way).
    let first_anchor = match ask(Request::Anchor {
        domain: domain.clone(),
    })
    .await
    {
        Reply::Anchor { anchor } => anchor,
        other => panic!("no anchor: {other:?}"),
    };
    assert_eq!(first_anchor, "");
    let Reply::Changes { updated, .. } = ask(Request::Changes {
        domain: domain.clone(),
        anchor: first_anchor,
    })
    .await
    else {
        panic!("no changes");
    };
    assert!(names(&updated).contains("Notes"), "{:?}", names(&updated));
    let anchor_with_one = match ask(Request::Anchor {
        domain: domain.clone(),
    })
    .await
    {
        Reply::Anchor { anchor } => anchor,
        other => panic!("no anchor: {other:?}"),
    };
    // A second drive, after the anchor: whole, with its folder.
    let (two, two_id) = drive_of(
        &w,
        &state,
        &second,
        &notes,
        dir.path().join("Acme - Notes"),
        &provider,
    )
    .await;
    assert_ne!(one_id, two_id);
    let Reply::Items { items: drives } = ask(Request::List {
        domain: domain.clone(),
        container: ROOT.into(),
    })
    .await
    else {
        panic!("no drives");
    };
    assert_eq!(
        names(&drives),
        ["Notes", "Acme - Notes"].map(String::from).into()
    );
    let Reply::Changes {
        updated, deleted, ..
    } = ask(Request::Changes {
        domain: domain.clone(),
        anchor: anchor_with_one.clone(),
    })
    .await
    else {
        panic!("no changes");
    };
    assert_eq!(
        names(&updated),
        ["Acme - Notes", "n.txt"].map(String::from).into()
    );
    assert!(deleted.is_empty(), "{deleted:?}");
    // The system touching a drive's folder (no change) is told it is as it is.
    let touched = ask(Request::Modify {
        domain: domain.clone(),
        id: format!("{one_id}/"),
        base: None,
        name: None,
        parent: None,
        contents: None,
        used: None,
    })
    .await;
    assert!(
        matches!(&touched, Reply::Item(item) if item.id == format!("{one_id}/")),
        "{touched:?}"
    );
    // The same node in both: each answers its own.
    let node = state.entries(one.id).unwrap()[0].node;
    for (id, pair) in [(one_id, &one), (two_id, &two)] {
        let reply = ask(Request::Item {
            domain: domain.clone(),
            id: item_id(id, Some(node)),
        })
        .await;
        assert!(
            matches!(&reply, Reply::Item(i) if i.id == item_id(id, Some(node)) && i.name == "n.txt"),
            "{} {reply:?}",
            pair.library_name
        );
    }

    // What isn't Theca's to do: nothing is made at the top; a drive isn't
    // renamed, moved, deleted nor kept; an item doesn't leave its drive.
    let refused = |reply: Reply| matches!(reply, Reply::Error { .. });
    assert!(refused(
        ask(Request::Create {
            domain: domain.clone(),
            parent: ROOT.into(),
            name: "x".into(),
            folder: true,
            contents: None,
        })
        .await
    ));
    for call in [
        Request::Modify {
            domain: domain.clone(),
            id: item_id(one_id, None),
            base: None,
            name: Some("renamed".into()),
            parent: None,
            contents: None,
            used: None,
        },
        Request::Delete {
            domain: domain.clone(),
            id: item_id(one_id, None),
        },
        Request::Keep {
            domain: domain.clone(),
            ids: vec![item_id(one_id, None)],
        },
        Request::Free {
            domain: domain.clone(),
            ids: vec![item_id(one_id, None)],
        },
        Request::Modify {
            domain: domain.clone(),
            id: item_id(one_id, Some(node)),
            base: None,
            name: None,
            parent: Some(item_id(two_id, None)),
            contents: None,
            used: None,
        },
        Request::Keep {
            domain: domain.clone(),
            ids: vec![item_id(one_id, Some(node)), item_id(two_id, Some(node))],
        },
    ] {
        let reply = ask(call.clone()).await;
        assert!(refused(reply.clone()), "{call:?} -> {reply:?}");
    }

    // A drive paused still lists, and fetches nothing.
    provider.serve(
        &one,
        one_id,
        theca::ops::client(&first).unwrap(),
        state.clone(),
        Default::default(),
        false,
    );
    let listed = ask(Request::List {
        domain: domain.clone(),
        container: item_id(one_id, None),
    })
    .await;
    assert!(
        matches!(&listed, Reply::Items { items } if items.len() == 1),
        "{listed:?}"
    );
    let into = dir.path().join("n.fetched");
    let fetched = ask(Request::Fetch {
        domain: domain.clone(),
        id: item_id(one_id, Some(node)),
        into: into.clone(),
    })
    .await;
    assert!(
        matches!(
            &fetched,
            Reply::Error {
                code: theca_fileprovider::protocol::Code::ServerUnreachable,
                ..
            }
        ),
        "{fetched:?}"
    );
    assert!(!into.exists());

    // A drive removed (no longer listed): its folder reported deleted.
    let Reply::Anchor { anchor: both } = ask(Request::Anchor {
        domain: domain.clone(),
    })
    .await
    else {
        panic!("no anchor");
    };
    // Not listed any more by this process, but still a drive of the state (its
    // account signed out, its engine not started yet): nothing is deleted.
    provider.forget(two_id);
    let Reply::Changes { deleted, .. } = ask(Request::Changes {
        domain: domain.clone(),
        anchor: both.clone(),
    })
    .await
    else {
        panic!("no changes");
    };
    assert!(deleted.is_empty(), "{deleted:?}");
    // Out of the state: its folder is gone.
    state.remove_pair(&two).unwrap();
    let Reply::Changes { deleted, .. } = ask(Request::Changes {
        domain: domain.clone(),
        anchor: both.clone(),
    })
    .await
    else {
        panic!("no changes");
    };
    assert_eq!(deleted, [item_id(two_id, None)]);
    // What the system still asks of it: no such item (so it drops it), not
    // "unreachable" (which it asks again for ever).
    let asked = ask(Request::Item {
        domain: domain.clone(),
        id: item_id(two_id, None),
    })
    .await;
    assert!(
        matches!(
            &asked,
            Reply::Error {
                code: theca_fileprovider::protocol::Code::NoSuchItem,
                ..
            }
        ),
        "{asked:?}"
    );

    // An anchor this app never gave, or from past this state's: the system
    // lists again.
    for anchor in ["nonsense".to_owned(), format!("{one_id}:999999")] {
        let reply = ask(Request::Changes {
            domain: domain.clone(),
            anchor,
        })
        .await;
        assert!(
            matches!(
                &reply,
                Reply::Error {
                    code: theca_fileprovider::protocol::Code::SyncAnchorExpired,
                    ..
                }
            ),
            "{reply:?}"
        );
    }

    // Out of the state while this process still lists it (its engine hasn't
    // looked yet): its folder is gone, and the call goes through (it failed,
    // and the system never heard of the removal).
    state.remove_pair(&one).unwrap();
    let Reply::Changes {
        deleted, anchor, ..
    } = ask(Request::Changes {
        domain: domain.clone(),
        anchor: format!("{one_id}:0"),
    })
    .await
    else {
        panic!("no changes");
    };
    assert_eq!(deleted, [item_id(one_id, None)]);
    assert_eq!(anchor, "");
}
