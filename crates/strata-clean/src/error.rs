//! Typed failures with human-readable reasons for the UI.

use serde::{Deserialize, Serialize};
use strata_core::{FileRef, FileTime, Safety};

use crate::locks::LockHolder;
use crate::never::Refusal;
use crate::volume::RecycleUnavailable;

/// What changed between the scan and the action (TOCTOU).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Change {
    /// The path now names a different file (path swap, delete + recreate).
    Identity {
        /// File reference from the scan.
        expected: FileRef,
        /// Identity found now (low 64 bits for NTFS).
        found: u128,
    },
    /// The file's size changed.
    Size {
        /// Size from the scan.
        expected: u64,
        /// Size now.
        found: u64,
    },
    /// The file was modified.
    Modified {
        /// Last-write time from the scan.
        expected: FileTime,
        /// Last-write time now.
        found: FileTime,
    },
    /// A file became a directory or vice versa.
    Kind {
        /// Whether the scan saw a directory.
        expected_dir: bool,
    },
    /// The id came from the fallback walker and was never read from disk.
    SyntheticReference,
    /// The item moved to another volume.
    Volume,
}

impl Change {
    fn describe(&self) -> String {
        match self {
            Self::Identity { .. } => "it is no longer the same file that was scanned".into(),
            Self::Size { expected, found } => {
                format!("its size changed from {expected} to {found} bytes")
            }
            Self::Modified { .. } => "it was modified after the scan".into(),
            Self::Kind { expected_dir: true } => "it is no longer a folder".into(),
            Self::Kind {
                expected_dir: false,
            } => "it is now a folder".into(),
            Self::SyntheticReference => "its identity was never verified; rescan it".into(),
            Self::Volume => "it is on a different drive than when scanned".into(),
        }
    }
}

/// Every way a cleanup step can fail. Serializable for the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[error("{}", self.message())]
pub enum CleanError {
    /// The never-list refused the item.
    Refused {
        /// The refusal.
        refusal: Refusal,
    },
    /// The item no longer exists.
    NotFound {
        /// Path.
        path: String,
    },
    /// The item changed since the scan; re-scan before deleting.
    Changed {
        /// Path.
        path: String,
        /// What changed.
        change: Change,
    },
    /// Another process holds the item open.
    Locked {
        /// Path.
        path: String,
        /// Who holds it, when Restart Manager could tell.
        holders: Vec<LockHolder>,
    },
    /// Windows denied access.
    AccessDenied {
        /// Path.
        path: String,
    },
    /// The volume has no usable Recycle Bin.
    RecycleBinUnavailable {
        /// Path.
        path: String,
        /// Why.
        reason: RecycleUnavailable,
    },
    /// The item is larger than the volume's Recycle Bin. The user must
    /// choose to delete it permanently or skip it; Strata never decides.
    TooLargeForRecycleBin {
        /// Path.
        path: String,
        /// Item size in bytes.
        size: u64,
        /// Recycle Bin capacity for the volume in bytes.
        capacity: u64,
    },
    /// Windows was about to delete the item permanently instead of
    /// recycling it, so Strata stopped. The user must choose.
    WouldDeletePermanently {
        /// Path.
        path: String,
    },
    /// The classifier put the item in the never tier.
    NeverTier {
        /// Path.
        path: String,
    },
    /// Permanent delete was chosen without its extra confirmation.
    NeedsPermanentConfirmation {
        /// Path.
        path: String,
    },
    /// The write-ahead log entry could not be written, so the item was not
    /// touched.
    AuditLogFailed {
        /// Path.
        path: String,
        /// Why.
        message: String,
    },
    /// The item needs an acknowledgement the request did not carry.
    NeedsAcknowledgement {
        /// Path.
        path: String,
        /// Tier that requires it.
        tier: Safety,
    },
    /// Permanent delete of something this large needs a second confirmation.
    NeedsLargeDeleteConfirmation {
        /// Path.
        path: String,
        /// Size in bytes.
        size: u64,
    },
    /// The user cancelled before this item ran.
    Cancelled {
        /// Path.
        path: String,
    },
    /// A folder was only partly deleted (some children failed).
    Partial {
        /// Path.
        path: String,
        /// Children deleted.
        deleted: u64,
        /// First failure.
        first_error: Box<CleanError>,
    },
    /// Any other OS error.
    Os {
        /// Path.
        path: String,
        /// Win32 error code or HRESULT.
        code: i32,
        /// System message.
        message: String,
    },
}

impl CleanError {
    /// Sentence for the UI.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Refused { refusal } => refusal.message(),
            Self::NotFound { path } => format!("{path} no longer exists."),
            Self::Changed { path, change } => {
                format!("{path} was not deleted because {}.", change.describe())
            }
            Self::Locked { path, holders } if holders.is_empty() => {
                format!("{path} is in use by another program.")
            }
            Self::Locked { path, holders } => {
                let names: Vec<String> = holders
                    .iter()
                    .map(|h| format!("{} (PID {})", h.app_name, h.pid))
                    .collect();
                format!("{path} is in use by {}.", names.join(", "))
            }
            Self::AccessDenied { path } => format!("Windows denied access to {path}."),
            Self::RecycleBinUnavailable { path, reason } => format!(
                "{path} cannot go to the Recycle Bin: {}.",
                reason.describe()
            ),
            Self::TooLargeForRecycleBin {
                path,
                size,
                capacity,
            } => format!(
                "{path} ({size} bytes) is larger than this drive's Recycle Bin ({capacity} bytes). Delete it permanently or skip it."
            ),
            Self::WouldDeletePermanently { path } => format!(
                "Windows would have deleted {path} permanently instead of moving it to the Recycle Bin, so Strata stopped. Delete it permanently or skip it."
            ),
            Self::NeverTier { path } => format!(
                "{path} is marked as never safe to delete. Strata offers information and official tools for it instead."
            ),
            Self::NeedsPermanentConfirmation { path } => {
                format!("{path} would be deleted permanently; confirm permanent deletion first.")
            }
            Self::AuditLogFailed { path, message } => format!(
                "{path} was not touched because the undo log could not be written ({message})."
            ),
            Self::NeedsAcknowledgement { path, tier } => {
                format!("{path} is marked {tier:?}; confirm you reviewed it before deleting.")
            }
            Self::NeedsLargeDeleteConfirmation { path, size } => {
                format!("{path} is {size} bytes; confirm again to delete it permanently.")
            }
            Self::Cancelled { path } => {
                format!("{path} was skipped because cleanup was cancelled.")
            }
            Self::Partial {
                path,
                deleted,
                first_error,
            } => format!(
                "{path} was only partly deleted ({deleted} items removed): {}",
                first_error.message()
            ),
            Self::Os {
                path,
                code,
                message,
            } => format!("{path}: {message} (error {code})."),
        }
    }

    /// Whether retrying later could succeed (locks, transient OS errors).
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Locked { .. } | Self::Os { .. } | Self::Partial { .. } | Self::Cancelled { .. }
        )
    }

    pub(crate) fn from_io(path: impl Into<String>, e: &std::io::Error) -> Self {
        let path = path.into();
        match e.raw_os_error().map(win32_code) {
            // ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_BAD_NETPATH,
            // ERROR_INVALID_NAME (verbatim names with odd characters).
            Some(2 | 3 | 53 | 123) => Self::NotFound { path },
            Some(5) => Self::AccessDenied { path },
            // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION,
            // ERROR_USER_MAPPED_FILE.
            Some(32 | 33 | 1224) => Self::Locked {
                path,
                holders: Vec::new(),
            },
            Some(code) => Self::Os {
                path,
                code,
                message: e.to_string(),
            },
            None => Self::Os {
                path,
                code: -1,
                message: e.to_string(),
            },
        }
    }
}

/// Unwraps `HRESULT_FROM_WIN32` values (`0x8007xxxx`), which is how errors
/// from the `windows` crate arrive inside `std::io::Error`.
pub(crate) fn win32_code(code: i32) -> i32 {
    let u = code as u32;
    if u & 0xFFFF_0000 == 0x8007_0000 {
        (u & 0xFFFF) as i32
    } else {
        code
    }
}

impl From<Refusal> for CleanError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused { refusal }
    }
}
