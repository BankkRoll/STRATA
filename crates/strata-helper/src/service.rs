//! Service mode (SPEC §4, §19): the helper as an on-demand Windows service,
//! so administrators get fast scans without a UAC prompt per launch.
//!
//! Trust model:
//!
//! - **Install/uninstall** (`--install-service`, `--uninstall-service`) need
//!   an elevated caller. The service runs as `LocalSystem` with start type
//!   "manual"; its configuration (binary path and the trusted app image
//!   passed as `--client-image`) is writable by administrators only.
//! - **Starting:** the service DACL lets interactive users (`IU`) start it
//!   and query its status, nothing else, so the unelevated app can start it
//!   on demand ([`start_service`]). Start arguments from callers are
//!   ignored: the service reads its settings from its own command line,
//!   which comes from the SCM configuration.
//! - **Pipes:** for every user signed in to an active session the service
//!   creates `\\.\pipe\strata-helper-svc-<user SID>` ([`service_pipe_name`])
//!   with the same DACL as on-demand pipes (that user + SYSTEM, medium
//!   label, no create-instance right). The name is well known so the app
//!   can find it; it is not a secret.
//! - **Squatting:** a process could create that name before the service
//!   does. The app always verifies the *server* process (image path and
//!   signer must be the installed helper) before sending anything, so a
//!   squatter can only deny service, never impersonate the helper.
//! - **Clients:** the client must be the trusted app image, run as the
//!   pipe's user, and that user must be a local administrator (UAC-filtered
//!   tokens count). Standard users are refused: service mode replaces an
//!   administrator's UAC consent and must not give anyone else SYSTEM-level
//!   raw reads or deletes ([`crate::verify::ServiceClientVerifier`]).
//! - **Lifetime:** the service stops itself after [`SERVICE_IDLE`] with no
//!   connected client and no requests.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use strata_ipc::pipe::ServerConfig;
use strata_ipc::security::{PIPE_PREFIX, SecurityDescriptor, is_sid_string};
use strata_win::WinError;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, LookupAccountNameW, PSECURITY_DESCRIPTOR, PSID, SID_NAME_USE,
};
use windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_INFO_CLASS, WTS_SESSION_INFOW, WTSActive, WTSDomainName,
    WTSEnumerateSessionsW, WTSFreeMemory, WTSQuerySessionInformationW, WTSUserName,
};
use windows::Win32::System::Services::{SC_HANDLE, SetServiceObjectSecurity};
use windows::core::{PCWSTR, PWSTR};
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::diag::diag;
use crate::ops::Shared;
use crate::server::{Activity, HelperConfig, serve};
use crate::source::Volumes;
use crate::verify::{ServiceClientVerifier, trust_policy};

/// Service key name.
pub const SERVICE_NAME: &str = "StrataHelper";
/// Name shown in the Services console.
pub const SERVICE_DISPLAY_NAME: &str = "Strata Helper";
/// Description shown in the Services console.
pub const SERVICE_DESCRIPTION: &str =
    "Fast disk scans and verified deletes for Strata administrators. Starts on demand.";
/// Idle time after which the service stops itself.
pub const SERVICE_IDLE: Duration = Duration::from_secs(5 * 60);
/// How often the service looks for newly signed-in users.
const SESSION_REFRESH: Duration = Duration::from_secs(2);

/// Service DACL: SYSTEM and administrators manage it; interactive users may
/// only query it and start it.
pub const SERVICE_SDDL: &str =
    "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWRPLORC;;;IU)";

/// `ERROR_SERVICE_ALREADY_RUNNING`.
const ERROR_SERVICE_ALREADY_RUNNING: i32 = 1056;
/// `ERROR_SERVICE_DOES_NOT_EXIST`.
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
/// `ERROR_SERVICE_EXISTS`.
const ERROR_SERVICE_EXISTS: i32 = 1073;
/// `ERROR_SERVICE_NOT_ACTIVE`.
const ERROR_SERVICE_NOT_ACTIVE: i32 = 1062;

/// Service management failures.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// The Service Control Manager refused or failed.
    #[error("service control manager: {0}")]
    Scm(#[from] windows_service::Error),
    /// A Win32 call failed.
    #[error(transparent)]
    Win(#[from] WinError),
    /// The service is not installed.
    #[error("the Strata helper service is not installed")]
    NotInstalled,
    /// Invalid input.
    #[error("{0}")]
    Invalid(String),
}

impl ServiceError {
    fn os_code(&self) -> Option<i32> {
        match self {
            Self::Scm(windows_service::Error::Winapi(e)) => e.raw_os_error(),
            _ => None,
        }
    }
}

/// The well-known service pipe for `user_sid`:
/// `\\.\pipe\strata-helper-svc-<SID>`.
///
/// # Errors
///
/// `user_sid` is not a plain SID string.
///
/// # Example
///
/// ```
/// let name = strata_helper::service::service_pipe_name("S-1-5-21-1-2-3-1001").unwrap();
/// assert_eq!(name, r"\\.\pipe\strata-helper-svc-S-1-5-21-1-2-3-1001");
/// assert!(strata_ipc::security::is_valid_pipe_name(&name));
/// ```
pub fn service_pipe_name(user_sid: &str) -> Result<String, ServiceError> {
    if !is_sid_string(user_sid) {
        return Err(ServiceError::Invalid("invalid user SID".into()));
    }
    Ok(format!("{PIPE_PREFIX}svc-{user_sid}"))
}

/// Installs (or reconfigures) the service to run this executable with
/// `--service --client-image <client_image>`. Needs elevation.
///
/// # Errors
///
/// Not elevated (access denied), or the SCM call failed.
pub fn install(client_image: &Path) -> Result<(), ServiceError> {
    if !client_image.is_absolute() {
        return Err(ServiceError::Invalid(
            "--client-image must be an absolute path".into(),
        ));
    }
    let exe = std::env::current_exe()
        .map_err(|e| ServiceError::Invalid(format!("cannot locate the helper: {e}")))?;
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: SERVICE_DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::OnDemand,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec![
            OsString::from("--service"),
            OsString::from("--client-image"),
            client_image.as_os_str().to_owned(),
        ],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;
    let access =
        ServiceAccess::CHANGE_CONFIG | ServiceAccess::WRITE_DAC | ServiceAccess::QUERY_STATUS;
    let service = match manager.create_service(&info, access) {
        Ok(s) => s,
        Err(e) => {
            let e = ServiceError::from(e);
            if e.os_code() != Some(ERROR_SERVICE_EXISTS) {
                return Err(e);
            }
            let s = manager.open_service(SERVICE_NAME, access)?;
            s.change_config(&info)?;
            s
        }
    };
    service.set_description(SERVICE_DESCRIPTION)?;
    let sd = SecurityDescriptor::from_sddl(SERVICE_SDDL)?;
    // SAFETY: `service` is an open handle with WRITE_DAC; `sd` is a valid
    // self-relative descriptor that outlives the call.
    unsafe {
        SetServiceObjectSecurity(
            SC_HANDLE(service.raw_handle()),
            DACL_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR(sd.as_ptr()),
        )
    }
    .map_err(|e| WinError::new("SetServiceObjectSecurity", &e))?;
    Ok(())
}

/// Stops (if running) and deletes the service. Succeeds when it is not
/// installed. Needs elevation.
///
/// # Errors
///
/// Not elevated, or the SCM call failed.
pub fn uninstall() -> Result<(), ServiceError> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    let service = match manager.open_service(SERVICE_NAME, access) {
        Ok(s) => s,
        Err(e) => {
            let e = ServiceError::from(e);
            return if e.os_code() == Some(ERROR_SERVICE_DOES_NOT_EXIST) {
                Ok(())
            } else {
                Err(e)
            };
        }
    };
    if service.query_status()?.current_state != ServiceState::Stopped {
        if let Err(e) = service.stop() {
            let e = ServiceError::from(e);
            if e.os_code() != Some(ERROR_SERVICE_NOT_ACTIVE) {
                return Err(e);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while service.query_status()?.current_state != ServiceState::Stopped
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    service.delete()?;
    Ok(())
}

/// Whether the service is installed (and visible to this user).
#[must_use]
pub fn is_installed() -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|m| m.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS))
        .is_ok()
}

/// Starts the service if it is not running. Works unelevated thanks to the
/// service DACL ([`SERVICE_SDDL`]).
///
/// # Errors
///
/// [`ServiceError::NotInstalled`], or the SCM refused.
pub fn start_service() -> Result<(), ServiceError> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::START | ServiceAccess::QUERY_STATUS,
    ) {
        Ok(s) => s,
        Err(e) => {
            let e = ServiceError::from(e);
            return Err(if e.os_code() == Some(ERROR_SERVICE_DOES_NOT_EXIST) {
                ServiceError::NotInstalled
            } else {
                e
            });
        }
    };
    if service.query_status()?.current_state == ServiceState::Running {
        return Ok(());
    }
    match service.start::<&str>(&[]) {
        Ok(()) => Ok(()),
        Err(e) => {
            let e = ServiceError::from(e);
            if e.os_code() == Some(ERROR_SERVICE_ALREADY_RUNNING) {
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Running as a service
// -----------------------------------------------------------------------------

/// The trusted app image, set by [`run_service`] before the dispatcher
/// starts (the service main function receives only SCM start arguments,
/// which unprivileged callers control).
static CLIENT_IMAGE: OnceLock<PathBuf> = OnceLock::new();

windows_service::define_windows_service!(ffi_service_main, service_main);

/// Hands the process to the Service Control Manager; returns when the
/// service stops.
///
/// # Errors
///
/// Not started by the SCM (e.g. run from a console), or registration failed.
pub fn run_service(client_image: PathBuf) -> Result<(), ServiceError> {
    let _ = CLIENT_IMAGE.set(client_image);
    windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

fn service_main(_scm_arguments: Vec<OsString>) {
    if let Err(e) = service_body() {
        diag!("service failed: {e}");
    }
}

fn status(state: ServiceState, accept: ServiceControlAccept, code: u32) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(code),
        checkpoint: 0,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    }
}

fn service_body() -> Result<(), ServiceError> {
    let stop = Arc::new(AtomicBool::new(false));
    let handler_stop = Arc::clone(&stop);
    let handle = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            handler_stop.store(true, Ordering::SeqCst);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    handle.set_service_status(status(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        0,
    ))?;
    let result = CLIENT_IMAGE
        .get()
        .cloned()
        .ok_or_else(|| ServiceError::Invalid("no --client-image configured".into()))
        .and_then(|image| host(&image, &stop));
    let code = if result.is_ok() { 0 } else { 1 };
    handle.set_service_status(status(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        code,
    ))?;
    result
}

/// Serves one pipe per signed-in user until stopped or idle.
fn host(client_image: &Path, stop: &Arc<AtomicBool>) -> Result<(), ServiceError> {
    let shared = Shared::new(Volumes::raw());
    let activity = Arc::new(Activity::default());
    let policy: Arc<dyn strata_ipc::security::PeerVerifier> = Arc::new(trust_policy(client_image)?);
    let elevated = strata_win::process::is_elevated().unwrap_or(true);
    std::thread::scope(|s| {
        let mut servers: HashMap<String, std::thread::ScopedJoinHandle<'_, ()>> = HashMap::new();
        while !stop.load(Ordering::SeqCst) {
            servers.retain(|_, h| !h.is_finished());
            for sid in active_session_users() {
                if servers.contains_key(&sid) {
                    continue;
                }
                let Ok(pipe) = service_pipe_name(&sid) else {
                    continue;
                };
                let verifier =
                    Arc::new(ServiceClientVerifier::new(Arc::clone(&policy), sid.clone()));
                let mut server = ServerConfig::new(pipe, sid.clone(), verifier);
                server.elevated = elevated;
                server.capabilities = crate::run::capabilities(elevated, false);
                let config = HelperConfig {
                    server,
                    exit_on_disconnect: false,
                    accept_timeout: None,
                    idle_timeout: None,
                    parent: None,
                    max_in_flight: 16,
                    stop: Arc::clone(stop),
                    activity: Some(Arc::clone(&activity)),
                };
                let shared = &shared;
                servers.insert(
                    sid,
                    s.spawn(move || match serve(config, shared) {
                        Ok(r) => diag!("user pipe ended: {r:?}"),
                        Err(e) => diag!("user pipe failed: {e}"),
                    }),
                );
            }
            if activity.idle_for(SERVICE_IDLE) {
                stop.store(true, Ordering::SeqCst);
                break;
            }
            std::thread::sleep(SESSION_REFRESH);
        }
    });
    Ok(())
}

// -----------------------------------------------------------------------------
// Sessions
// -----------------------------------------------------------------------------

/// SIDs of the users signed in to active sessions (console or remote).
#[must_use]
pub fn active_session_users() -> Vec<String> {
    let mut sessions: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: out-pointers are valid; the array is freed below.
    if unsafe {
        WTSEnumerateSessionsW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            0,
            1,
            &mut sessions,
            &mut count,
        )
    }
    .is_err()
    {
        return Vec::new();
    }
    // SAFETY: WTSEnumerateSessionsW returned `count` entries at `sessions`.
    let list = unsafe { std::slice::from_raw_parts(sessions, count as usize) };
    let ids: Vec<u32> = list
        .iter()
        .filter(|s| s.State == WTSActive)
        .map(|s| s.SessionId)
        .collect();
    // SAFETY: allocated by WTSEnumerateSessionsW; freed once, after `list`
    // is no longer used.
    unsafe { WTSFreeMemory(sessions.cast()) };
    let mut out: Vec<String> = ids.into_iter().filter_map(session_user_sid).collect();
    out.sort();
    out.dedup();
    out
}

fn session_string(session: u32, class: WTS_INFO_CLASS) -> Option<String> {
    let mut buf = PWSTR::null();
    let mut bytes = 0u32;
    // SAFETY: out-pointers are valid; the buffer is freed below.
    unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            session,
            class,
            &mut buf,
            &mut bytes,
        )
    }
    .ok()?;
    // SAFETY: WTS returns a NUL-terminated string.
    let s = unsafe { buf.to_string() }.ok();
    // SAFETY: allocated by WTSQuerySessionInformationW; freed once.
    unsafe { WTSFreeMemory(buf.0.cast()) };
    s.filter(|s| !s.is_empty())
}

fn session_user_sid(session: u32) -> Option<String> {
    let user = session_string(session, WTSUserName)?;
    let domain = session_string(session, WTSDomainName).unwrap_or_default();
    let account = if domain.is_empty() {
        user
    } else {
        format!("{domain}\\{user}")
    };
    account_sid(&account).ok()
}

/// Resolves an account name (`DOMAIN\user`) to its SID string.
///
/// # Errors
///
/// The account cannot be resolved.
pub fn account_sid(account: &str) -> Result<String, WinError> {
    let name: Vec<u16> = account.encode_utf16().chain(std::iter::once(0)).collect();
    let mut sid_len = 0u32;
    let mut dom_len = 0u32;
    let mut use_ = SID_NAME_USE::default();
    // SAFETY: size query; failure with ERROR_INSUFFICIENT_BUFFER expected.
    let _ = unsafe {
        LookupAccountNameW(
            PCWSTR::null(),
            PCWSTR(name.as_ptr()),
            None,
            &mut sid_len,
            None,
            &mut dom_len,
            &mut use_,
        )
    };
    if sid_len == 0 {
        return Err(WinError::last("LookupAccountNameW"));
    }
    let mut sid = vec![0u64; (sid_len as usize).div_ceil(8)];
    let mut dom = vec![0u16; dom_len.max(1) as usize];
    // SAFETY: buffers have the sizes reported by the first call.
    unsafe {
        LookupAccountNameW(
            PCWSTR::null(),
            PCWSTR(name.as_ptr()),
            Some(PSID(sid.as_mut_ptr().cast())),
            &mut sid_len,
            Some(PWSTR(dom.as_mut_ptr())),
            &mut dom_len,
            &mut use_,
        )
    }
    .map_err(|e| WinError::new("LookupAccountNameW", &e))?;
    let mut text = PWSTR::null();
    // SAFETY: `sid` holds a valid SID; `text` is LocalAlloc'd and freed below.
    unsafe { ConvertSidToStringSidW(PSID(sid.as_mut_ptr().cast()), &mut text) }
        .map_err(|e| WinError::new("ConvertSidToStringSidW", &e))?;
    // SAFETY: NUL-terminated string from ConvertSidToStringSidW.
    let s = unsafe { text.to_string() }.unwrap_or_default();
    // SAFETY: allocated by ConvertSidToStringSidW; freed once.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_names_are_per_user_and_valid() {
        let n = service_pipe_name("S-1-5-21-1-2-3-1001").unwrap();
        assert!(strata_ipc::security::is_valid_pipe_name(&n));
        assert!(service_pipe_name("S-1-5-21-1-2-3-1001)(A;;GA;;;WD").is_err());
        assert!(service_pipe_name("").is_err());
    }

    #[test]
    fn service_sddl_parses() {
        assert!(SecurityDescriptor::from_sddl(SERVICE_SDDL).is_ok());
        assert!(SERVICE_SDDL.contains("(A;;CCLCSWRPLORC;;;IU)"));
    }

    #[test]
    fn current_session_user_resolves() {
        let me = strata_win::process::current_user().unwrap();
        let users = active_session_users();
        // NOTE: CI runners have no interactive session; locally the current
        // user is signed in to the console.
        if !users.is_empty() && me.sid.starts_with("S-1-5-21-") {
            assert!(users.contains(&me.sid), "{users:?}");
        }
        if let Some(account) = me.account {
            assert_eq!(account_sid(&account).unwrap(), me.sid);
        }
    }

    #[test]
    fn unelevated_install_is_refused() {
        if strata_win::process::is_elevated().unwrap_or(false) {
            return;
        }
        assert!(install(Path::new(r"C:\Program Files\Strata\strata.exe")).is_err());
        assert!(!is_installed() || uninstall().is_err());
    }
}
