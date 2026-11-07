//! App passwords in the OS keychain (docs/decisions.md, Sync): the real one on
//! GitHub's macOS and Windows runners, the in-memory one elsewhere
//! (`common::keychain`).

mod common;

use common::*;
use theca::keychain::{self, KeychainError};
use theca::ops;
use theca::state::Password;

/// A login keeps its app password in the keychain, the engine reads it from
/// there, and removing the account deletes its entry.
#[tokio::test(flavor = "multi_thread")]
async fn the_password_lives_in_the_keychain() {
    let w = world().await;
    let a = w.device("a").await;
    let Password::Keychain(secret) = a.account.password.clone() else {
        panic!("kept in the state: {:?}", a.notices());
    };
    assert!(!keychain::read(&secret).unwrap().is_empty());
    a.write("x.txt", "x");
    w.converge(&[&a]).await;
    let state = a.state();
    ops::remove_account(&state, &a.account, false)
        .await
        .unwrap();
    assert!(matches!(
        keychain::read(&secret),
        Err(KeychainError::Missing)
    ));
}

/// `--no-keyring` keeps it in the state, and says nothing about a keychain.
#[tokio::test(flavor = "multi_thread")]
async fn without_the_keyring_it_stays_in_the_state() {
    let w = world().await;
    let a = w.device_without_keychain("a").await;
    assert!(matches!(a.account.password, Password::InState(_)));
    a.write("x.txt", "x");
    w.converge(&[&a]).await;
    assert!(
        !a.notices().iter().any(|n| n.contains("keychain")),
        "{:?}",
        a.notices()
    );
}

/// An entry gone from the keychain (deleted by hand, the state copied to
/// another computer) signs the account out, saying why.
#[tokio::test(flavor = "multi_thread")]
async fn a_password_missing_from_the_keychain_signs_out() {
    let w = world().await;
    let mut a = w.device("a").await;
    a.stop().await;
    let Password::Keychain(secret) = a.account.password.clone() else {
        panic!("kept in the state");
    };
    keychain::delete(&secret).unwrap();
    a.restart();
    eventually("signed out", || async {
        a.state().accounts().unwrap()[0].signed_out
    })
    .await;
    assert!(
        a.notices()
            .iter()
            .any(|n| n.contains("missing from the keychain")),
        "{:?}",
        a.notices()
    );
}
