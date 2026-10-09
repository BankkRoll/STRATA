//! Pre-flight: right before acting, re-verify each item
//! still exists and is the file the scan saw, look for locks, and check the
//! Recycle Bin can take it.

use serde::{Deserialize, Serialize};
use strata_core::Safety;

use crate::apps::{RunningAppWarning, running_app_warnings};
use crate::canon::CanonicalPath;
use crate::error::CleanError;
use crate::flow::{Acknowledgements, CleanupConfig, QueueItem};
use crate::guard::SafetyGuard;
use crate::locks::{LockHolder, who_locks};
use crate::recycle::fits_shell_path_limit;
use crate::volume::{RecycleBinSupport, RecycleUnavailable, volume_info};

/// Whether an item can go to the Recycle Bin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fit", rename_all = "snake_case")]
pub enum RecycleFit {
    /// Fits.
    Fits,
    /// The volume has not recorded a capacity yet; the delete sink still
    /// stops any permanent delete.
    Unknown,
    /// Larger than the volume's Recycle Bin: ask the user.
    TooLarge {
        /// Capacity in bytes.
        capacity: u64,
    },
    /// No Recycle Bin for this item: ask the user.
    Unavailable {
        /// Why.
        reason: RecycleUnavailable,
    },
}

impl RecycleFit {
    /// Whether recycling would need the user to choose permanent deletion.
    #[must_use]
    pub fn needs_decision(self) -> bool {
        matches!(self, Self::TooLarge { .. } | Self::Unavailable { .. })
    }
}

/// Recycle Bin fit of an item at `resolved` with `size` bytes.
#[must_use]
pub fn recycle_fit(resolved: &CanonicalPath, size: u64) -> RecycleFit {
    if !fits_shell_path_limit(resolved) {
        return RecycleFit::Unavailable {
            reason: RecycleUnavailable::PathTooLong,
        };
    }
    match volume_info(resolved) {
        Err(_) => RecycleFit::Unavailable {
            reason: RecycleUnavailable::UnknownVolume,
        },
        Ok(v) => match v.recycle_bin {
            RecycleBinSupport::Unavailable { reason } => RecycleFit::Unavailable { reason },
            RecycleBinSupport::Available {
                capacity: Some(c), ..
            } if size > c => RecycleFit::TooLarge { capacity: c },
            RecycleBinSupport::Available {
                capacity: Some(_), ..
            } => RecycleFit::Fits,
            RecycleBinSupport::Available { capacity: None, .. } => RecycleFit::Unknown,
        },
    }
}

/// Pre-flight result for one item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Verdict {
    /// Can proceed.
    Ready {
        /// Recycle Bin fit (relevant when recycling).
        recycle: RecycleFit,
    },
    /// Cannot proceed as requested.
    Blocked {
        /// Why.
        error: CleanError,
    },
}

/// Pre-flight result plus context for the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemVerdict {
    /// Queue item id.
    pub id: u64,
    /// Path.
    pub path: String,
    /// Verdict.
    pub verdict: Verdict,
    /// Processes holding the item (empty when free).
    pub holders: Vec<LockHolder>,
    /// "Close X first" warnings.
    pub running_apps: Vec<RunningAppWarning>,
}

/// Runs every pre-flight check on one item.
#[must_use]
pub fn preflight_item(
    guard: &SafetyGuard,
    item: &QueueItem,
    acks: &Acknowledgements,
    cfg: &CleanupConfig,
) -> ItemVerdict {
    let path = item.path.display().to_string();
    let blocked = |error| ItemVerdict {
        id: item.id,
        path: path.clone(),
        verdict: Verdict::Blocked { error },
        holders: Vec::new(),
        running_apps: Vec::new(),
    };
    match item.safety {
        Safety::Never => return blocked(CleanError::NeverTier { path: path.clone() }),
        Safety::Careful if !acks.careful.contains(&item.id) => {
            return blocked(CleanError::NeedsAcknowledgement {
                path: path.clone(),
                tier: Safety::Careful,
            });
        }
        _ => {}
    }
    let checked = match guard.check_path(&item.path) {
        Ok(c) => c,
        Err(e) => return blocked(e),
    };
    if let Err(change) = item.expected.verify(&checked.facts) {
        return blocked(CleanError::Changed {
            path: path.clone(),
            change,
        });
    }
    let holders = who_locks(&item.path, cfg.max_lock_files).unwrap_or_default();
    let running_apps = running_app_warnings(&item.path, &holders).unwrap_or_default();
    let verdict = if holders.is_empty() {
        Verdict::Ready {
            recycle: recycle_fit(&checked.resolved, item.expected.size),
        }
    } else {
        Verdict::Blocked {
            error: CleanError::Locked {
                path: path.clone(),
                holders: holders.clone(),
            },
        }
    };
    ItemVerdict {
        id: item.id,
        path,
        verdict,
        holders,
        running_apps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_needed_only_when_recycling_cannot_work() {
        assert!(!RecycleFit::Fits.needs_decision());
        assert!(!RecycleFit::Unknown.needs_decision());
        assert!(RecycleFit::TooLarge { capacity: 1 }.needs_decision());
        assert!(
            RecycleFit::Unavailable {
                reason: RecycleUnavailable::RemovableDrive
            }
            .needs_decision()
        );
    }

    #[test]
    fn long_paths_cannot_be_recycled() {
        let long = format!(r"D:\{}", "a\\".repeat(200));
        let p = CanonicalPath::parse(long).unwrap();
        assert_eq!(
            recycle_fit(&p, 1),
            RecycleFit::Unavailable {
                reason: RecycleUnavailable::PathTooLong
            }
        );
    }
}
