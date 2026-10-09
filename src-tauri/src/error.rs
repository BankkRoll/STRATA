//! The error type every Tauri command returns.
//!
//! Errors cross the IPC boundary as `{ code, message, detail? }` JSON
//! objects. `code` is a stable snake_case category the UI can branch on,
//! `message` is ready to show, and `detail` carries an engine's own typed
//! error (for example a `strata_clean::CleanError`) when there is one. The UI
//! maps `code: "unavailable"` to its designed "not available" state and shows
//! `message` for everything else (`ui/src/lib/backend.ts`).

use serde::Serialize;
use strata_store::{SettingsIssue, StoreError};

/// Machine-readable error category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The feature cannot work in this build or environment (helper binary
    /// missing, store folder unusable, elevation required, ...). Never a bug.
    Unavailable,
    /// The volume, stream, entry, plan item or record is unknown or no
    /// longer valid.
    NotFound,
    /// The request is malformed (bad view name, empty viewport, ...).
    BadRequest,
    /// Something is already running (a scan of the same volume, a cleanup).
    Busy,
    /// The user declined a prompt (UAC).
    Declined,
    /// Operating system or I/O failure.
    Io,
    /// Unexpected internal state (a bug).
    Internal,
    /// A database file is damaged or too new; offer the matching reset.
    StoreCorrupt,
    /// Settings failed validation; `detail` lists the issues.
    InvalidSettings,
    /// Cleanup is not available yet (crash recovery still running) or not
    /// at all (state database damaged).
    NotReady,
    /// The cleanup plan id is unknown or expired.
    UnknownPlan,
    /// Execute was called without a recent pre-flight of the same items.
    PreflightRequired,
    /// The never-list refused an item at the app layer.
    Refused,
    /// A `strata_clean::CleanError` (in `detail`).
    Clean,
    /// A `strata_clean::recycle::RestoreError` (in `detail`).
    Restore,
    /// A `strata_clean::tools::ToolError` (in `detail`).
    Tool,
    /// A `strata_clean::locks::CloseError` (in `detail`).
    CloseApp,
    /// A confirmation prompt is unknown, used, expired or answered too fast.
    Consent,
    /// The helper ran and failed.
    HelperFailed,
    /// The user cancelled a dialog or an operation.
    Cancelled,
}

/// A failed command.
#[derive(Debug, Clone, PartialEq, Serialize, thiserror::Error)]
#[error("{message}")]
pub struct CommandError {
    /// Category.
    pub code: ErrorCode,
    /// User-facing explanation.
    pub message: String,
    /// Typed engine error, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

impl CommandError {
    /// Builds an error.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
        }
    }

    /// An error carrying a serializable engine error as `detail`.
    #[must_use]
    pub fn with_detail(
        code: ErrorCode,
        message: impl Into<String>,
        detail: &impl Serialize,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            detail: serde_json::to_value(detail).ok(),
        }
    }

    /// [`ErrorCode::Unavailable`].
    #[must_use]
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unavailable, message)
    }

    /// [`ErrorCode::NotFound`].
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    /// [`ErrorCode::BadRequest`].
    #[must_use]
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, message)
    }

    /// Shorthand for [`ErrorCode::BadRequest`] (invalid input).
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::bad_request(message)
    }

    /// [`ErrorCode::Io`].
    #[must_use]
    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Io, message)
    }

    /// [`ErrorCode::Io`] from an I/O error, with context.
    #[must_use]
    pub fn io_err(context: &str, e: &std::io::Error) -> Self {
        Self::io(format!("{context}: {e}"))
    }

    /// [`ErrorCode::Internal`].
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }

    /// Settings validation failure; `detail` lists the issues.
    #[must_use]
    pub fn settings(issues: Vec<SettingsIssue>) -> Self {
        let text = issues
            .iter()
            .map(|i| format!("{}: {}", i.key, i.message))
            .collect::<Vec<_>>()
            .join("; ");
        Self::with_detail(
            ErrorCode::InvalidSettings,
            format!("Some settings are invalid: {text}"),
            &issues,
        )
    }
}

impl From<StoreError> for CommandError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::InvalidSettings(issues) => Self::settings(issues),
            StoreError::Corrupt { .. } | StoreError::TooNew { .. } => {
                Self::new(ErrorCode::StoreCorrupt, e.to_string())
            }
            StoreError::Unavailable { .. } => Self::unavailable(e.to_string()),
            StoreError::NotFound(_) => Self::not_found(e.to_string()),
            StoreError::InvalidInput(_) => Self::bad_request(e.to_string()),
            StoreError::Io { .. } => Self::io(e.to_string()),
            other => Self::internal(other.to_string()),
        }
    }
}

impl From<strata_clean::CleanError> for CommandError {
    fn from(e: strata_clean::CleanError) -> Self {
        Self::with_detail(ErrorCode::Clean, e.message(), &e)
    }
}

/// Result of a command.
pub type CmdResult<T> = Result<T, CommandError>;

/// Runs blocking work (index reads, store calls, Win32, the Shell) on
/// Tauri's blocking pool, never on the IPC thread.
///
/// # Errors
///
/// The closure's error, or [`ErrorCode::Internal`] if it panicked.
pub async fn blocking<T, F>(f: F) -> CmdResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> CmdResult<T> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| CommandError::internal(format!("background task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_as_code_and_message() {
        let e = CommandError::unavailable("no helper");
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"code":"unavailable","message":"no helper"}"#
        );
    }

    #[test]
    fn serializes_detail_when_present() {
        let e = CommandError::settings(vec![SettingsIssue {
            key: "live.update_tick_ms".into(),
            message: "too small".into(),
        }]);
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["code"], "invalid_settings");
        assert!(
            v["message"]
                .as_str()
                .unwrap()
                .contains("live.update_tick_ms")
        );
        assert_eq!(v["detail"][0]["key"], "live.update_tick_ms");
    }
}
