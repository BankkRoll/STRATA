//! Restart Manager sessions (RAII).

use std::io;

use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS, FILETIME, WIN32_ERROR};
use windows::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RM_UNIQUE_PROCESS, RmEndSession, RmGetList,
    RmRegisterResources, RmShutdown, RmStartSession,
};
use windows::core::{PCWSTR, PWSTR};

/// One process holding a registered resource.
#[derive(Debug, Clone)]
pub(crate) struct RmProcess {
    pub pid: u32,
    pub start_time: u64,
    pub app_name: String,
    pub service: String,
    pub app_type: i32,
    pub restartable: bool,
}

/// A Restart Manager session, ended on drop.
#[derive(Debug)]
pub(crate) struct RmSession(u32);

fn check(e: WIN32_ERROR) -> io::Result<()> {
    if e == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(e.0 as i32))
    }
}

impl RmSession {
    pub(crate) fn start() -> io::Result<Self> {
        let mut handle = 0u32;
        let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
        // SAFETY: `handle` and `key` are valid out buffers of the required size.
        check(unsafe { RmStartSession(&mut handle, None, PWSTR(key.as_mut_ptr())) })?;
        Ok(Self(handle))
    }

    /// Registers files (NUL-terminated paths). Restart Manager accepts at
    /// most a few thousand per call, so callers bound the count.
    pub(crate) fn register_files(&self, files: &[Vec<u16>]) -> io::Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let ptrs: Vec<PCWSTR> = files.iter().map(|f| PCWSTR(f.as_ptr())).collect();
        // SAFETY: every pointer refers to a NUL-terminated buffer in `files`,
        // which outlives the call.
        check(unsafe { RmRegisterResources(self.0, Some(&ptrs), None, None) })
    }

    /// Registers one process, for a targeted polite shutdown.
    pub(crate) fn register_process(&self, pid: u32, start_time: u64) -> io::Result<()> {
        let p = RM_UNIQUE_PROCESS {
            dwProcessId: pid,
            ProcessStartTime: FILETIME {
                dwLowDateTime: start_time as u32,
                dwHighDateTime: (start_time >> 32) as u32,
            },
        };
        // SAFETY: `p` is valid for the duration of the call.
        check(unsafe { RmRegisterResources(self.0, None, Some(&[p]), None) })
    }

    /// Processes holding any registered resource.
    pub(crate) fn list(&self) -> io::Result<Vec<RmProcess>> {
        let mut needed = 0u32;
        let mut buf: Vec<RM_PROCESS_INFO> = Vec::new();
        loop {
            let mut count = buf.len() as u32;
            let mut reasons = 0u32;
            let ptr = if buf.is_empty() {
                None
            } else {
                Some(buf.as_mut_ptr())
            };
            // SAFETY: `buf` holds `count` elements; out params are valid.
            let r = unsafe { RmGetList(self.0, &mut needed, &mut count, ptr, &mut reasons) };
            if r == ERROR_MORE_DATA {
                // SAFETY: RM_PROCESS_INFO is plain old data; zeroed is valid.
                buf.resize(needed as usize + 4, unsafe { std::mem::zeroed() });
                continue;
            }
            check(r)?;
            buf.truncate(count as usize);
            return Ok(buf.iter().map(convert).collect());
        }
    }

    /// Asks every registered process to close (WM_QUERYENDSESSION / WM_CLOSE
    /// / service stop). Never forced: an app that refuses stays running and
    /// this returns an error.
    pub(crate) fn shutdown_politely(&self) -> io::Result<()> {
        // SAFETY: 0 = no RmForceShutdown; no progress callback.
        check(unsafe { RmShutdown(self.0, 0, None) })
    }
}

impl Drop for RmSession {
    fn drop(&mut self) {
        // SAFETY: ends the session we started, exactly once.
        let _ = unsafe { RmEndSession(self.0) };
    }
}

fn convert(p: &RM_PROCESS_INFO) -> RmProcess {
    let s = |b: &[u16]| {
        let n = b.iter().position(|&u| u == 0).unwrap_or(b.len());
        String::from_utf16_lossy(&b[..n])
    };
    RmProcess {
        pid: p.Process.dwProcessId,
        start_time: (u64::from(p.Process.ProcessStartTime.dwHighDateTime) << 32)
            | u64::from(p.Process.ProcessStartTime.dwLowDateTime),
        app_name: s(&p.strAppName),
        service: s(&p.strServiceShortName),
        app_type: p.ApplicationType.0,
        restartable: p.bRestartable.as_bool(),
    }
}
