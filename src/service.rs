//! `theca service on|off`: the Windows service that runs `theca run` as a
//! user, so a computer that restarts keeps syncing without anyone opening
//! Theca. One service per user, named `theca-<user>`, run as that user (their
//! profile, folders and keychain) with the password they give once: the
//! service manager keeps it, Theca never does. The service itself is
//! `theca run --service` (`theca-winservice`, which holds the `unsafe`).

use std::ffi::OsString;
use std::io::{BufRead, IsTerminal};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use windows_service::Error as ServiceError;
use windows_service::service::{
    Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl,
    ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState,
    ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_LOGON_FAILURE: i32 = 1326;
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
const ERROR_SERVICE_SPECIFIC_ERROR: i32 = 1066;
const ERROR_SERVICE_LOGON_FAILED: i32 = 1069;
const ERROR_SERVICE_EXISTS: i32 = 1073;

/// How long the service gets to start or to stop.
const WAIT: Duration = Duration::from_secs(30);

/// What it means that the service ended while it was starting.
const STOPPED_AS_IT_STARTED: &str =
    "the service stopped as it started: `theca logs` says why (the state may be held by the app)";

/// The service for `user`, and the account it runs as.
struct Target {
    /// `theca-<user>`.
    service: String,
    /// `DOMAIN\user` (`.\user` for a local user), as the service manager has it.
    account: String,
    /// The account as the right is looked up by: the bare name for a local user
    /// (the lookup's own handling of `.` isn't relied on).
    lookup: String,
    /// The account's domain (`.` for this computer).
    domain: String,
    user: String,
}

impl Target {
    /// `account`'s (`user` or `DOMAIN\user`), or the user running this.
    fn new(account: Option<&str>) -> Result<Self, String> {
        let (domain, user) = match account {
            Some(account) => match account.split_once('\\') {
                Some((domain, user)) => (domain.to_owned(), user.to_owned()),
                None => (".".to_owned(), account.to_owned()),
            },
            None => (
                std::env::var("USERDOMAIN").unwrap_or_else(|_| ".".into()),
                std::env::var("USERNAME")
                    .map_err(|_| "who runs this isn't known: give --account")?,
            ),
        };
        Ok(Self {
            service: format!("theca-{user}"),
            account: format!("{domain}\\{user}"),
            lookup: if domain == "." {
                user.clone()
            } else {
                format!("{domain}\\{user}")
            },
            domain,
            user,
        })
    }
}

/// What a failed call to the service manager means to the user.
fn explain(what: &str, error: ServiceError) -> String {
    match &error {
        ServiceError::Winapi(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
            format!("{what}: it needs an administrator: open the terminal as administrator")
        }
        _ => format!("{what}: {error}"),
    }
}

fn raw_error(error: &ServiceError) -> Option<i32> {
    match error {
        ServiceError::Winapi(e) => e.raw_os_error(),
        _ => None,
    }
}

/// The user's Windows password: asked at a prompt that doesn't show it, or
/// read as a line of standard input when it isn't a terminal (a script). Never
/// an argument or a variable: those are logged and shown in process lists.
fn password(target: &Target) -> Result<String, String> {
    let stdin = std::io::stdin();
    let read = if stdin.is_terminal() {
        rpassword::prompt_password(format!("Windows password of {}: ", target.account))
    } else {
        let mut line = String::new();
        stdin.lock().read_line(&mut line).map(|_| line)
    };
    let password = read.map_err(|e| format!("the password can't be read: {e}"))?;
    Ok(password.trim_end_matches(['\r', '\n']).to_owned())
}

/// `theca service on`: the service registered (or, when it already is, stopped
/// and its password and path changed), set to restart after a failure, and
/// started. `state`: the `--state` it runs on, when the user gave one.
pub fn on(account: Option<&str>, state: Option<PathBuf>) -> Result<String, String> {
    let target = Target::new(account)?;
    // Before the password is asked: a user who isn't an administrator is told
    // first.
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|e| explain("the service manager can't be opened", e))?;
    let password = password(&target)?;
    theca_winservice::grant_service_logon(&target.lookup).map_err(|e| {
        format!(
            "{} can't be allowed to log on as a service: {e}",
            target.account
        )
    })?;
    // The service runs as this account, with the token the account is given
    // as a service: an administrator's is the full administrator token, so the
    // binary it runs must be one only administrators can replace (any
    // program of the account's could otherwise replace it and be started with
    // those rights at the next start). Logging the account on also tells a
    // wrong password at once.
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let administrator = theca_winservice::is_administrator(&target.user, &target.domain, &password)
        .map_err(|e| match e.raw_os_error() {
            Some(ERROR_LOGON_FAILURE) => {
                format!("the password is wrong for {}", target.account)
            }
            _ => format!("{} can't be logged on: {e}", target.account),
        })?;
    if administrator && !protected(&exe) {
        return Err(format!(
            "{} is an administrator, and the service would run {}, which that account can overwrite: any program it runs could replace it and be started with the account's full rights. Copy theca.exe to C:\\Program Files\\Theca\\ and run `theca service on` from there.",
            target.account,
            exe.display()
        ));
    }

    let mut arguments: Vec<OsString> = Vec::new();
    if let Some(state) = state {
        // The service's working directory isn't this one: a relative path
        // would be read from there.
        let state = std::path::absolute(&state)
            .map_err(|e| format!("the state file {} can't be resolved: {e}", state.display()))?;
        arguments.extend(["--state".into(), state.into_os_string()]);
    }
    arguments.extend(["run".into(), "--service".into()]);
    let info = ServiceInfo {
        name: target.service.clone().into(),
        display_name: format!("Theca ({})", target.user).into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: arguments,
        dependencies: vec![],
        account_name: Some(target.account.clone().into()),
        account_password: Some(password.into()),
    };
    let access = ServiceAccess::QUERY_STATUS
        | ServiceAccess::QUERY_CONFIG
        | ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::START
        | ServiceAccess::STOP
        | ServiceAccess::DELETE;
    let (service, created) = match manager.create_service(&info, access) {
        Ok(service) => (service, true),
        // Registered already: its configuration (the password, the path to
        // this binary) is made this one's, which a running service takes at its
        // next start: it is stopped first.
        Err(e) if raw_error(&e) == Some(ERROR_SERVICE_EXISTS) => {
            let service = manager
                .open_service(&target.service, access)
                .map_err(|e| explain("the service can't be opened", e))?;
            stop(&service)?;
            service
                .change_config(&info)
                .map_err(|e| explain("the service can't be changed", e))?;
            (service, false)
        }
        Err(e) => return Err(explain("the service can't be created", e)),
    };

    // A failure to stop as asked (an update installed, a state held by another
    // process) is a failure to the manager: it starts the service again.
    let actions = ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(5),
            };
            3
        ]),
    };
    let configured = service
        .update_failure_actions(actions)
        .and_then(|()| service.set_failure_actions_on_non_crash_failures(true));
    let started = configured
        .and_then(|()| service.start::<OsString>(&[]))
        .map_err(|e| match raw_error(&e) {
            Some(ERROR_SERVICE_LOGON_FAILED) => format!(
                "the service can't start as {}: the password is wrong, or the account may not log on as a service",
                target.account
            ),
            // The service answered the manager, then ended before it ran.
            Some(ERROR_SERVICE_SPECIFIC_ERROR) => STOPPED_AS_IT_STARTED.to_owned(),
            _ => explain("the service can't start", e),
        })
        .and_then(|()| wait_for(&service, ServiceState::Running));
    if let Err(why) = started {
        // A service that never ran isn't left behind; one that was there
        // before is.
        if created {
            let _ = service.delete();
        }
        return Err(why);
    }
    Ok(format!(
        "The service {} runs `theca run` as {}, and starts with Windows. `theca service off` removes it.",
        target.service, target.account
    ))
}

/// Whether `exe` is where only administrators write: under Program Files, as
/// the operating system has it (`theca_winservice::program_files`: never the
/// environment, which the account's own programs can set). Not the Windows
/// folder: `Temp`, `Tasks` and others under it are folders users create files
/// in.
fn protected(exe: &Path) -> bool {
    let roots: Vec<String> = theca_winservice::program_files()
        .iter()
        .map(|folder| folder.to_string_lossy().into_owned())
        .collect();
    under(exe, &roots)
}

/// `exe` is inside one of `roots` (a folder, compared by whole components and
/// without regard to case: `C:\Program Files Evil` isn't in `C:\Program
/// Files`). A path with `..` in it is in none, and no roots protect nothing.
fn under(exe: &Path, roots: &[String]) -> bool {
    if exe.components().any(|c| c == Component::ParentDir) {
        return false;
    }
    let exe = normal(&exe.to_string_lossy());
    roots.iter().any(|root| {
        let root = normal(root);
        !root.is_empty() && exe.starts_with(&format!("{root}\\"))
    })
}

/// A path as compared: no `\\?\`, one kind of separator, no trailing one,
/// lower case.
fn normal(path: &str) -> String {
    path.trim_start_matches(r"\\?\")
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// `theca service off`: the service stopped and removed.
pub fn off(account: Option<&str>) -> Result<String, String> {
    let target = Target::new(account)?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| explain("the service manager can't be opened", e))?;
    let service = match manager.open_service(
        &target.service,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    ) {
        Ok(service) => service,
        Err(e) if raw_error(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
            return Ok(format!("The service {} isn't there.", target.service));
        }
        Err(e) => return Err(explain("the service can't be opened", e)),
    };
    stop(&service)?;
    service
        .delete()
        .map_err(|e| explain("the service can't be removed", e))?;
    Ok(format!("The service {} is removed.", target.service))
}

/// `service` stopped, if it runs: waited for.
fn stop(service: &Service) -> Result<(), String> {
    if service
        .query_status()
        .is_ok_and(|s| s.current_state != ServiceState::Stopped)
    {
        // Already stopping is no failure: waited for below.
        let _ = service.stop();
        wait_for(service, ServiceState::Stopped)?;
    }
    Ok(())
}

/// Waits until `service` is in `state` (a service that stopped while it was to
/// run has failed to start).
fn wait_for(service: &Service, state: ServiceState) -> Result<(), String> {
    let deadline = Instant::now() + WAIT;
    loop {
        let status = service
            .query_status()
            .map_err(|e| explain("the service's state can't be read", e))?;
        if status.current_state == state {
            return Ok(());
        }
        if state == ServiceState::Running && status.current_state == ServiceState::Stopped {
            return Err(STOPPED_AS_IT_STARTED.into());
        }
        if Instant::now() > deadline {
            return Err(format!("the service isn't {state:?} after {WAIT:?}"));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A service manager that refuses is an administrator to ask for, not a
    /// bare code.
    #[test]
    fn a_denied_call_asks_for_an_administrator() {
        let denied = ServiceError::Winapi(std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED));
        let said = explain("the service can't be created", denied);
        assert!(said.contains("administrator"), "{said}");
        let other = ServiceError::Winapi(std::io::Error::from_raw_os_error(2));
        assert!(!explain("x", other).contains("administrator"));
    }

    /// Where the binary may be for an administrator's service: whole folders
    /// of the roots given (Program Files), in any case and with either
    /// separator, and nowhere a `..` could lead out of them.
    #[test]
    fn only_program_files_is_protected() {
        let roots = [
            "C:\\Program Files".to_owned(),
            "C:\\Program Files (x86)\\".to_owned(),
        ];
        let under = |path: &str| under(Path::new(path), &roots);
        assert!(under("C:\\Program Files\\Theca\\theca.exe"));
        assert!(under("c:\\program files (x86)\\Theca\\theca.exe"));
        assert!(under(r"\\?\C:\Program Files\Theca\theca.exe"));
        // Either separator, and a root with a trailing one.
        assert!(under("C:/Program Files/Theca/theca.exe"));
        assert!(under("C:\\Program Files (x86)\\Theca\\theca.exe"));
        // A name that only starts like a root, a profile, a way out, the
        // Windows folder (users create files in parts of it).
        assert!(!under("C:\\Program Files Evil\\theca.exe"));
        assert!(!under(
            "C:\\Users\\a\\AppData\\Local\\Programs\\Theca\\theca.exe"
        ));
        assert!(!under("C:\\ProgramData\\Theca\\theca.exe"));
        assert!(!under("C:\\Program Files\\..\\Users\\a\\theca.exe"));
        assert!(!under("C:\\Windows\\Temp\\theca.exe"));
        // Nothing to compare with: nothing is protected.
        assert!(!super::under(
            Path::new("C:\\Program Files\\Theca\\theca.exe"),
            &[]
        ));
    }

    /// The service is named for the user, and runs as their account: `user`
    /// is the local account, `DOMAIN\user` the domain's.
    #[test]
    fn the_service_is_named_for_its_user() {
        let local = Target::new(Some("alice")).unwrap();
        assert_eq!(local.service, "theca-alice");
        // The right is looked up by the bare name of a local user.
        assert_eq!(local.lookup, "alice");
        assert_eq!(local.account, ".\\alice");
        let domain = Target::new(Some("CORP\\bob")).unwrap();
        assert_eq!(domain.service, "theca-bob");
        assert_eq!(domain.account, "CORP\\bob");
        assert_eq!(domain.lookup, "CORP\\bob");
    }
}
