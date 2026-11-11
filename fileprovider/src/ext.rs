//! The extension's Objective-C side: the principal class
//! `ThecaDriveExtension` (`NSFileProviderReplicatedExtension`, and Finder's
//! actions), its enumerator, its item class, and an observer for the
//! materialized items.
//! Each call is forwarded to the app (`ipc`) and its answer translated.
//!
//! Completion handlers are answered from a worker thread: the block is
//! copied and moved there (`Unsend`); File Provider's blocks may be called
//! from any thread.

use std::ffi::{CString, c_char, c_int};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::Mutex;
use std::sync::mpsc;

use block2::DynBlock;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, ClassType, DefinedClass, Message, define_class, msg_send, sel};
use objc2_file_provider::{
    NSFileProviderChangeObserver, NSFileProviderContentPolicy, NSFileProviderCreateItemOptions,
    NSFileProviderCustomAction, NSFileProviderDeleteItemOptions, NSFileProviderDomain,
    NSFileProviderEnumerating, NSFileProviderEnumerationObserver, NSFileProviderEnumerator,
    NSFileProviderErrorCode, NSFileProviderErrorDomain, NSFileProviderFileSystemFlags,
    NSFileProviderItem, NSFileProviderItemCapabilities, NSFileProviderItemFields,
    NSFileProviderItemProtocol, NSFileProviderItemVersion, NSFileProviderManager,
    NSFileProviderModifyItemOptions, NSFileProviderReplicatedExtension, NSFileProviderRequest,
    NSFileProviderWorkingSetContainerItemIdentifier,
};
use objc2_foundation::{
    NSArray, NSData, NSDate, NSDictionary, NSError, NSNumber, NSProgress, NSString, NSURL,
};
use objc2_uniform_type_identifiers::{UTType, UTTypeData, UTTypeFolder};

use crate::ipc::{self, log};
use crate::protocol::{Code, FREE, Item as Entry, KEEP, Reply, Request};

/// Lets a completion block cross to the worker thread.
struct Unsend<T>(T);
// SAFETY: only completion blocks, which File Provider lets be called on any
// thread.
unsafe impl<T> Send for Unsend<T> {}
impl<T> Unsend<T> {
    fn into_inner(self) -> T {
        self.0
    }
}

fn error(code: Code, why: &str) -> Retained<NSError> {
    let code = match code {
        Code::NoSuchItem => NSFileProviderErrorCode::NoSuchItem,
        Code::ServerUnreachable => NSFileProviderErrorCode::ServerUnreachable,
        Code::CannotSynchronize => NSFileProviderErrorCode::CannotSynchronize,
        Code::SyncAnchorExpired => NSFileProviderErrorCode::SyncAnchorExpired,
        Code::NotAuthenticated => NSFileProviderErrorCode::NotAuthenticated,
    };
    log(&format!("error {} ({why})", code.0));
    // SAFETY: an extern static of the framework, and a plain constructor.
    unsafe { NSError::errorWithDomain_code_userInfo(NSFileProviderErrorDomain, code.0, None) }
}

fn unexpected(reply: &Reply) -> (Code, String) {
    (
        Code::CannotSynchronize,
        format!("unexpected reply {reply:?}"),
    )
}

fn progress() -> Retained<NSProgress> {
    let progress = NSProgress::discreteProgressWithTotalUnitCount(1);
    progress.setCompletedUnitCount(1);
    progress
}

fn path_of(url: Option<&NSURL>) -> Option<PathBuf> {
    url.and_then(|u| u.to_file_path())
}

/// The anchor's bytes: the drive's counter, big-endian.
fn anchor_data(anchor: u64) -> Retained<NSData> {
    NSData::with_bytes(&anchor.to_be_bytes())
}

fn anchor_of(data: &NSData) -> Option<u64> {
    let bytes = data.to_vec();
    Some(u64::from_be_bytes(bytes.try_into().ok()?))
}

// ---------------------------------------------------------------------------
// Item

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "ThecaDriveItem"]
    #[ivars = Entry]
    struct Item;

    unsafe impl NSObjectProtocol for Item {}

    unsafe impl NSFileProviderItemProtocol for Item {
        #[unsafe(method_id(itemIdentifier))]
        fn item_identifier(&self) -> Retained<NSString> {
            NSString::from_str(&self.ivars().id)
        }

        #[unsafe(method_id(parentItemIdentifier))]
        fn parent_item_identifier(&self) -> Retained<NSString> {
            NSString::from_str(&self.ivars().parent)
        }

        #[unsafe(method_id(filename))]
        fn filename(&self) -> Retained<NSString> {
            NSString::from_str(&self.ivars().name)
        }

        #[unsafe(method_id(contentType))]
        fn content_type(&self) -> Retained<UTType> {
            let entry = self.ivars();
            let by_extension = (!entry.folder)
                .then(|| entry.name.rsplit_once('.'))
                .flatten()
                .and_then(|(_, ext)| UTType::typeWithFilenameExtension(&NSString::from_str(ext)));
            match by_extension {
                Some(t) => t,
                // SAFETY: extern statics of UniformTypeIdentifiers.
                None if entry.folder => unsafe { UTTypeFolder.retain() },
                None => unsafe { UTTypeData.retain() },
            }
        }

        #[unsafe(method(capabilities))]
        fn capabilities(&self) -> NSFileProviderItemCapabilities {
            let entry = self.ivars();
            let mut can = NSFileProviderItemCapabilities::AllowsReading;
            if entry.folder {
                can |= NSFileProviderItemCapabilities::AllowsContentEnumerating;
            }
            if entry.writable {
                can |= NSFileProviderItemCapabilities::AllowsWriting
                    | NSFileProviderItemCapabilities::AllowsRenaming
                    | NSFileProviderItemCapabilities::AllowsReparenting
                    | NSFileProviderItemCapabilities::AllowsDeleting;
            }
            can
        }

        #[unsafe(method(fileSystemFlags))]
        fn file_system_flags(&self) -> NSFileProviderFileSystemFlags {
            let entry = self.ivars();
            let mut flags = NSFileProviderFileSystemFlags::UserReadable;
            if entry.folder {
                flags |= NSFileProviderFileSystemFlags::UserExecutable;
            }
            if entry.writable {
                flags |= NSFileProviderFileSystemFlags::UserWritable;
            }
            flags
        }

        #[unsafe(method_id(documentSize))]
        fn document_size(&self) -> Option<Retained<NSNumber>> {
            (!self.ivars().folder).then(|| NSNumber::new_u64(self.ivars().size))
        }

        #[unsafe(method_id(contentModificationDate))]
        fn content_modification_date(&self) -> Option<Retained<NSDate>> {
            self.ivars()
                .modified
                .map(|t| NSDate::dateWithTimeIntervalSince1970(t as f64))
        }

        #[unsafe(method_id(creationDate))]
        fn creation_date(&self) -> Option<Retained<NSDate>> {
            self.ivars()
                .modified
                .map(|t| NSDate::dateWithTimeIntervalSince1970(t as f64))
        }

        #[unsafe(method_id(itemVersion))]
        fn item_version(&self) -> Retained<NSFileProviderItemVersion> {
            let entry = self.ivars();
            let content = NSData::with_bytes(entry.version.as_bytes());
            let metadata =
                NSData::with_bytes(format!("{}/{}", entry.parent, entry.name).as_bytes());
            // SAFETY: a plain initializer.
            unsafe {
                NSFileProviderItemVersion::initWithContentVersion_metadataVersion(
                    NSFileProviderItemVersion::alloc(),
                    &content,
                    &metadata,
                )
            }
        }

        /// Kept itself: downloaded before it's read, never evicted by the
        /// system; otherwise as its folder (the root: lazily), so a folder
        /// kept or not anymore changes what is below it at once.
        #[unsafe(method(contentPolicy))]
        fn content_policy(&self) -> NSFileProviderContentPolicy {
            if self.ivars().kept_itself {
                NSFileProviderContentPolicy::DownloadEagerlyAndKeepDownloaded
            } else {
                NSFileProviderContentPolicy::Inherited
            }
        }

        /// What Finder's actions are offered on (their rules in `Info.plist`).
        #[unsafe(method_id(userInfo))]
        fn user_info(&self) -> Option<Retained<NSDictionary>> {
            let entry = self.ivars();
            let keys = [
                &*NSString::from_str("kept"),
                &*NSString::from_str("keptItself"),
            ];
            let values = [
                &*NSNumber::new_bool(entry.kept),
                &*NSNumber::new_bool(entry.kept_itself),
            ];
            let info = NSDictionary::from_slices(&keys, &values);
            // SAFETY: NSDictionary's generics are Rust-only; the object is
            // the same.
            Some(unsafe { Retained::cast_unchecked(info) })
        }
    }
);

impl Item {
    fn new(entry: Entry) -> Retained<Self> {
        let this = Self::alloc().set_ivars(entry);
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }

    fn as_item(&self) -> *mut NSFileProviderItem {
        ptr::from_ref(ProtocolObject::<dyn NSFileProviderItemProtocol>::from_ref(
            self,
        ))
        .cast_mut()
    }
}

fn items(entries: Vec<Entry>) -> Retained<NSArray<NSFileProviderItem>> {
    let items: Vec<Retained<NSFileProviderItem>> = entries
        .into_iter()
        .map(|e| ProtocolObject::from_retained(Item::new(e)))
        .collect();
    NSArray::from_retained_slice(&items)
}

// ---------------------------------------------------------------------------
// Enumerator

struct Container {
    domain: String,
    id: String,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "ThecaDriveEnumerator"]
    #[ivars = Container]
    struct Enumerator;

    unsafe impl NSObjectProtocol for Enumerator {}

    unsafe impl NSFileProviderEnumerator for Enumerator {
        #[unsafe(method(invalidate))]
        fn invalidate(&self) {}

        #[unsafe(method(enumerateItemsForObserver:startingAtPage:))]
        fn enumerate_items(
            &self,
            observer: &ProtocolObject<dyn NSFileProviderEnumerationObserver>,
            _page: &NSData,
        ) {
            let c = self.ivars();
            let asked = Request::List {
                domain: c.domain.clone(),
                container: c.id.clone(),
            };
            match ipc::ask(&asked) {
                Ok(Reply::Items { items: list }) => {
                    // SAFETY: protocol calls with valid arguments.
                    unsafe {
                        observer.didEnumerateItems(&items(list));
                        observer.finishEnumeratingUpToPage(None);
                    }
                }
                other => {
                    let (code, why) = other.map_or_else(|e| e, |r| unexpected(&r));
                    // SAFETY: a protocol call with a valid error.
                    unsafe { observer.finishEnumeratingWithError(&error(code, &why)) };
                }
            }
        }

        #[unsafe(method(enumerateChangesForObserver:fromSyncAnchor:))]
        fn enumerate_changes(
            &self,
            observer: &ProtocolObject<dyn NSFileProviderChangeObserver>,
            anchor: &NSData,
        ) {
            let Some(from) = anchor_of(anchor) else {
                let e = error(
                    Code::SyncAnchorExpired,
                    "an anchor this extension didn't give",
                );
                // SAFETY: a protocol call with a valid error.
                unsafe { observer.finishEnumeratingWithError(&e) };
                return;
            };
            let asked = Request::Changes {
                domain: self.ivars().domain.clone(),
                anchor: from,
            };
            match ipc::ask(&asked) {
                Ok(Reply::Changes {
                    updated,
                    deleted,
                    anchor,
                }) => {
                    let gone: Vec<Retained<NSString>> =
                        deleted.iter().map(|id| NSString::from_str(id)).collect();
                    // SAFETY: protocol calls with valid arguments.
                    unsafe {
                        if !updated.is_empty() {
                            observer.didUpdateItems(&items(updated));
                        }
                        if !gone.is_empty() {
                            observer.didDeleteItemsWithIdentifiers(&NSArray::from_retained_slice(
                                &gone,
                            ));
                        }
                        observer.finishEnumeratingChangesUpToSyncAnchor_moreComing(
                            &anchor_data(anchor),
                            false,
                        );
                    }
                }
                other => {
                    let (code, why) = other.map_or_else(|e| e, |r| unexpected(&r));
                    // SAFETY: a protocol call with a valid error.
                    unsafe { observer.finishEnumeratingWithError(&error(code, &why)) };
                }
            }
        }

        #[unsafe(method(currentSyncAnchorWithCompletionHandler:))]
        fn current_sync_anchor(&self, done: &DynBlock<dyn Fn(*mut NSData)>) {
            let asked = Request::Anchor {
                domain: self.ivars().domain.clone(),
            };
            match ipc::ask(&asked) {
                Ok(Reply::Anchor { anchor }) => {
                    let data = anchor_data(anchor);
                    done.call((Retained::as_ptr(&data).cast_mut(),));
                }
                // None: the system tries again later.
                _ => done.call((ptr::null_mut(),)),
            }
        }
    }
);

impl Enumerator {
    fn new(domain: String, id: String) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Container { domain, id });
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }
}

// ---------------------------------------------------------------------------
// The materialized items, enumerated for the app (the drive limit)

/// The identifiers listed, or why not.
type Listed = Result<Vec<String>, String>;

struct Collected {
    ids: Mutex<Vec<String>>,
    done: Mutex<Option<mpsc::Sender<Listed>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "ThecaDriveMaterialized"]
    #[ivars = Collected]
    struct Materialized;

    unsafe impl NSObjectProtocol for Materialized {}

    unsafe impl NSFileProviderEnumerationObserver for Materialized {
        #[unsafe(method(didEnumerateItems:))]
        fn did_enumerate_items(&self, items: &NSArray<NSFileProviderItem>) {
            let mut ids = self.ivars().ids.lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: a required property of the item protocol.
            ids.extend(
                items
                    .iter()
                    .map(|i| unsafe { i.itemIdentifier() }.to_string()),
            );
        }

        #[unsafe(method(finishEnumeratingUpToPage:))]
        fn finish_up_to_page(&self, _page: Option<&NSData>) {
            let ids =
                std::mem::take(&mut *self.ivars().ids.lock().unwrap_or_else(|e| e.into_inner()));
            if let Some(done) = self
                .ivars()
                .done
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                let _ = done.send(Ok(ids));
            }
        }

        #[unsafe(method(finishEnumeratingWithError:))]
        fn finish_with_error(&self, error: &NSError) {
            if let Some(done) = self
                .ivars()
                .done
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                let _ = done.send(Err(error.localizedDescription().to_string()));
            }
        }
    }
);

// ---------------------------------------------------------------------------
// Principal class

struct Ivars {
    domain: Retained<NSFileProviderDomain>,
    id: String,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "ThecaDriveExtension"]
    #[ivars = Ivars]
    struct Extension;

    unsafe impl NSObjectProtocol for Extension {}

    unsafe impl NSFileProviderEnumerating for Extension {
        #[unsafe(method_id(enumeratorForContainerItemIdentifier:request:error:))]
        fn enumerator_for(
            &self,
            container: &NSString,
            _request: &NSFileProviderRequest,
            _error: *mut *mut NSError,
        ) -> Option<Retained<ProtocolObject<dyn NSFileProviderEnumerator>>> {
            Some(ProtocolObject::from_retained(Enumerator::new(
                self.ivars().id.clone(),
                container.to_string(),
            )))
        }
    }

    unsafe impl NSFileProviderReplicatedExtension for Extension {
        #[unsafe(method_id(initWithDomain:))]
        fn init_with_domain(
            this: Allocated<Self>,
            domain: &NSFileProviderDomain,
        ) -> Retained<Self> {
            // SAFETY: a property getter.
            let id = unsafe { domain.identifier() }.to_string();
            log(&format!("serving {id}"));
            let this = this.set_ivars(Ivars {
                domain: domain.retain(),
                id,
            });
            // SAFETY: NSObject's init.
            let this: Retained<Self> = unsafe { msg_send![super(this), init] };
            // Copies an extension killed mid-upload left.
            let staging = this.staging();
            std::thread::spawn(move || {
                crate::staged::sweep(&staging, crate::staged::STALE, std::time::SystemTime::now());
            });
            this
        }

        #[unsafe(method(invalidate))]
        fn invalidate(&self) {}

        #[unsafe(method_id(itemForIdentifier:request:completionHandler:))]
        fn item_for(
            &self,
            identifier: &NSString,
            _request: &NSFileProviderRequest,
            done: &DynBlock<dyn Fn(*mut NSFileProviderItem, *mut NSError)>,
        ) -> Retained<NSProgress> {
            let asked = Request::Item {
                domain: self.ivars().id.clone(),
                id: identifier.to_string(),
            };
            self.answer_item(asked, done);
            progress()
        }

        #[unsafe(method_id(fetchContentsForItemWithIdentifier:version:request:completionHandler:))]
        fn fetch_contents(
            &self,
            identifier: &NSString,
            _version: Option<&NSFileProviderItemVersion>,
            _request: &NSFileProviderRequest,
            done: &DynBlock<dyn Fn(*mut NSURL, *mut NSFileProviderItem, *mut NSError)>,
        ) -> Retained<NSProgress> {
            let id = identifier.to_string();
            let into = self.staging().join(format!("{id}-{}", unique()));
            let asked = Request::Fetch {
                domain: self.ivars().id.clone(),
                id,
                into: into.clone(),
            };
            let done = Unsend(done.copy());
            std::thread::spawn(move || {
                let done = done.into_inner();
                match ipc::ask(&asked) {
                    Ok(Reply::Item(entry)) => {
                        let url =
                            NSURL::fileURLWithPath(&NSString::from_str(&into.to_string_lossy()));
                        let item = Item::new(entry);
                        done.call((
                            Retained::as_ptr(&url).cast_mut(),
                            item.as_item(),
                            ptr::null_mut(),
                        ));
                    }
                    other => {
                        let (code, why) = other.map_or_else(|e| e, |r| unexpected(&r));
                        let e = error(code, &why);
                        done.call((
                            ptr::null_mut(),
                            ptr::null_mut(),
                            Retained::as_ptr(&e).cast_mut(),
                        ));
                    }
                }
            });
            progress()
        }

        #[unsafe(method_id(createItemBasedOnTemplate:fields:contents:options:request:completionHandler:))]
        fn create_item(
            &self,
            template: &NSFileProviderItem,
            _fields: NSFileProviderItemFields,
            contents: Option<&NSURL>,
            _options: NSFileProviderCreateItemOptions,
            _request: &NSFileProviderRequest,
            done: &DynBlock<
                dyn Fn(*mut NSFileProviderItem, NSFileProviderItemFields, Bool, *mut NSError),
            >,
        ) -> Retained<NSProgress> {
            // SAFETY: required properties of the item protocol; contentType
            // optional, checked.
            let (name, parent, folder) = unsafe {
                let folder = template.respondsToSelector(sel!(contentType))
                    && template
                        .contentType()
                        .conformsToType(&UTTypeFolder.retain());
                (
                    template.filename().to_string(),
                    template.parentItemIdentifier().to_string(),
                    folder,
                )
            };
            let staged = self.stage(contents);
            let asked = Request::Create {
                domain: self.ivars().id.clone(),
                parent,
                name,
                folder,
                contents: staged.as_ref().map(|(_, copy)| copy.clone()),
            };
            answer_changed(asked, staged, done);
            progress()
        }

        #[unsafe(method_id(modifyItem:baseVersion:changedFields:contents:options:request:completionHandler:))]
        fn modify_item(
            &self,
            item: &NSFileProviderItem,
            base: &NSFileProviderItemVersion,
            fields: NSFileProviderItemFields,
            contents: Option<&NSURL>,
            _options: NSFileProviderModifyItemOptions,
            _request: &NSFileProviderRequest,
            done: &DynBlock<
                dyn Fn(*mut NSFileProviderItem, NSFileProviderItemFields, Bool, *mut NSError),
            >,
        ) -> Retained<NSProgress> {
            let staged = if fields.contains(NSFileProviderItemFields::Contents) {
                self.stage(contents)
            } else {
                None
            };
            // SAFETY: required properties of the item protocol; lastUsedDate
            // optional, checked.
            let asked = unsafe {
                let used = (fields.contains(NSFileProviderItemFields::LastUsedDate)
                    && item.respondsToSelector(sel!(lastUsedDate)))
                .then(|| item.lastUsedDate())
                .flatten()
                .map(|d| d.timeIntervalSince1970() as i64);
                Request::Modify {
                    domain: self.ivars().id.clone(),
                    id: item.itemIdentifier().to_string(),
                    base: Some(
                        String::from_utf8_lossy(&base.contentVersion().to_vec()).into_owned(),
                    ),
                    name: fields
                        .contains(NSFileProviderItemFields::Filename)
                        .then(|| item.filename().to_string()),
                    parent: fields
                        .contains(NSFileProviderItemFields::ParentItemIdentifier)
                        .then(|| item.parentItemIdentifier().to_string()),
                    contents: staged.as_ref().map(|(_, copy)| copy.clone()),
                    used,
                }
            };
            answer_changed(asked, staged, done);
            progress()
        }

        #[unsafe(method_id(deleteItemWithIdentifier:baseVersion:options:request:completionHandler:))]
        fn delete_item(
            &self,
            identifier: &NSString,
            _base: &NSFileProviderItemVersion,
            _options: NSFileProviderDeleteItemOptions,
            _request: &NSFileProviderRequest,
            done: &DynBlock<dyn Fn(*mut NSError)>,
        ) -> Retained<NSProgress> {
            let asked = Request::Delete {
                domain: self.ivars().id.clone(),
                id: identifier.to_string(),
            };
            let done = Unsend(done.copy());
            std::thread::spawn(move || {
                let done = done.into_inner();
                match ipc::ask(&asked) {
                    Ok(Reply::Done) => done.call((ptr::null_mut(),)),
                    other => {
                        let (code, why) = other.map_or_else(|e| e, |r| unexpected(&r));
                        let e = error(code, &why);
                        done.call((Retained::as_ptr(&e).cast_mut(),));
                    }
                }
            });
            progress()
        }

        /// What is downloaded changed: the app is told all of it (the drive
        /// limit evicts from it).
        #[unsafe(method(materializedItemsDidChangeWithCompletionHandler:))]
        fn materialized_items_did_change(&self, done: &DynBlock<dyn Fn()>) {
            let (tx, rx) = mpsc::channel();
            let observer = Materialized::alloc().set_ivars(Collected {
                ids: Mutex::new(Vec::new()),
                done: Mutex::new(Some(tx)),
            });
            // SAFETY: NSObject's init.
            let observer: Retained<Materialized> = unsafe { msg_send![super(observer), init] };
            // SAFETY: a valid domain; the enumerator's own protocol call.
            unsafe {
                if let Some(manager) = NSFileProviderManager::managerForDomain(&self.ivars().domain)
                {
                    let enumerator = manager.enumeratorForMaterializedItems();
                    enumerator.enumerateItemsForObserver_startingAtPage(
                        ProtocolObject::from_ref(&*observer),
                        &NSData::new(),
                    );
                }
            }
            let domain = self.ivars().id.clone();
            let done = Unsend(done.copy());
            std::thread::spawn(move || {
                let done = done.into_inner();
                match rx.recv_timeout(std::time::Duration::from_secs(60)) {
                    Ok(Ok(ids)) => {
                        let _ = ipc::ask(&Request::Materialized { domain, ids });
                    }
                    Ok(Err(why)) => log(&format!("materialized items not listed: {why}")),
                    Err(_) => log("materialized items not listed in time"),
                }
                done.call(());
            });
        }

        #[unsafe(method(importDidFinishWithCompletionHandler:))]
        fn import_did_finish(&self, done: &DynBlock<dyn Fn()>) {
            done.call(());
        }
    }

    unsafe impl NSFileProviderCustomAction for Extension {
        /// Always Keep on This Mac, or Free Up Space: the app records it (and
        /// frees), then the system
        /// is signaled, reads the items again and applies their policy.
        #[unsafe(method_id(performActionWithIdentifier:onItemsWithIdentifiers:completionHandler:))]
        fn perform_action(
            &self,
            action: &NSString,
            items: &NSArray<NSString>,
            done: &DynBlock<dyn Fn(*mut NSError)>,
        ) -> Retained<NSProgress> {
            let domain = self.ivars().id.clone();
            let ids: Vec<String> = items.iter().map(|i| i.to_string()).collect();
            let asked = match action.to_string().as_str() {
                KEEP => Some(Request::Keep { domain, ids }),
                FREE => Some(Request::Free { domain, ids }),
                _ => None,
            };
            // SAFETY: a class method on a valid domain.
            let manager =
                Unsend(unsafe { NSFileProviderManager::managerForDomain(&self.ivars().domain) });
            let action = action.to_string();
            let done = Unsend(done.copy());
            std::thread::spawn(move || {
                let done = done.into_inner();
                let Some(asked) = asked else {
                    let e = error(Code::CannotSynchronize, &format!("no action {action}"));
                    done.call((Retained::as_ptr(&e).cast_mut(),));
                    return;
                };
                match ipc::ask(&asked) {
                    Ok(Reply::Done) => {
                        if let Some(manager) = manager.into_inner() {
                            let signaled = block2::RcBlock::new(|_: *mut NSError| {});
                            // SAFETY: an extern static and a valid block.
                            unsafe {
                                manager
                                    .signalEnumeratorForContainerItemIdentifier_completionHandler(
                                        NSFileProviderWorkingSetContainerItemIdentifier,
                                        &signaled,
                                    );
                            }
                        }
                        done.call((ptr::null_mut(),));
                    }
                    other => {
                        let (code, why) = other.map_or_else(|e| e, |r| unexpected(&r));
                        let e = error(code, &why);
                        done.call((Retained::as_ptr(&e).cast_mut(),));
                    }
                }
            });
            progress()
        }
    }
);

/// A call answered with an item.
fn answer_item(asked: Request, done: &DynBlock<dyn Fn(*mut NSFileProviderItem, *mut NSError)>) {
    let done = Unsend(done.copy());
    std::thread::spawn(move || {
        let done = done.into_inner();
        match ipc::ask(&asked) {
            Ok(Reply::Item(entry)) => done.call((Item::new(entry).as_item(), ptr::null_mut())),
            other => {
                let (code, why) = other.map_or_else(|e| e, |r| unexpected(&r));
                let e = error(code, &why);
                done.call((ptr::null_mut(), Retained::as_ptr(&e).cast_mut()));
            }
        }
    });
}

/// A local change answered with the item as it is now (no fields left to
/// apply, no content to fetch again). Its content (`staged`: the system's
/// file, and where it goes) copied first, removed after.
fn answer_changed(
    asked: Request,
    staged: Option<(PathBuf, PathBuf)>,
    done: &DynBlock<dyn Fn(*mut NSFileProviderItem, NSFileProviderItemFields, Bool, *mut NSError)>,
) {
    let done = Unsend(done.copy());
    std::thread::spawn(move || {
        let done = done.into_inner();
        let copied = match &staged {
            Some((from, to)) => std::fs::copy(from, to)
                .map(|_| ())
                .map_err(|e| (Code::CannotSynchronize, format!("not staged: {e}"))),
            None => Ok(()),
        };
        let answer = copied.and_then(|()| ipc::ask(&asked));
        if let Some((_, copy)) = &staged {
            let _ = std::fs::remove_file(copy);
        }
        match answer {
            Ok(Reply::Item(entry)) => done.call((
                Item::new(entry).as_item(),
                NSFileProviderItemFields::empty(),
                Bool::NO,
                ptr::null_mut(),
            )),
            other => {
                let (code, why) = other.map_or_else(|e| e, |r| unexpected(&r));
                let e = error(code, &why);
                done.call((
                    ptr::null_mut(),
                    NSFileProviderItemFields::empty(),
                    Bool::NO,
                    Retained::as_ptr(&e).cast_mut(),
                ));
            }
        }
    });
}

/// A name nobody else picks, for a staged fetch.
fn unique() -> String {
    use std::hash::BuildHasher;
    let random =
        std::collections::hash_map::RandomState::new().hash_one(std::time::SystemTime::now());
    format!("{random:016x}")
}

impl Extension {
    /// Where a local change's content is copied for the app: the system's
    /// file, in this sandbox, is one the app may not read (a developer's
    /// Mac refused it: "Operation not permitted"); the staging folder, where
    /// fetches land, it can.
    fn stage(&self, contents: Option<&NSURL>) -> Option<(PathBuf, PathBuf)> {
        let from = path_of(contents)?;
        Some((from, self.staging().join(format!("upload-{}", unique()))))
    }

    fn answer_item(
        &self,
        asked: Request,
        done: &DynBlock<dyn Fn(*mut NSFileProviderItem, *mut NSError)>,
    ) {
        answer_item(asked, done);
    }

    /// Where a fetch is staged: the domain's temporary folder, on the same
    /// volume, so the system moves it into place.
    fn staging(&self) -> PathBuf {
        // SAFETY: a class method and a getter, on a valid domain.
        let url = unsafe {
            NSFileProviderManager::managerForDomain(&self.ivars().domain)
                .and_then(|manager| manager.temporaryDirectoryURLWithError().ok())
        };
        url.and_then(|u| u.to_file_path())
            .unwrap_or_else(|| PathBuf::from(objc2_foundation::NSTemporaryDirectory().to_string()))
    }
}

// ---------------------------------------------------------------------------
// Registration and entry point

/// objc2 registers a `define_class!` class on its first use; PlugInKit looks
/// the principal class up by name before any Rust runs: registered at load.
#[used]
#[unsafe(link_section = "__DATA,__mod_init_func")]
static REGISTER_ON_LOAD: extern "C" fn() = register_on_load;

extern "C" fn register_on_load() {
    let _ = Extension::class();
    let _ = Enumerator::class();
    let _ = Item::class();
    let _ = Materialized::class();
}

#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {
    /// Foundation's entry point for extensions (exported, in no header).
    fn NSExtensionMain(argc: c_int, argv: *const *const c_char) -> c_int;
}

/// The entry when linked without `-e _NSExtensionMain`.
pub fn start() {
    register_on_load();
    log("main() reached: the -e _NSExtensionMain link flag didn't apply");
    let arguments: Vec<CString> = std::env::args_os()
        .filter_map(|a| CString::new(a.into_vec()).ok())
        .collect();
    let mut pointers: Vec<*const c_char> = arguments.iter().map(|a| a.as_ptr()).collect();
    pointers.push(ptr::null());
    // SAFETY: argv is a NULL-terminated array of C strings alive until the
    // call returns.
    let code = unsafe { NSExtensionMain(arguments.len() as c_int, pointers.as_ptr()) };
    std::process::exit(code);
}
