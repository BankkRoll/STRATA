//! Error type.

use serde::{Deserialize, Serialize};

/// Win32 status codes the session logic reacts to.
pub mod code {
    /// `ERROR_ACCESS_DENIED`: the caller is not elevated (or not in
    /// Performance Log Users).
    pub const ACCESS_DENIED: u32 = 5;
    /// `ERROR_ALREADY_EXISTS`: a session with this name is running.
    pub const ALREADY_EXISTS: u32 = 183;
    /// `ERROR_WMI_INSTANCE_NOT_FOUND`: no session with this name.
    pub const NOT_FOUND: u32 = 4201;
    /// `ERROR_NO_SYSTEM_RESOURCES`: too many sessions are running.
    pub const NO_SYSTEM_RESOURCES: u32 = 1450;
    /// `ERROR_CANCELLED`: `ProcessTrace` ended because the trace was closed.
    pub const CANCELLED: u32 = 1223;
}

/// Errors from starting, running or stopping activity tracking.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum EtwError {
    /// Kernel tracing needs an elevated process.
    #[error("activity tracking needs administrator rights")]
    NotElevated,
    /// A Win32 trace call failed.
    #[error("{op} failed with Win32 error {code}")]
    Win32 {
        /// The failing call.
        op: String,
        /// Win32 status.
        code: u32,
    },
    /// A thread could not be spawned.
    #[error("could not start the {0} thread")]
    Thread(String),
}

impl EtwError {
    /// A [`EtwError::Win32`], mapping `ERROR_ACCESS_DENIED` to
    /// [`EtwError::NotElevated`].
    #[must_use]
    pub fn win32(op: &str, status: u32) -> Self {
        if status == code::ACCESS_DENIED {
            Self::NotElevated
        } else {
            Self::Win32 {
                op: op.to_owned(),
                code: status,
            }
        }
    }
}
