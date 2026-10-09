//! Processes, top-level windows, elevation and Shell launches.

use std::io;
use std::mem::size_of;

use windows::Win32::Foundation::{HWND, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, INFINITE, OpenProcess, OpenProcessToken,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetWindow, GetWindowThreadProcessId, IsWindowVisible, PostMessageW,
    SW_SHOWNORMAL, WM_CLOSE,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

use super::handle::OwnedHandle;
use super::wide;

/// One running process.
#[derive(Debug, Clone)]
pub(crate) struct ProcessEntry {
    pub pid: u32,
    pub exe_name: String,
}

/// Snapshot of running processes.
pub(crate) fn processes() -> io::Result<Vec<ProcessEntry>> {
    // SAFETY: plain snapshot call.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }?;
    // SAFETY: the snapshot handle is ours to close.
    let snap = unsafe { OwnedHandle::from_raw(snap) };
    let mut e = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut out = Vec::new();
    // SAFETY: `e` has dwSize set and is a valid out pointer.
    let mut ok = unsafe { Process32FirstW(snap.raw(), &mut e) }.is_ok();
    while ok {
        let n = e
            .szExeFile
            .iter()
            .position(|&u| u == 0)
            .unwrap_or(e.szExeFile.len());
        out.push(ProcessEntry {
            pid: e.th32ProcessID,
            exe_name: String::from_utf16_lossy(&e.szExeFile[..n]),
        });
        // SAFETY: as above.
        ok = unsafe { Process32NextW(snap.raw(), &mut e) }.is_ok();
    }
    Ok(out)
}

/// Full image path of a process, when we may query it.
pub(crate) fn image_path(pid: u32) -> Option<String> {
    // SAFETY: limited query right only.
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    // SAFETY: we own the process handle.
    let h = unsafe { OwnedHandle::from_raw(h) };
    let mut buf = [0u16; 32 * 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` is valid for `len` units.
    unsafe {
        QueryFullProcessImageNameW(
            h.raw(),
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .ok()?;
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// Posts `WM_CLOSE` to every visible, unowned top-level window of `pid`.
/// Returns how many windows were asked. The app may still refuse (unsaved
/// work prompts); nothing is terminated.
pub(crate) fn post_close_to_windows(pid: u32) -> usize {
    struct Ctx {
        pid: u32,
        windows: Vec<HWND>,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `&mut Ctx` passed to EnumWindows below,
        // alive for the whole enumeration.
        let ctx = unsafe { &mut *(lparam.0 as *mut Ctx) };
        let mut owner_pid = 0u32;
        // SAFETY: `hwnd` comes from EnumWindows.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner_pid)) };
        // SAFETY: `hwnd` comes from EnumWindows.
        let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
        // SAFETY: `hwnd` comes from EnumWindows.
        let owned = unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|o| !o.is_invalid());
        if owner_pid == ctx.pid && visible && !owned {
            ctx.windows.push(hwnd);
        }
        BOOL(1)
    }
    let mut ctx = Ctx {
        pid,
        windows: Vec::new(),
    };
    // SAFETY: `ctx` outlives the synchronous enumeration.
    let _ = unsafe { EnumWindows(Some(cb), LPARAM(&raw mut ctx as isize)) };
    let mut asked = 0;
    for w in ctx.windows {
        // SAFETY: posting a message to a window handle; harmless if the
        // window has since closed.
        if unsafe { PostMessageW(Some(w), WM_CLOSE, WPARAM(0), LPARAM(0)) }.is_ok() {
            asked += 1;
        }
    }
    asked
}

/// Whether this process runs elevated.
pub(crate) fn is_elevated() -> bool {
    let mut token = windows::Win32::Foundation::HANDLE::default();
    // SAFETY: pseudo handle for the current process; `token` is an out param.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return false;
    }
    // SAFETY: we own the token handle.
    let token = unsafe { OwnedHandle::from_raw(token) };
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    // SAFETY: `elevation` is valid for its size.
    unsafe {
        GetTokenInformation(
            token.raw(),
            TokenElevation,
            Some((&raw mut elevation).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    }
    .is_ok()
        && elevation.TokenIsElevated != 0
}

/// String SID of the user running this process (`S-1-5-21-...`).
pub(crate) fn current_user_sid() -> Option<String> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{TOKEN_USER, TokenUser};

    let mut token = windows::Win32::Foundation::HANDLE::default();
    // SAFETY: pseudo handle for the current process; `token` is an out param.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
    // SAFETY: we own the token handle.
    let token = unsafe { OwnedHandle::from_raw(token) };
    let mut len = 0u32;
    // SAFETY: size query; expected to fail with ERROR_INSUFFICIENT_BUFFER.
    let _ = unsafe { GetTokenInformation(token.raw(), TokenUser, None, 0, &mut len) };
    if len == 0 {
        return None;
    }
    // u64 storage keeps TOKEN_USER suitably aligned.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` is valid for `len` bytes.
    unsafe {
        GetTokenInformation(
            token.raw(),
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            len,
            &mut len,
        )
    }
    .ok()?;
    // SAFETY: the buffer now holds a TOKEN_USER whose SID points into it.
    let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
    let mut s = PWSTR::null();
    // SAFETY: valid SID; `s` receives a LocalAlloc'd string freed below.
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut s) }.ok()?;
    // SAFETY: `s` is a valid NUL-terminated string.
    let out = unsafe { s.to_string() }.ok();
    // SAFETY: frees the LocalAlloc'd string exactly once.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(s.0.cast())));
    }
    out
}

/// `ShellExecuteExW` with an optional verb (`runas` for UAC). When `wait` is
/// set, blocks until the process exits and returns its exit code.
pub(crate) fn shell_execute(
    verb: Option<&str>,
    file: &str,
    params: Option<&str>,
    wait: bool,
) -> io::Result<Option<u32>> {
    let verb_w = verb.map(wide);
    let file_w = wide(file);
    let params_w = params.map(wide);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC
            | if wait {
                SEE_MASK_NOCLOSEPROCESS
            } else {
                Default::default()
            },
        lpVerb: verb_w
            .as_ref()
            .map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr())),
        lpFile: PCWSTR(file_w.as_ptr()),
        lpParameters: params_w
            .as_ref()
            .map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr())),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: every string pointer refers to a live NUL-terminated buffer.
    unsafe { ShellExecuteExW(&mut info) }?;
    if !wait || info.hProcess.is_invalid() {
        return Ok(None);
    }
    // SAFETY: SEE_MASK_NOCLOSEPROCESS hands us the process handle.
    let p = unsafe { OwnedHandle::from_raw(info.hProcess) };
    // SAFETY: waiting on our own handle.
    if unsafe { WaitForSingleObject(p.raw(), INFINITE) } != WAIT_OBJECT_0 {
        return Err(io::Error::last_os_error());
    }
    let mut code = 0u32;
    // SAFETY: valid handle and out param.
    unsafe { GetExitCodeProcess(p.raw(), &mut code) }?;
    Ok(Some(code))
}
