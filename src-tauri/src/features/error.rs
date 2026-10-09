//! The one error type every shell-feature command returns.
//!
//! Commands reject with `{ kind, message, detail? }`: `kind` is a stable
//! snake_case code the UI can branch on, `message` is ready to show, and
//! `detail` carries the engine's own typed error (for example a
//! `strata_clean::CleanError`) when there is one.

use serde::Serialize;
use strata_store::{SettingsIssue, StoreError};

/// Stable error codes for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The store could not be opened at all.
    StoreUnavailable,
    /// A database file is damaged or too new; offer the matching reset.
    StoreCorrupt,
    /// Settings failed validation; `detail` lists the issues.
    InvalidSettings,
    /// A request argument was malformed.
    InvalidInput,
    /// Cleanup is not available yet (crash recovery still running) or not
    /// at all (state database damaged).
    NotReady,
    /// The plan id is unknown or expired.
    UnknownPlan,
    /// `cleanup_execute` was called without a recent pre-flight.
    PreflightRequired,
    /// Another cleanup is running.
    Busy,
    /// The never-list refused an item at the app layer.
    Refused,
    /// A `strata_clean::CleanError`.
    Clean,
    /// A `strata_clean::recycle::RestoreError`.
    Restore,
    /// A `strata_clean::tools::ToolError`.
    Tool,
    /// A `strata_clean::locks::CloseError`.
    CloseApp,
    /// The confirmation token is unknown, used, expired or too fast.
    Consent,
    /// `strata-helper.exe` is not installed next to the app.
    HelperMissing,
    /// The user declined the UAC prompt.
    HelperDeclined,
    /// The helper ran and failed.
    HelperFailed,
    /// The feature is not available in this build or on this machine.
    Unsupported,
    /// File-system or OS failure.
    Io,
    /// The user cancelled a dialog.
    Cancelled,
    /// A referenced record does not exist.
    NotFound,
    /// Anything else (a bug).
    Internal,
}

/// Error returned by every shell-feature command.
#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[error("{message}")]
pub struct FeatureError {
    /// Stable code.
    pub kind: ErrorKind,
    /// Human-readable text.
    pub message: String,
    /// Typed engine error, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

impl FeatureError {
    /// An error without detail.
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            detail: None,
        }
    }

    /// An error carrying a serializable engine error as `detail`.
    #[must_use]
    pub fn with_detail(
        kind: ErrorKind,
        message: impl Into<String>,
        detail: &impl Serialize,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            detail: serde_json::to_value(detail).ok(),
        }
    }

    /// Shorthand for [`ErrorKind::Internal`].
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }

    /// Shorthand for [`ErrorKind::InvalidInput`].
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidInput, message)
    }

    /// Shorthand for [`ErrorKind::Io`].
    #[must_use]
    pub fn io(context: &str, e: &std::io::Error) -> Self {
        Self::new(ErrorKind::Io, format!("{context}: {e}"))
    }

    /// Settings validation failure.
    #[must_use]
    pub fn settings(issues: Vec<SettingsIssue>) -> Self {
        let text = issues
            .iter()
            .map(|i| format!("{}: {}", i.key, i.message))
            .collect::<Vec<_>>()
            .join("; ");
        Self::with_detail(
            ErrorKind::InvalidSettings,
            format!("Some settings are invalid: {text}"),
            &issues,
        )
    }
}

impl From<StoreError> for FeatureError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::InvalidSettings(issues) => Self::settings(issues),
            StoreError::Corrupt { .. } | StoreError::TooNew { .. } => {
                Self::new(ErrorKind::StoreCorrupt, e.to_string())
            }
            StoreError::Unavailable { .. } => Self::new(ErrorKind::StoreUnavailable, e.to_string()),
            StoreError::NotFound(_) => Self::new(ErrorKind::NotFound, e.to_string()),
            StoreError::InvalidInput(_) => Self::new(ErrorKind::InvalidInput, e.to_string()),
            StoreError::Io { .. } => Self::new(ErrorKind::Io, e.to_string()),
            other => Self::internal(other.to_string()),
        }
    }
}

impl From<strata_clean::CleanError> for FeatureError {
    fn from(e: strata_clean::CleanError) -> Self {
        Self::with_detail(ErrorKind::Clean, e.message(), &e)
    }
}

/// Result alias for shell features.
pub type FeatureResult<T> = Result<T, FeatureError>;

/// Runs blocking work (store calls, Win32, the Shell) off the IPC thread.
///
/// # Errors
///
/// The closure's error, or [`ErrorKind::Internal`] if it panicked.
pub async fn blocking<T, F>(f: F) -> FeatureResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> FeatureResult<T> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| FeatureError::internal(format!("background task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_kind_message_and_detail() {
        let e = FeatureError::settings(vec![SettingsIssue {
            key: "live.update_tick_ms".into(),
            message: "too small".into(),
        }]);
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], "invalid_settings");
        assert!(
            v["message"]
                .as_str()
                .unwrap()
                .contains("live.update_tick_ms")
        );
        assert_eq!(v["detail"][0]["key"], "live.update_tick_ms");
        let v = serde_json::to_value(FeatureError::internal("x")).unwrap();
        assert!(v.get("detail").is_none());
    }
}
