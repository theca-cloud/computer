//! See Cargo.toml: what needs `unsafe` for Windows' service.

#![cfg(windows)]

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::{define_windows_service, service_dispatcher};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, GetLastError, HANDLE,
};
use windows_sys::Win32::Security::Authentication::Identity::{
    LSA_HANDLE, LSA_OBJECT_ATTRIBUTES, LSA_UNICODE_STRING, LsaAddAccountRights, LsaClose,
    LsaNtStatusToWinError, LsaOpenPolicy, POLICY_CREATE_ACCOUNT, POLICY_LOOKUP_NAMES,
};
use windows_sys::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, DuplicateToken, LOGON32_LOGON_SERVICE,
    LOGON32_PROVIDER_DEFAULT, LogonUserW, LookupAccountNameW, SID_NAME_USE, SecurityImpersonation,
    SidTypeUser, WinBuiltinAdministratorsSid,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{
    FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX64, FOLDERID_ProgramFilesX86, SHGetKnownFolderPath,
};
use windows_sys::core::{BOOL, GUID, PWSTR};

/// What the service manager asked of the service, for its body to follow, and
/// the way to tell it the service runs.
#[derive(Clone)]
pub struct Stop {
    requests: Arc<Mutex<Requests>>,
    status: Arc<OnceLock<ServiceStatusHandle>>,
}

#[derive(Default)]
struct Requests {
    stopped: bool,
    callbacks: Vec<Box<dyn FnOnce() + Send>>,
}

impl Stop {
    /// `then` runs once the service manager asks the service to stop (on a
    /// thread of its own; at once if it already did).
    pub fn on_stop(&self, then: impl FnOnce() + Send + 'static) {
        let mut requests = self.requests.lock().expect("stop requests");
        if requests.stopped {
            drop(requests);
            then();
        } else {
            requests.callbacks.push(Box::new(then));
        }
    }

    /// The body is up: the service is running. Until it says so the manager
    /// has the service starting, and a body that ends first (a state another
    /// process holds) is a service that failed to start, which `StartService`
    /// reports.
    pub fn ready(&self) {
        if let Some(status) = self.status.get() {
            let _ = report(status, ServiceState::Running, 0, 0);
        }
    }

    fn request(&self) {
        let callbacks = {
            let mut requests = self.requests.lock().expect("stop requests");
            requests.stopped = true;
            std::mem::take(&mut requests.callbacks)
        };
        for callback in callbacks {
            callback();
        }
    }
}

/// The body of the service: runs until the service ends, and returns the exit
/// code it reports (0: success; any other: a failure, which the service's
/// recovery actions restart).
type Body = Box<dyn FnOnce(Stop) -> u32 + Send>;

static BODY: Mutex<Option<Body>> = Mutex::new(None);
static NAME: OnceLock<String> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

/// Connects this process to the service manager as the service `name`, and
/// runs `body` as its work, until it returns. Fails at once when the process
/// was not started by the service manager.
pub fn run(name: &str, body: impl FnOnce(Stop) -> u32 + Send + 'static) -> io::Result<()> {
    let _ = NAME.set(name.to_owned());
    *BODY.lock().expect("service body") = Some(Box::new(body));
    service_dispatcher::start(name, ffi_service_main).map_err(|e| match e {
        windows_service::Error::Winapi(e) => e,
        e => io::Error::other(e),
    })
}

fn service_main(_arguments: Vec<OsString>) {
    let Some(body) = BODY.lock().expect("service body").take() else {
        return;
    };
    let stop = Stop {
        requests: Arc::default(),
        status: Arc::default(),
    };
    let registered = service_control_handler::register(NAME.get().map_or("theca", |n| n), {
        let stop = stop.clone();
        move |control| match control {
            ServiceControl::Stop => {
                if let Some(status) = stop.status.get() {
                    let _ = report(status, ServiceState::StopPending, 0, 1);
                }
                stop.request();
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    });
    let Ok(registered) = registered else { return };
    let _ = stop.status.set(registered);
    // Starting, until the body calls `ready`.
    let _ = report(&registered, ServiceState::StartPending, 0, 1);
    let code = body(stop);
    let _ = report(&registered, ServiceState::Stopped, code, 0);
}

/// Tells the service manager the service is in `state`; `code` is the exit
/// code it ends with (0: none).
fn report(
    handle: &ServiceStatusHandle,
    state: ServiceState,
    code: u32,
    checkpoint: u32,
) -> windows_service::Result<()> {
    handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == ServiceState::Running {
            ServiceControlAccept::STOP
        } else {
            ServiceControlAccept::empty()
        },
        exit_code: if code == 0 {
            ServiceExitCode::NO_ERROR
        } else {
            ServiceExitCode::ServiceSpecific(code)
        },
        checkpoint,
        wait_hint: if matches!(
            state,
            ServiceState::StopPending | ServiceState::StartPending
        ) {
            Duration::from_secs(30)
        } else {
            Duration::ZERO
        },
        process_id: None,
    })
}

/// Lets `account` (`DOMAIN\user`, or a local user's name) run services: the
/// "Log on as a service" right. `CreateService` doesn't grant it, and a
/// service started without it fails with error 1069 (assumed from Microsoft's
/// documentation, tried only on GitHub's Windows runner). Needs an
/// administrator. Only a user account gets it: a group's name (`Users`) would
/// grant it to everyone in it.
pub fn grant_service_logon(account: &str) -> io::Result<()> {
    let name: Vec<u16> = account.encode_utf16().chain(Some(0)).collect();
    let mut sid_len = 0u32;
    let mut domain_len = 0u32;
    let mut kind: SID_NAME_USE = 0;
    // SAFETY: the first call only measures: null buffers with zero lengths,
    // which it answers with ERROR_INSUFFICIENT_BUFFER and the sizes needed.
    unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut sid_len,
            std::ptr::null_mut(),
            &mut domain_len,
            &mut kind,
        );
        if GetLastError() != ERROR_INSUFFICIENT_BUFFER {
            return Err(io::Error::last_os_error());
        }
    }
    // A SID wants 4-byte alignment, which a `Vec<u8>` doesn't promise.
    let mut sid = vec![0u32; (sid_len as usize).div_ceil(4)];
    let mut domain = vec![0u16; domain_len as usize];
    // SAFETY: `sid` and `domain` hold at least the sizes the first call asked
    // for, and outlive the call; `name` is NUL-terminated.
    let found = unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            name.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut sid_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    };
    if found == 0 {
        return Err(io::Error::last_os_error());
    }
    if kind != SidTypeUser {
        return Err(io::Error::other(format!("{account} isn't a user account")));
    }

    let right: Vec<u16> = "SeServiceLogonRight".encode_utf16().collect();
    let length = u16::try_from(right.len() * 2).expect("a short constant");
    let right = LSA_UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: right.as_ptr().cast_mut(),
    };
    // SAFETY: a zeroed LSA_OBJECT_ATTRIBUTES is what LsaOpenPolicy documents
    // for the local policy; `policy` is closed below; `sid` is a valid SID
    // (just filled in) and `right` points into `right`'s buffer, alive here.
    unsafe {
        let attributes: LSA_OBJECT_ATTRIBUTES = std::mem::zeroed();
        let mut policy: LSA_HANDLE = 0;
        let status = LsaOpenPolicy(
            std::ptr::null(),
            &attributes,
            (POLICY_CREATE_ACCOUNT | POLICY_LOOKUP_NAMES) as u32,
            &mut policy,
        );
        if status != 0 {
            return Err(io::Error::from_raw_os_error(
                LsaNtStatusToWinError(status) as i32
            ));
        }
        let status = LsaAddAccountRights(policy, sid.as_mut_ptr().cast(), &right, 1);
        LsaClose(policy);
        if status != 0 {
            return Err(io::Error::from_raw_os_error(
                LsaNtStatusToWinError(status) as i32
            ));
        }
    }
    Ok(())
}

/// Whether `user` (of `domain`, `.` for this computer) is an administrator
/// *as the service run as them is*: the account is logged on as a service with
/// `password`, which gives the unfiltered token the service would have, and
/// that token is asked about the Administrators group (the well-known SID, so
/// no localized name). The account needs the right to log on as a service
/// first (`grant_service_logon`). A wrong password is error 1326.
pub fn is_administrator(user: &str, domain: &str, password: &str) -> io::Result<bool> {
    let wide = |text: &str| -> Vec<u16> { text.encode_utf16().chain(Some(0)).collect() };
    let (user, domain, password) = (wide(user), wide(domain), wide(password));
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the three strings are NUL-terminated and outlive the call;
    // `token` is the handle it fills in, closed below.
    let logged_on = unsafe {
        LogonUserW(
            user.as_ptr(),
            domain.as_ptr(),
            password.as_ptr(),
            LOGON32_LOGON_SERVICE,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )
    };
    let failed = io::Error::last_os_error();
    if logged_on == 0 {
        return Err(failed);
    }
    // CheckTokenMembership wants an impersonation token, not LogonUser's
    // primary one.
    let mut impersonation: HANDLE = std::ptr::null_mut();
    // SAFETY: `token` is the open handle above; `impersonation` is closed
    // below.
    let duplicated = unsafe { DuplicateToken(token, SecurityImpersonation, &mut impersonation) };
    let failed = io::Error::last_os_error();
    // SAFETY: `token` is open, and not used again.
    unsafe { CloseHandle(token) };
    if duplicated == 0 {
        return Err(failed);
    }
    // A SID wants 4-byte alignment: SECURITY_MAX_SID_SIZE is 68 bytes.
    let mut sid = [0u32; 17];
    let mut size = std::mem::size_of_val(&sid) as u32;
    let mut member: BOOL = 0;
    // SAFETY: `sid` holds `size` bytes; `impersonation` is open and closed
    // after the last use; `member` is the answer it writes.
    let (made, checked, failed) = unsafe {
        let made = CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            std::ptr::null_mut(),
            sid.as_mut_ptr().cast(),
            &mut size,
        );
        let checked = if made == 0 {
            0
        } else {
            CheckTokenMembership(impersonation, sid.as_mut_ptr().cast(), &mut member)
        };
        let failed = io::Error::last_os_error();
        CloseHandle(impersonation);
        (made, checked, failed)
    };
    if made == 0 || checked == 0 {
        return Err(failed);
    }
    Ok(member != 0)
}

/// The Program Files folders (64-bit, 32-bit, and the native one on a 64-bit
/// Windows), as the operating system has them (the registry's, which only
/// administrators write), never as the process's environment says: a program
/// of the account's own can set `ProgramFiles` in its user registry, and an
/// elevated console opened later would read it (as `windir` is used to get past
/// UAC).
pub fn program_files() -> Vec<PathBuf> {
    [
        &FOLDERID_ProgramFiles,
        &FOLDERID_ProgramFilesX86,
        &FOLDERID_ProgramFilesX64,
    ]
    .into_iter()
    .filter_map(known_folder)
    .collect()
}

/// The path of a known folder, or none (one this Windows hasn't: the native
/// folder of a 32-bit one).
fn known_folder(id: &GUID) -> Option<PathBuf> {
    let mut path: PWSTR = std::ptr::null_mut();
    // SAFETY: `id` is a known-folder ID; no flags and no token (the current
    // user's view, which Program Files doesn't depend on); `path` is what it
    // fills in, a NUL-terminated string it allocated, freed below.
    let result = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut path) };
    if result < 0 || path.is_null() {
        return None;
    }
    // SAFETY: `path` is the NUL-terminated UTF-16 string above, read up to its
    // end, then freed once (the caller's to free even on success).
    let folder = unsafe {
        let mut length = 0;
        while *path.add(length) != 0 {
            length += 1;
        }
        let folder = String::from_utf16_lossy(std::slice::from_raw_parts(path, length));
        CoTaskMemFree(path.cast());
        folder
    };
    Some(PathBuf::from(folder))
}
