//! Per-request cancellation.
//!
//! A request can block in three different ways, each with its own way of
//! being woken: the MFT scanner polls an `AtomicBool`, `strata-clean` takes a
//! [`CancelToken`], and overlapped `DeviceIoControl` waits (USN reads) wait
//! on a kernel event. [`Cancel`] drives all three from one call.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use strata_clean::CancelToken;
use strata_win::{OwnedHandle, WinError};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{CreateEventW, SetEvent};

/// Cancellation for one in-flight request. Clones share state.
#[derive(Debug, Clone)]
pub struct Cancel {
    flag: Arc<AtomicBool>,
    token: CancelToken,
    event: Arc<OwnedHandle>,
}

impl Cancel {
    /// A fresh, not-cancelled state with its own manual-reset event.
    ///
    /// # Errors
    ///
    /// `CreateEventW` failed.
    pub fn new() -> Result<Self, WinError> {
        // SAFETY: unnamed manual-reset event, default security; the handle
        // is owned below.
        let h = unsafe { CreateEventW(None, true, false, None) }
            .map_err(|e| WinError::new("CreateEventW", &e))?;
        // SAFETY: fresh handle from CreateEventW, owned by nobody else.
        let event = unsafe { OwnedHandle::from_raw(h) }
            .ok_or_else(|| WinError::from_win32("CreateEventW", 6))?;
        Ok(Self {
            flag: Arc::new(AtomicBool::new(false)),
            token: CancelToken::new(),
            event: Arc::new(event),
        })
    }

    /// Requests cancellation. Idempotent.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.token.cancel();
        // SAFETY: the event handle lives as long as `self`.
        unsafe {
            let _ = SetEvent(self.event.raw());
        }
    }

    /// Whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// The flag the MFT scanner polls.
    #[must_use]
    pub fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.flag)
    }

    /// The token `strata-clean` observes.
    #[must_use]
    pub fn token(&self) -> &CancelToken {
        &self.token
    }

    /// The manual-reset event signaled on cancellation (valid while `self`
    /// lives).
    #[must_use]
    pub fn event(&self) -> HANDLE {
        self.event.raw()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::System::Threading::WaitForSingleObject;

    #[test]
    fn cancel_sets_every_signal() {
        let c = Cancel::new().unwrap();
        let other = c.clone();
        assert!(!c.is_cancelled());
        other.cancel();
        assert!(c.is_cancelled());
        assert!(c.flag().load(Ordering::SeqCst));
        assert!(c.token().is_cancelled());
        // SAFETY: live event handle.
        assert_eq!(unsafe { WaitForSingleObject(c.event(), 0) }, WAIT_OBJECT_0);
    }
}
