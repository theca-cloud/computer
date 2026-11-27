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
    NSFileProviderErrorDomain, NSFileProviderManager, NSFileProviderManagerDisconnectionOptions,
    NSFileProviderWorkingSetContainerItemIdentifier,
};
use objc2_foundation::{NSArray, NSError, NSFileManager, NSString, NSURL};

use crate::protocol::{ROOT, group};

const WAIT: Duration = Duration::from_secs(60);

/// The app group's folder (`~/Library/Group Containers/<group>`), where the
/// app creates the socket and both keep what they hand each other: the same
/// call in the app and in the sandboxed extension. None when the process
/// lacks the group's entitlement.
pub fn group_folder() -> Option<PathBuf> {
    NSFileManager::defaultManager()
        .containerURLForSecurityApplicationGroupIdentifier(&NSString::from_str(&group()))?
        .to_file_path()
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
    wait_within(WAIT, call)
}

/// A reply block's type, `void (^)(NSError *)`, which the block carries:
/// without it (`RcBlock::new` gives none) a call the system forwards over XPC
/// ends in "a NULL reply block was passed" (`caught_up`, on a runner).
struct ErrorReply;

// SAFETY: the encoding is that of `void (^)(NSError *)` on 64 bits (the
// only Macs we build for).
unsafe impl block2::ManualBlockEncoding for ErrorReply {
    type Arguments = (*mut NSError,);
    type Return = ();
    const ENCODING_CSTR: &'static std::ffi::CStr = c"v16@?0@\"NSError\"8";
}

/// `wait_coded`, for `limit` at most.
fn wait_within(
    limit: Duration,
    call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>),
) -> Result<(), (String, Option<isize>)> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::with_encoding::<_, _, _, ErrorReply>(move |error: *mut NSError| {
        let _ = tx.send(describe(error).map(|why| (why, code(error))));
    });
    call(&block);
    match rx.recv_timeout(limit) {
        Ok(None) => Ok(()),
        Ok(Some(error)) => Err(error),
        Err(_) => {
            // The system may still answer, to a block it holds: one freed
            // by now raises an exception that ends this process (found with
            // `caught_up`, on a runner).
            std::mem::forget(block);
            Err(("no answer from the system".into(), None))
        }
    }
}

/// The drive added (its name shown in Finder's sidebar): macOS adds it
/// switched off, the user switches it on (System Settings).
pub fn add(id: &str, name: &str) -> Result<(), String> {
    let domain = domain(id, name);
    // No trash of its own: a delete goes to the server's.
    // Left on, the system asks the extension for one again and again.
    // SAFETY: a property setter on a fresh domain.
    unsafe { domain.setSupportsSyncingTrash(false) };
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

/// A drive as macOS lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Domain {
    pub id: String,
    /// Its name in Finder (its folder's: `Theca-<name>`).
    pub name: String,
    /// Switched on in System Settings.
    pub enabled: bool,
    /// Present, but cut from its extension (`disconnect`): browsable, and
    /// what isn't downloaded doesn't open.
    pub disconnected: bool,
}

/// Every drive of this app macOS has, whichever state added it.
pub fn domains() -> Result<Vec<Domain>, String> {
    let (tx, rx) = mpsc::channel();
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
                .map(|d| unsafe {
                    Domain {
                        id: d.identifier().to_string(),
                        name: d.displayName().to_string(),
                        enabled: d.userEnabled(),
                        disconnected: d.isDisconnected(),
                    }
                })
                .collect();
            let _ = tx.send(Ok(found));
        },
    );
    // SAFETY: a valid block.
    unsafe { NSFileProviderManager::getDomainsWithCompletionHandler(&block) };
    rx.recv_timeout(WAIT)
        .map_err(|_| "no answer from the system".to_owned())?
}

/// The drive as macOS has it: none (removed), switched on or not.
pub fn enabled(id: &str) -> Result<Option<bool>, String> {
    Ok(domains()?
        .into_iter()
        .find(|d| d.id == id)
        .map(|d| d.enabled))
}

/// How long the system gets to answer a disconnect or a reconnect: a quit
/// doesn't wait for a minute.
const CONNECTING: Duration = Duration::from_secs(10);

/// The domain's manager, built with the display name macOS lists it with: one
/// made as `manager` does (the id as the name) renamed the drive's folder
/// `Theca-<id>` for good when it disconnected it, and the name stayed after
/// the reconnect (found on a runner, theca/computer#120).
fn listed_manager(id: &str) -> Result<Retained<NSFileProviderManager>, String> {
    let name = domains()?
        .into_iter()
        .find(|d| d.id == id)
        .ok_or_else(|| format!("no drive {id} on this Mac"))?
        .name;
    // SAFETY: a valid domain.
    unsafe { NSFileProviderManager::managerForDomain(&domain(id, &name)) }
        .ok_or_else(|| format!("no drive {id} on this Mac"))
}

/// The drive cut from its extension, saying why in Finder: it stays listed
/// and browsable, a file not downloaded fails to open at once, and the
/// extension isn't asked (nor retried for as long as the app is away). Holds
/// after this process ends, until `reconnect`.
pub fn disconnect(id: &str, reason: &str) -> Result<(), String> {
    let manager = listed_manager(id)?;
    let reason = NSString::from_str(reason);
    // SAFETY: a valid reason, options and block.
    wait_within(CONNECTING, |block| unsafe {
        manager.disconnectWithReason_options_completionHandler(
            &reason,
            NSFileProviderManagerDisconnectionOptions::empty(),
            block,
        )
    })
    .map_err(|(why, _)| why)
}

/// The drive served by its extension again.
pub fn reconnect(id: &str) -> Result<(), String> {
    let manager = listed_manager(id)?;
    // SAFETY: a valid block.
    wait_within(CONNECTING, |block| unsafe {
        manager.reconnectWithCompletionHandler(block)
    })
    .map_err(|(why, _)| why)
}

fn manager(id: &str) -> Result<Retained<NSFileProviderManager>, String> {
    // SAFETY: a valid domain.
    unsafe { NSFileProviderManager::managerForDomain(&domain(id, id)) }
        .ok_or_else(|| format!("no drive {id} on this Mac"))
}

/// What the system holds below `item` (a drive's folder) caught up with: its
/// local changes handed to the extension and answered. Waited for `within`
/// at most; why not, otherwise (offline, an upload refused). That it waits
/// for the uploads still queued is assumed from Apple's documentation.
pub fn caught_up(id: &str, item: &str, within: Duration) -> Result<(), String> {
    let manager = manager(id)?;
    let item = NSString::from_str(item);
    // SAFETY: a valid identifier and block.
    wait_within(within, |block| unsafe {
        manager.waitForChangesOnItemsBelowItemWithIdentifier_completionHandler(&item, block)
    })
    .map_err(|(why, _)| why)
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

// sys/resource.h (not in the libc crate).
const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: libc::c_int = 3;
const IOPOL_SCOPE_PROCESS: libc::c_int = 0;
const IOPOL_SCOPE_THREAD: libc::c_int = 1;
const IOPOL_MATERIALIZE_DATALESS_FILES_ON: libc::c_int = 2;

fn downloads_on(scope: libc::c_int) {
    unsafe extern "C" {
        fn setiopolicy_np(
            iotype: libc::c_int,
            scope: libc::c_int,
            policy: libc::c_int,
        ) -> libc::c_int;
    }
    // SAFETY: a plain system call.
    unsafe {
        setiopolicy_np(
            IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
            scope,
            IOPOL_MATERIALIZE_DATALESS_FILES_ON,
        )
    };
}

/// Downloads on for this whole process and the ones it starts: a service's
/// (a CI runner's) refuses to read a drive's folders and files that aren't
/// here (`EDEADLK`). For a test reading the domain as files.
pub fn downloads_on_in_process() {
    downloads_on(IOPOL_SCOPE_PROCESS);
}

/// A drive's file downloaded now, by reading its first byte on this thread
/// with downloads on (a thread may have them off: a service's): what
/// keeping a file means. Asked through the File Provider instead, macOS
/// queues it as a background task, which may wait (sprint 18, on a laptop).
pub fn materialize(path: &std::path::Path) -> Result<(), String> {
    use std::io::Read;
    downloads_on(IOPOL_SCOPE_THREAD);
    let mut byte = [0; 1];
    std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut byte))
        .map(|_| ())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// A drive's folder listed, with downloads on (a thread may have them off: a
/// service's): the system reads it into its index, which is what the first
/// visit to a folder waits for (theca/computer#124). How many it holds.
pub fn list_dir(path: &std::path::Path) -> std::io::Result<usize> {
    downloads_on(IOPOL_SCOPE_THREAD);
    Ok(std::fs::read_dir(path)?.count())
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
