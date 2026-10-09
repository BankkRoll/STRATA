//! Typed request failures and their mapping to protocol error codes.

use std::io;

use strata_clean::CleanError;
use strata_ipc::IpcError;
use strata_ipc::protocol::{ErrorCode, ErrorReply};
use strata_ntfs::NtfsError;
use strata_win::WinError;

/// Win32 `ERROR_FILE_NOT_FOUND`.
const ERROR_FILE_NOT_FOUND: u32 = 2;
/// Win32 `ERROR_PATH_NOT_FOUND`.
const ERROR_PATH_NOT_FOUND: u32 = 3;
/// Win32 `ERROR_ACCESS_DENIED`.
const ERROR_ACCESS_DENIED: u32 = 5;
/// Win32 `ERROR_INVALID_DRIVE`.
const ERROR_INVALID_DRIVE: u32 = 15;
/// Win32 `ERROR_NOT_READY`.
const ERROR_NOT_READY: u32 = 21;
/// Win32 `ERROR_SHARING_VIOLATION`.
const ERROR_SHARING_VIOLATION: u32 = 32;
/// Win32 `ERROR_NOT_SUPPORTED`.
const ERROR_NOT_SUPPORTED: u32 = 50;
/// Win32 `ERROR_INVALID_FUNCTION` (FSCTL not supported by the filesystem).
const ERROR_INVALID_FUNCTION: u32 = 1;
/// Win32 `ERROR_DEVICE_NOT_CONNECTED`.
const ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;

/// A failed request, ready to be sent as [`ErrorReply`].
///
/// Messages describe the failure in terms of what the client sent; they
/// never include data the client did not already have.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct HelperError {
    /// Protocol category.
    pub code: ErrorCode,
    /// Human-readable detail.
    pub message: String,
    /// Set when the client went away; nothing can be sent back.
    pub disconnected: bool,
}

impl HelperError {
    /// A new error.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            disconnected: false,
        }
    }

    /// [`ErrorCode::BadRequest`].
    #[must_use]
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, message)
    }

    /// [`ErrorCode::Internal`].
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }

    /// The client disconnected while the request ran.
    #[must_use]
    pub fn disconnected() -> Self {
        Self {
            code: ErrorCode::Cancelled,
            message: "client disconnected".into(),
            disconnected: true,
        }
    }

    /// Maps a Win32 error code.
    #[must_use]
    pub fn from_win32(code: u32, message: impl Into<String>) -> Self {
        let category = match code {
            ERROR_ACCESS_DENIED => ErrorCode::AccessDenied,
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => ErrorCode::NotFound,
            ERROR_INVALID_DRIVE | ERROR_NOT_READY | ERROR_DEVICE_NOT_CONNECTED => {
                ErrorCode::UnknownVolume
            }
            ERROR_SHARING_VIOLATION => ErrorCode::Busy,
            ERROR_NOT_SUPPORTED | ERROR_INVALID_FUNCTION => ErrorCode::NotSupported,
            _ => ErrorCode::Io,
        };
        Self::new(category, message)
    }

    /// Maps an I/O error from opening or reading a volume.
    #[must_use]
    pub fn from_io(context: &str, e: &io::Error) -> Self {
        match e.raw_os_error() {
            Some(code) => Self::from_win32(win32_code(code), format!("{context}: {e}")),
            None if e.kind() == io::ErrorKind::UnexpectedEof => Self::new(
                ErrorCode::Io,
                format!("{context}: read past the end of the volume"),
            ),
            None => Self::new(ErrorCode::Io, format!("{context}: {e}")),
        }
    }

    /// The wire form.
    #[must_use]
    pub fn to_reply(&self) -> ErrorReply {
        ErrorReply {
            code: self.code,
            message: self.message.clone(),
            retry_after_ms: None,
        }
    }
}

/// Unwraps `HRESULT_FROM_WIN32` values, which is how errors from the
/// `windows` crate arrive inside `io::Error`.
fn win32_code(code: i32) -> u32 {
    let u = code as u32;
    if u & 0xFFFF_0000 == 0x8007_0000 {
        u & 0xFFFF
    } else {
        u
    }
}

impl From<WinError> for HelperError {
    fn from(e: WinError) -> Self {
        match e.win32_code() {
            Some(code) => Self::from_win32(code, e.to_string()),
            None => Self::new(ErrorCode::Io, e.to_string()),
        }
    }
}

impl From<NtfsError> for HelperError {
    fn from(e: NtfsError) -> Self {
        match &e {
            NtfsError::Io(io) => Self::from_io("volume read failed", io),
            NtfsError::Boot(_) => {
                Self::new(ErrorCode::NotSupported, format!("not an NTFS volume: {e}"))
            }
            NtfsError::Mft(_) | NtfsError::Record { .. } => Self::new(ErrorCode::Io, e.to_string()),
            NtfsError::OutOfRange(_) => Self::new(ErrorCode::NotFound, e.to_string()),
            NtfsError::InvalidOption(_) => Self::bad_request(e.to_string()),
        }
    }
}

impl From<IpcError> for HelperError {
    fn from(e: IpcError) -> Self {
        match e {
            IpcError::Disconnected => Self::disconnected(),
            other => Self::internal(other.to_string()),
        }
    }
}

impl From<&CleanError> for HelperError {
    fn from(e: &CleanError) -> Self {
        let code = match e {
            CleanError::Refused { .. } | CleanError::NeverTier { .. } => ErrorCode::Protected,
            CleanError::Changed { .. } => ErrorCode::Mismatch,
            CleanError::NotFound { .. } => ErrorCode::NotFound,
            CleanError::AccessDenied { .. } => ErrorCode::AccessDenied,
            CleanError::Locked { .. } => ErrorCode::Busy,
            CleanError::Cancelled { .. } => ErrorCode::Cancelled,
            CleanError::Partial { .. } | CleanError::Os { .. } => ErrorCode::Io,
            CleanError::NeedsAcknowledgement { .. }
            | CleanError::NeedsPermanentConfirmation { .. }
            | CleanError::NeedsLargeDeleteConfirmation { .. } => ErrorCode::BadRequest,
            CleanError::AuditLogFailed { .. }
            | CleanError::RecycleBinUnavailable { .. }
            | CleanError::TooLargeForRecycleBin { .. }
            | CleanError::WouldDeletePermanently { .. } => ErrorCode::Internal,
        };
        Self::new(code, e.message())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win32_codes_map_to_categories() {
        assert_eq!(
            HelperError::from_win32(5, "x").code,
            ErrorCode::AccessDenied
        );
        assert_eq!(HelperError::from_win32(2, "x").code, ErrorCode::NotFound);
        assert_eq!(
            HelperError::from_win32(21, "x").code,
            ErrorCode::UnknownVolume
        );
        assert_eq!(
            HelperError::from_win32(1, "x").code,
            ErrorCode::NotSupported
        );
        assert_eq!(HelperError::from_win32(1117, "x").code, ErrorCode::Io);
        let io = io::Error::from_raw_os_error(0x8007_0005_u32 as i32);
        assert_eq!(
            HelperError::from_io("open", &io).code,
            ErrorCode::AccessDenied
        );
    }

    #[test]
    fn ntfs_errors_map_to_categories() {
        assert_eq!(
            HelperError::from(NtfsError::Boot("bad")).code,
            ErrorCode::NotSupported
        );
        assert_eq!(
            HelperError::from(NtfsError::Io(io::Error::from_raw_os_error(5))).code,
            ErrorCode::AccessDenied
        );
    }

    #[test]
    fn disconnect_is_flagged() {
        let e = HelperError::from(IpcError::Disconnected);
        assert!(e.disconnected);
        assert!(!HelperError::from(IpcError::Timeout).disconnected);
    }
}
