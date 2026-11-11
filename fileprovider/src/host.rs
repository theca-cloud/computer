//! The app's File Provider calls, safe: a domain (a drive) added, removed,
//! looked up, its changes signaled, an item evicted. Each waits for its
//! completion handler.

use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2_file_provider::{
    NSFileProviderDomain, NSFileProviderDomainRemovalMode, NSFileProviderErrorCode,
    NSFileProviderErrorDomain, NSFileProviderManager,
    NSFileProviderWorkingSetContainerItemIdentifier,
};
use objc2_foundation::{NSArray, NSError, NSString, NSURL};

use crate::protocol::{EXTENSION, ROOT};

const WAIT: Duration = Duration::from_secs(60);

/// The extension's container, where the app creates the socket.
pub fn container() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join("Library/Containers")
            .join(EXTENSION)
            .join("Data"),
    )
}

fn describe(error: *mut NSError) -> Option<String> {
    // SAFETY: the system hands either nil or a valid NSError.
    let error = unsafe { error.as_ref() }?;
    Some(format!(
        "{} ({} {})",
        error.localizedDescription(),
        error.domain(),
        error.code()
    ))
}

fn domain(id: &str, name: &str) -> Retained<NSFileProviderDomain> {
    // SAFETY: a plain initializer.
    unsafe {
        NSFileProviderDomain::initWithIdentifier_displayName(
            NSFileProviderDomain::alloc(),
            &NSString::from_str(id),
            &NSString::from_str(name),
        )
    }
}

/// The File Provider's error code of `error`, if it is one of its own.
fn code(error: *mut NSError) -> Option<isize> {
    // SAFETY: nil or a valid NSError; an extern static of the framework.
    let error = unsafe { error.as_ref() }?;
    (error.domain().to_string() == unsafe { NSFileProviderErrorDomain }.to_string())
        .then(|| error.code())
}

/// Runs a call taking an error completion handler, and waits for it.
fn wait(call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> Result<(), String> {
    wait_coded(call).map_err(|(why, _)| why)
}

/// `wait`, with the File Provider's error code when it gave one.
fn wait_coded(
    call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>),
) -> Result<(), (String, Option<isize>)> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |error: *mut NSError| {
        let _ = tx.send(describe(error).map(|why| (why, code(error))));
    });
    call(&block);
    match rx.recv_timeout(WAIT) {
        Ok(None) => Ok(()),
        Ok(Some(error)) => Err(error),
        Err(_) => Err(("no answer from the system".into(), None)),
    }
}

/// The drive added (its name shown in Finder's sidebar): macOS adds it
/// switched off, the user switches it on (System Settings).
pub fn add(id: &str, name: &str) -> Result<(), String> {
    let domain = domain(id, name);
    // SAFETY: a valid domain and block.
    wait(|block| unsafe { NSFileProviderManager::addDomain_completionHandler(&domain, block) })
}

/// The drive removed: macOS deletes its folder, but for what wasn't
/// uploaded yet, which it moves to the folder returned (none: nothing was
/// left).
pub fn remove(id: &str) -> Result<Option<PathBuf>, String> {
    let domain = domain(id, id);
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |url: *mut NSURL, error: *mut NSError| {
        let result = match describe(error) {
            Some(error) => Err(error),
            // SAFETY: nil or a valid URL.
            None => Ok(unsafe { url.as_ref() }.and_then(|u| u.to_file_path())),
        };
        let _ = tx.send(result);
    });
    // SAFETY: a valid domain and block.
    unsafe {
        NSFileProviderManager::removeDomain_mode_completionHandler(
            &domain,
            NSFileProviderDomainRemovalMode::PreserveDirtyUserData,
            &block,
        )
    };
    rx.recv_timeout(WAIT)
        .map_err(|_| "no answer from the system".to_owned())?
}

/// The drive as macOS has it: none (removed), switched on or not.
pub fn enabled(id: &str) -> Result<Option<bool>, String> {
    let (tx, rx) = mpsc::channel();
    let wanted = id.to_owned();
    let block = RcBlock::new(
        move |list: NonNull<NSArray<NSFileProviderDomain>>, error: *mut NSError| {
            if let Some(error) = describe(error) {
                let _ = tx.send(Err(error));
                return;
            }
            // SAFETY: the system hands a valid array.
            let list = unsafe { list.as_ref() };
            let found = list
                .iter()
                // SAFETY: property getters.
                .find(|d| unsafe { d.identifier() }.to_string() == wanted)
                .map(|d| unsafe { d.userEnabled() });
            let _ = tx.send(Ok(found));
        },
    );
    // SAFETY: a valid block.
    unsafe { NSFileProviderManager::getDomainsWithCompletionHandler(&block) };
    rx.recv_timeout(WAIT)
        .map_err(|_| "no answer from the system".to_owned())?
}

fn manager(id: &str) -> Result<Retained<NSFileProviderManager>, String> {
    // SAFETY: a valid domain.
    unsafe { NSFileProviderManager::managerForDomain(&domain(id, id)) }
        .ok_or_else(|| format!("no drive {id} on this Mac"))
}

/// Changes to show: the system asks the extension for them.
pub fn signal(id: &str) -> Result<(), String> {
    let manager = manager(id)?;
    // SAFETY: an extern static and a valid block.
    wait(|block| unsafe {
        manager.signalEnumeratorForContainerItemIdentifier_completionHandler(
            NSFileProviderWorkingSetContainerItemIdentifier,
            block,
        )
    })
}

/// An eviction refused: why, and whether the system holds the item as kept
/// downloaded (as it last read it: told otherwise, it may still).
pub struct Refused {
    pub why: String,
    pub kept: bool,
}

/// An item's content dropped (it stays listed); refused while it is open,
/// edited, or kept downloaded.
pub fn evict(id: &str, item: &str) -> Result<(), Refused> {
    let manager = manager(id).map_err(|why| Refused { why, kept: false })?;
    let item = NSString::from_str(item);
    // SAFETY: a valid identifier and block.
    wait_coded(|block| unsafe { manager.evictItemWithIdentifier_completionHandler(&item, block) })
        .map_err(|(why, code)| Refused {
            why,
            kept: code == Some(NSFileProviderErrorCode::NonEvictable.0),
        })
}

/// The drive's folder, where the system put it
/// (`~/Library/CloudStorage/Theca-<name>`).
pub fn folder(id: &str) -> Result<PathBuf, String> {
    let manager = manager(id)?;
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |url: *mut NSURL, error: *mut NSError| {
        // SAFETY: nil or a valid URL.
        let path = unsafe { url.as_ref() }.and_then(|u| u.to_file_path());
        let _ = tx.send(path.ok_or_else(|| describe(error).unwrap_or_default()));
    });
    // SAFETY: a valid identifier and block.
    unsafe {
        manager
            .getUserVisibleURLForItemIdentifier_completionHandler(&NSString::from_str(ROOT), &block)
    };
    rx.recv_timeout(WAIT)
        .map_err(|_| "no answer from the system".to_owned())?
}
