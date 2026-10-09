//! The error type shared by every wrapper in this crate.

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::WIN32_ERROR;
use windows::core::HRESULT;

/// A failed Win32 or COM call, with the operation that failed.
///
/// Serializable so it can be shown in the UI or carried over the helper pipe
/// without losing the code.
///
/// # Example
///
/// ```
/// use strata_win::WinError;
/// let e = WinError::from_win32("OpenThing", 5);
/// assert!(e.is_access_denied());
/// assert_eq!(e.win32_code(), Some(5));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{op} failed: {message} (0x{hresult:08X})")]
pub struct WinError {
    /// Name of the API or step that failed.
    pub op: String,
    /// The failure as an `HRESULT` (Win32 codes are wrapped as `0x8007xxxx`).
    pub hresult: u32,
    /// System message for the code, in the user's UI language.
    pub message: String,
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, WinError>;

/// `ERROR_ACCESS_DENIED`.
pub(crate) const ERROR_ACCESS_DENIED: u32 = 5;

impl WinError {
    /// Wraps a `windows` crate error.
    #[must_use]
    pub fn new(op: impl Into<String>, e: &windows::core::Error) -> Self {
        Self {
            op: op.into(),
            hresult: e.code().0 as u32,
            message: e.message().trim_end().to_owned(),
        }
    }

    /// Builds an error from a raw Win32 error code.
    #[must_use]
    pub fn from_win32(op: impl Into<String>, code: u32) -> Self {
        let hr = HRESULT::from_win32(code);
        Self {
            op: op.into(),
            hresult: hr.0 as u32,
            message: hr.message().trim_end().to_owned(),
        }
    }

    /// Builds an error from a `WIN32_ERROR` return value.
    #[must_use]
    pub(crate) fn from_status(op: impl Into<String>, code: WIN32_ERROR) -> Self {
        Self::from_win32(op, code.0)
    }

    /// Captures the calling thread's last error.
    #[must_use]
    pub fn last(op: impl Into<String>) -> Self {
        Self::new(op, &windows::core::Error::from_thread())
    }

    /// The Win32 error code, when this wraps one (`HRESULT_FROM_WIN32`).
    #[must_use]
    pub fn win32_code(&self) -> Option<u32> {
        (self.hresult & 0xFFFF_0000 == 0x8007_0000).then_some(self.hresult & 0xFFFF)
    }

    /// Whether the failure was `ERROR_ACCESS_DENIED` (or `E_ACCESSDENIED`).
    #[must_use]
    pub fn is_access_denied(&self) -> bool {
        self.win32_code() == Some(ERROR_ACCESS_DENIED)
    }
}

/// Attaches an operation name to a `windows` crate result.
pub(crate) trait Context<T> {
    /// Maps the error into a [`WinError`] tagged with `op`.
    fn ctx(self, op: &'static str) -> Result<T>;
}

impl<T> Context<T> for windows::core::Result<T> {
    fn ctx(self, op: &'static str) -> Result<T> {
        self.map_err(|e| WinError::new(op, &e))
    }
}

/// Converts a `WIN32_ERROR` status return into a result.
pub(crate) fn check(op: &'static str, status: WIN32_ERROR) -> Result<()> {
    if status.0 == 0 {
        Ok(())
    } else {
        Err(WinError::from_status(op, status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win32_codes_round_trip_through_hresult() {
        let e = WinError::from_win32("x", 2);
        assert_eq!(e.hresult, 0x8007_0002);
        assert_eq!(e.win32_code(), Some(2));
        assert!(!e.message.is_empty());
    }

    #[test]
    fn non_win32_hresult_has_no_win32_code() {
        let e = WinError::new(
            "x",
            &windows::core::Error::from_hresult(HRESULT(0x8031_0000_u32 as i32)),
        );
        assert_eq!(e.win32_code(), None);
    }

    #[test]
    fn check_maps_status() {
        assert!(check("x", WIN32_ERROR(0)).is_ok());
        assert!(check("x", WIN32_ERROR(5)).unwrap_err().is_access_denied());
    }
}
