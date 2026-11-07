//! App passwords in the OS keychain: macOS's login keychain, Windows'
//! Credential Manager, Secret Service on Linux (docs/decisions.md, Sync). One
//! entry per account, named by a random ID its row keeps (`Password::Keychain`),
//! under the app's identifier: two states on one computer never share one.

use std::sync::Arc;

use keyring_core::{CredentialStore, Entry, Error};

use crate::state::{Account, Password};

/// The app's identifier (`tauri.conf.json`): the entries' service.
const SERVICE: &str = "rs.theca.computer";

#[derive(Debug, thiserror::Error)]
pub enum KeychainError {
    /// Deleted by hand, or the state copied from another computer: signed out.
    #[error("the app password is missing from the keychain: log in again")]
    Missing,
    /// Locked, or not running yet (Secret Service at boot): tried again later.
    #[error("the keychain can't be reached: {0}")]
    Unreachable(String),
}

pub type Result<T> = std::result::Result<T, KeychainError>;

/// The account's app password, wherever it is.
pub fn password(account: &Account) -> Result<String> {
    match &account.password {
        Password::InState(password) => Ok(password.clone()),
        Password::Keychain(secret) => read(secret),
    }
}

/// A new entry's name.
pub fn new_secret() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn store(secret: &str, password: &str) -> Result<()> {
    entry(secret)?.set_password(password).map_err(error)
}

pub fn read(secret: &str) -> Result<String> {
    entry(secret)?.get_password().map_err(error)
}

/// Deletes an entry; one already gone is fine.
pub fn delete(secret: &str) -> Result<()> {
    match entry(secret)?.delete_credential() {
        Ok(()) | Err(Error::NoEntry) => Ok(()),
        Err(e) => Err(error(e)),
    }
}

/// An in-memory keychain for this process (tests): set before any entry is
/// used, it replaces the OS's.
pub fn use_mock() {
    if keyring_core::get_default_store().is_none() {
        keyring_core::set_default_store(keyring_core::mock::Store::new().expect("a mock store"));
    }
}

fn entry(secret: &str) -> Result<Entry> {
    // The OS's store, set once it can be reached: one not running yet (Secret
    // Service at boot) is tried again on the next call.
    if keyring_core::get_default_store().is_none() {
        keyring_core::set_default_store(native().map_err(error)?);
    }
    Entry::new(SERVICE, secret).map_err(error)
}

#[cfg(target_os = "macos")]
fn native() -> keyring_core::Result<Arc<CredentialStore>> {
    Ok(apple_native_keyring_store::keychain::Store::new()?)
}

#[cfg(windows)]
fn native() -> keyring_core::Result<Arc<CredentialStore>> {
    Ok(windows_native_keyring_store::Store::new()?)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn native() -> keyring_core::Result<Arc<CredentialStore>> {
    Ok(zbus_secret_service_keyring_store::Store::new()?)
}

fn error(e: Error) -> KeychainError {
    match e {
        Error::NoEntry => KeychainError::Missing,
        e => KeychainError::Unreachable(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The entries' service is the app's identifier, as the app is built, and
    /// the one 1.0.0 released: another would find none of the entries users
    /// have, and sign every account out.
    #[test]
    fn the_service_is_the_identifier() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(config["identifier"], SERVICE);
        assert_eq!(SERVICE, "rs.theca.computer");
    }

    /// Stored, read, deleted, and deleted again; a missing entry is told
    /// apart from a keychain that can't be reached.
    #[test]
    fn an_entry_round_trips() {
        use_mock();
        let secret = new_secret();
        assert!(matches!(read(&secret), Err(KeychainError::Missing)));
        store(&secret, "p").unwrap();
        assert_eq!(read(&secret).unwrap(), "p");
        delete(&secret).unwrap();
        delete(&secret).unwrap();
        assert!(matches!(read(&secret), Err(KeychainError::Missing)));
        let entry = entry(&secret).unwrap();
        let mock: &keyring_core::mock::Cred = entry.as_any().downcast_ref().unwrap();
        mock.set_error(Error::NoStorageAccess("locked".into()));
        assert!(matches!(
            entry.get_password().map_err(error),
            Err(KeychainError::Unreachable(_))
        ));
    }
}
