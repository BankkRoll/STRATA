//! Watching the process that launched the helper.
//!
//! An on-demand helper must not outlive the app: if the app crashes without
//! closing the pipe cleanly (or never connects), the helper notices the
//! parent's exit and quits instead of lingering elevated.

use strata_win::{OwnedHandle, WinError};
use windows::Win32::Foundation::WAIT_TIMEOUT;
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};

/// A handle to a process, used only to test whether it is still running.
#[derive(Debug)]
pub struct ProcessWatch {
    pid: u32,
    handle: OwnedHandle,
}

impl ProcessWatch {
    /// Opens `pid` with `SYNCHRONIZE` access.
    ///
    /// Holding the handle pins the process object, so a reused PID can never
    /// be mistaken for the original process.
    ///
    /// # Errors
    ///
    /// The process does not exist (already exited) or cannot be opened.
    pub fn open(pid: u32) -> Result<Self, WinError> {
        // SAFETY: plain OpenProcess; the handle is owned below.
        let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }
            .map_err(|e| WinError::new("OpenProcess", &e))?;
        // SAFETY: fresh handle owned by nobody else.
        let handle = unsafe { OwnedHandle::from_raw(h) }
            .ok_or_else(|| WinError::from_win32("OpenProcess", 87))?;
        Ok(Self { pid, handle })
    }

    /// The watched process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether the process is still running.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        // SAFETY: the handle is live and has SYNCHRONIZE access.
        unsafe { WaitForSingleObject(self.handle.raw(), 0) == WAIT_TIMEOUT }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watches_a_child_until_it_exits() {
        let me = ProcessWatch::open(std::process::id()).unwrap();
        assert!(me.is_alive());
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "exit", "0"])
            .spawn()
            .unwrap();
        let w = ProcessWatch::open(child.id()).ok();
        child.wait().unwrap();
        if let Some(w) = w {
            assert!(!w.is_alive());
        }
    }
}
