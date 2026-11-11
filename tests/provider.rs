//! macOS's drive, the app's side, on every OS: the extension's calls
//! (`protocol::Request`) answered as the socket answers them, against the
//! real server. A drive's entries come from the feed (here a listing,
//! applied as a macOS drive's round is).

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use common::*;
use theca::drive::provider::{Provider, apply_remote, domain_of, kept_files, still_kept};
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
    provider.serve(&pair, client, state.clone(), Default::default());
    let domain = domain_of(&pair);
    let ask = |r: Request| provider.answer(r);

    let Reply::Items { items: root } = ask(Request::List {
        domain: domain.clone(),
        container: ROOT.into(),
    })
    .await
    else {
        panic!("no listing");
    };
    assert_eq!(names(&root), ["a.txt", "d"].map(String::from).into());
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
        (ROOT.to_owned(), "not the drive itself"),
        (uuid::Uuid::now_v7().to_string(), "no item"),
    ] {
        let reply = keep(vec![a.id.clone(), refused.clone()], true).await;
        assert!(
            matches!(&reply, Reply::Error { message, .. } if message.contains(why)),
            "{refused}: {reply:?}"
        );
    }
    assert!(
        listed(ROOT.into())
            .await
            .iter()
            .all(|i| !i.kept && !i.kept_itself)
    );
    assert_eq!(keep(vec![folder.id.clone()], true).await, Reply::Done);
    let shown = listed(ROOT.into()).await;
    let d = shown.iter().find(|i| i.name == "d").unwrap();
    assert!(d.kept && d.kept_itself, "{d:?}");
    assert!(shown.iter().any(|i| i.name == "a.txt" && !i.kept));
    let below = listed(folder.id.clone()).await;
    assert!(below[0].kept && !below[0].kept_itself, "{below:?}");
    let Reply::Changes { updated, .. } = ask(Request::Changes {
        domain: domain.clone(),
        anchor,
    })
    .await
    else {
        panic!("no changes");
    };
    assert_eq!(names(&updated), ["d".to_owned()].into());
    // What the engine downloads then: the kept folder's file, by its path in
    // the drive; not once freed.
    let b: uuid::Uuid = below[0].id.parse().unwrap();
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
        parent: ROOT.into(),
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
        parent: Some(ROOT.into()),
        contents: None,
        used: Some(1_800_000_000),
    })
    .await;
    assert!(
        matches!(&renamed, Reply::Item(i) if i.name == "c.txt" && i.parent == ROOT),
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
        anchor,
    })
    .await
    else {
        panic!("no changes");
    };
    assert!(now > anchor);
    assert_eq!(
        names(&updated),
        ["a.txt", "c.txt", "d"].map(String::from).into()
    );
    assert_eq!(deleted, [new.id]);
}

fn read(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}
