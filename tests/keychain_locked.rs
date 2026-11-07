//! A keychain that can't be reached (locked, `theca run` over SSH; Secret
//! Service not running, a headless server), in a process of its own: its
//! store is the process's.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::*;
use keyring_core::api::CredentialStoreApi;
use keyring_core::{Entry, Error};
use theca::keychain;
use theca::state::Password;

/// A keychain refusing everything.
#[derive(Debug)]
struct Locked;

impl CredentialStoreApi for Locked {
    fn vendor(&self) -> String {
        "theca tests".into()
    }

    fn id(&self) -> String {
        "locked".into()
    }

    fn build(
        &self,
        _: &str,
        _: &str,
        _: Option<&HashMap<&str, &str>>,
    ) -> keyring_core::Result<Entry> {
        Err(Error::NoStorageAccess("the keychain is locked".into()))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// One test: the store is the process's. A migration and a login fall back
/// to the state, saying so; an account whose password is in the keychain
/// stops its engine on the error, is never signed out, and syncs again once
/// the keychain is back.
#[tokio::test(flavor = "multi_thread")]
async fn a_keychain_out_of_reach() {
    keyring_core::set_default_store(Arc::new(Locked));

    // 0.2.0's state, migrated with no keychain: its passwords stay in it.
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("state.sqlite");
    let release = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/upgrade/0.2.0");
    std::fs::copy(release.join("state.sqlite"), &old).unwrap();
    let migrated = theca::state::State::open(&old).unwrap();
    assert!(
        migrated
            .accounts()
            .unwrap()
            .iter()
            .all(|a| matches!(a.password, Password::InState(_)))
    );
    let said = migrated
        .notices()
        .unwrap()
        .iter()
        .filter(|n| n.message.contains("no keychain could be reached"))
        .count();
    assert_eq!(said, migrated.accounts().unwrap().len());

    let w = world().await;
    let mut a = w.device("a").await;
    let Password::InState(password) = a.account.password.clone() else {
        panic!("in a locked keychain");
    };
    assert!(
        a.notices()
            .iter()
            .any(|n| n.contains("no keychain could be reached")),
        "{:?}",
        a.notices()
    );

    a.stop().await;
    let state = a.state();
    let secret = keychain::new_secret();
    let mut account = a.account.clone();
    account.password = Password::Keychain(secret.clone());
    state.save_account(&account).unwrap();
    a.restart();
    eventually("the engine's error", || async {
        a.pair().error.is_some_and(|e| e.contains("keychain"))
    })
    .await;
    assert!(!state.accounts().unwrap()[0].signed_out);

    // Unlocked: the next try (a wake-up instead of 30 s) syncs.
    keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
    keychain::store(&secret, &password).unwrap();
    a.handle.wake();
    a.write("x.txt", "x");
    w.converge(&[&a]).await;
    assert_eq!(a.pair().error, None);
}
