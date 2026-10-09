//! Processes and elevation (SPEC §4).
//!
//! - Elevation queries and privilege management (re-exported from the token
//!   module): [`is_elevated`], [`elevation_type`], [`enable_privilege`],
//!   [`drop_privileges`].
//! - [`launch_elevated`]: starts the helper through UAC (`runas`), telling a
//!   declined prompt apart from real failures.
//! - [`ProcessInfo`]: image path and start time of a PID. (PID, start time) is
//!   the identity of a process, because PIDs are reused.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use strata_core::FileTime;
use windows::Win32::Foundation::{ERROR_CANCELLED, FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetExitCodeProcess, GetProcessId, GetProcessTimes, OpenProcess,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::PWSTR;

use crate::error::{Context, Result, WinError};
use crate::handle::OwnedHandle;
pub use crate::token::{
    ElevationType, PrivilegeError, PrivilegeGuard, PrivilegeState, SE_BACKUP, SE_CHANGE_NOTIFY,
    SE_MANAGE_VOLUME, SE_RESTORE, Token, UserAccount, current_user, drop_privileges,
    elevation_type, enable_privilege, is_elevated,
};
use crate::wide::WideCString;

/// `STILL_ACTIVE` exit code.
const STILL_ACTIVE: u32 = 0x103;

/// Why [`launch_elevated`] failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaunchError {
    /// The user clicked "No" on the UAC prompt (`ERROR_CANCELLED`). The app
    /// falls back to the unelevated walker (SPEC §4).
    #[error("the user declined the elevation prompt")]
    Declined,
    /// Any other failure (missing file, policy blocks elevation, ...).
    #[error(transparent)]
    Failed(#[from] WinError),
}

impl LaunchError {
    /// Classifies a `ShellExecuteExW` failure.
    #[must_use]
    pub fn from_win_error(e: WinError) -> Self {
        if e.win32_code() == Some(ERROR_CANCELLED.0) {
            Self::Declined
        } else {
            Self::Failed(e)
        }
    }
}

/// A process started by [`launch_elevated`].
#[derive(Debug)]
pub struct ElevatedChild {
    process: OwnedHandle,
    pid: u32,
}

impl ElevatedChild {
    /// Process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Process handle (`SYNCHRONIZE` + query access).
    #[must_use]
    pub fn handle(&self) -> HANDLE {
        self.process.raw()
    }

    /// Exit code if the process has exited, `None` while it runs.
    pub fn try_exit_code(&self) -> Result<Option<u32>> {
        exit_code(self.process.raw())
    }

    /// Waits up to `timeout` for exit; returns the exit code if it exited.
    pub fn wait(&self, timeout: Duration) -> Result<Option<u32>> {
        wait_for_exit(self.process.raw(), timeout)
    }
}

fn exit_code(h: HANDLE) -> Result<Option<u32>> {
    let mut code = 0u32;
    // SAFETY: `h` is a live process handle with query access.
    unsafe { GetExitCodeProcess(h, &mut code) }.ctx("GetExitCodeProcess")?;
    // NOTE: a process that exits with 259 (STILL_ACTIVE) is indistinguishable
    // here; `wait` checks the handle's signaled state first to avoid that.
    Ok((code != STILL_ACTIVE).then_some(code))
}

fn wait_for_exit(h: HANDLE, timeout: Duration) -> Result<Option<u32>> {
    let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
    // SAFETY: `h` is a live process handle with SYNCHRONIZE access.
    let r = unsafe { WaitForSingleObject(h, ms) };
    if r == WAIT_TIMEOUT {
        return Ok(None);
    }
    if r != WAIT_OBJECT_0 {
        return Err(WinError::last("WaitForSingleObject"));
    }
    let mut code = 0u32;
    // SAFETY: as above; the process has exited.
    unsafe { GetExitCodeProcess(h, &mut code) }.ctx("GetExitCodeProcess")?;
    Ok(Some(code))
}

/// Quotes one argument per the `CommandLineToArgvW` / MSVC CRT rules, so the
/// child sees exactly `arg` in its `argv`.
#[must_use]
pub fn quote_arg(arg: &OsStr) -> OsString {
    let units: Vec<u16> = arg.encode_wide().collect();
    let needs_quotes = units.is_empty()
        || units
            .iter()
            .any(|&c| c == u16::from(b' ') || c == u16::from(b'\t') || c == u16::from(b'"'));
    if !needs_quotes {
        return arg.to_os_string();
    }
    let bs = u16::from(b'\\');
    let q = u16::from(b'"');
    let mut out = vec![q];
    let mut backslashes = 0usize;
    for &c in &units {
        if c == bs {
            backslashes += 1;
            continue;
        }
        if c == q {
            out.extend(std::iter::repeat_n(bs, backslashes * 2 + 1));
        } else {
            out.extend(std::iter::repeat_n(bs, backslashes));
        }
        backslashes = 0;
        out.push(c);
    }
    // Backslashes before the closing quote must be doubled.
    out.extend(std::iter::repeat_n(bs, backslashes * 2));
    out.push(q);
    OsString::from_wide(&out)
}

/// Joins arguments into a command-line parameter string.
#[must_use]
pub fn join_args<S: AsRef<OsStr>>(args: &[S]) -> OsString {
    let mut out = OsString::new();
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push(" ");
        }
        out.push(quote_arg(a.as_ref()));
    }
    out
}

/// Starts `exe` elevated through UAC (`ShellExecuteExW`, verb `runas`) and
/// returns its process handle.
///
/// Blocks while the consent prompt is shown.
///
/// # Example
///
/// ```no_run
/// use strata_win::process::{launch_elevated, LaunchError};
/// match launch_elevated(r"C:\Program Files\Strata\strata-helper.exe".as_ref(), &["--pipe", "x"]) {
///     Ok(child) => println!("helper pid {}", child.pid()),
///     Err(LaunchError::Declined) => println!("standard scan"),
///     Err(e) => eprintln!("{e}"),
/// }
/// ```
pub fn launch_elevated<S: AsRef<OsStr>>(
    exe: &Path,
    args: &[S],
) -> std::result::Result<ElevatedChild, LaunchError> {
    let verb = WideCString::new("runas");
    let file = WideCString::new(exe);
    let params = WideCString::new(join_args(args));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        // NOTE: NOASYNC because the caller may not pump messages; FLAG_NO_UI
        // suppresses error dialogs (not the consent prompt itself).
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: verb.as_pcwstr(),
        lpFile: file.as_pcwstr(),
        lpParameters: params.as_pcwstr(),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: `info` is fully initialized and every string it points to
    // outlives the call.
    unsafe { ShellExecuteExW(&mut info) }
        .map_err(|e| LaunchError::from_win_error(WinError::new("ShellExecuteExW", &e)))?;
    // SAFETY: with SEE_MASK_NOCLOSEPROCESS the handle (if any) is ours.
    let process = unsafe { OwnedHandle::from_raw(info.hProcess) }.ok_or_else(|| {
        // NOTE: no handle means the shell reused an existing process (DDE);
        // that never happens for an .exe, so treat it as a failure.
        LaunchError::Failed(WinError::from_win32("ShellExecuteExW (no process)", 6))
    })?;
    // SAFETY: `process` is a live handle.
    let pid = unsafe { GetProcessId(process.raw()) };
    Ok(ElevatedChild { process, pid })
}

/// Identity and image of a running process.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProcessInfo {
    /// Process id (reused by Windows after exit).
    pub pid: u32,
    /// Creation time; with `pid`, uniquely identifies the process.
    pub start_time: FileTime,
    /// Full Win32 image path (`QueryFullProcessImageNameW`).
    pub image: PathBuf,
}

impl ProcessInfo {
    /// Opens `pid` once and reads its image path and start time from the
    /// same handle, so both describe the same process even if the PID is
    /// recycled concurrently.
    pub fn of(pid: u32) -> Result<Self> {
        // SAFETY: plain FFI; the returned handle is owned below.
        let h = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
        }
        .ctx("OpenProcess")?;
        // SAFETY: OpenProcess returned a handle we now own.
        let h = unsafe { OwnedHandle::from_raw(h) }
            .ok_or_else(|| WinError::from_win32("OpenProcess", 6))?;
        Ok(Self {
            pid,
            start_time: process_start_time(h.raw())?,
            image: process_image_path(h.raw())?,
        })
    }

    /// The current process.
    pub fn current() -> Result<Self> {
        // SAFETY: no arguments.
        Self::of(unsafe { GetCurrentProcessId() })
    }
}

/// Image path of a process handle opened with
/// `PROCESS_QUERY_LIMITED_INFORMATION`.
pub fn process_image_path(process: HANDLE) -> Result<PathBuf> {
    let mut buf = vec![0u16; 1024];
    loop {
        let mut len = buf.len() as u32;
        // SAFETY: `buf` holds `len` writable units.
        let r = unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
        };
        match r {
            Ok(()) => return Ok(PathBuf::from(OsString::from_wide(&buf[..len as usize]))),
            // NOTE: ERROR_INSUFFICIENT_BUFFER (122); long paths can exceed
            // MAX_PATH, so grow up to the 32K limit.
            Err(e) if e.code().0 as u32 == 0x8007_007A && buf.len() < 32_768 => {
                buf.resize(buf.len() * 4, 0);
            }
            Err(e) => return Err(WinError::new("QueryFullProcessImageNameW", &e)),
        }
    }
}

/// Creation time of a process handle.
pub fn process_start_time(process: HANDLE) -> Result<FileTime> {
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: all out-pointers are valid FILETIMEs.
    unsafe { GetProcessTimes(process, &mut c, &mut e, &mut k, &mut u) }.ctx("GetProcessTimes")?;
    Ok(FileTime(
        (u64::from(c.dwHighDateTime) << 32) | u64::from(c.dwLowDateTime),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &str) -> String {
        quote_arg(OsStr::new(s)).to_string_lossy().into_owned()
    }

    #[test]
    fn argument_quoting_follows_crt_rules() {
        assert_eq!(q("plain"), "plain");
        assert_eq!(q(""), "\"\"");
        assert_eq!(q("a b"), "\"a b\"");
        assert_eq!(q(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(q(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(q(r"C:\no\quotes"), r"C:\no\quotes");
        assert_eq!(q(r#"a\"b c"#), r#""a\\\"b c""#);
        assert_eq!(
            join_args(&["--pipe", r"\\.\pipe\x y"]).to_string_lossy(),
            r#"--pipe "\\.\pipe\x y""#
        );
    }

    #[test]
    fn declined_prompt_is_distinguished() {
        assert_eq!(
            LaunchError::from_win_error(WinError::from_win32("x", 1223)),
            LaunchError::Declined
        );
        assert!(matches!(
            LaunchError::from_win_error(WinError::from_win32("x", 2)),
            LaunchError::Failed(_)
        ));
    }

    #[test]
    fn current_process_identity() {
        let me = ProcessInfo::current().unwrap();
        let exe = std::env::current_exe().unwrap();
        assert!(crate::path::eq_ignore_case(
            me.image.as_os_str(),
            exe.as_os_str()
        ));
        assert!(me.start_time.0 > 0);
        let again = ProcessInfo::current().unwrap();
        assert_eq!(me, again);
        assert!(ProcessInfo::of(0).is_err());
    }

    #[test]
    fn waits_for_a_child_exit() {
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/c", "exit", "7"])
            .spawn()
            .unwrap();
        let info = ProcessInfo::of(child.id());
        // SAFETY: opening a handle to our own child for SYNCHRONIZE access.
        let h = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                child.id(),
            )
        }
        .unwrap();
        // SAFETY: fresh handle owned here.
        let h = unsafe { OwnedHandle::from_raw(h) }.unwrap();
        assert_eq!(
            wait_for_exit(h.raw(), Duration::from_secs(10)).unwrap(),
            Some(7)
        );
        assert_eq!(exit_code(h.raw()).unwrap(), Some(7));
        assert_eq!(child.wait().unwrap().code(), Some(7));
        if let Ok(info) = info {
            assert!(
                info.image
                    .to_string_lossy()
                    .to_lowercase()
                    .ends_with("cmd.exe")
            );
        }
    }
}
