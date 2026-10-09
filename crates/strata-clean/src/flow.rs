//! The cleanup flow the app calls (SPEC §15.2):
//!
//! 1. [`plan`]: dedupe the queue, drop never-list hits, total by tier,
//!    report Recycle Bin availability and running-app warnings. Cheap.
//! 2. [`preflight`]: per-item TOCTOU re-verification, locks, Recycle Bin
//!    fit. Run right before acting.
//! 3. [`execute`]: gate every item (tier, acknowledgements, permanent and
//!    large-delete confirmations), write the audit log **before** each item
//!    acts, act, record the outcome. Never falls back from recycling to a
//!    permanent delete on its own.
//! 4. [`Plan::retry`]: a plan of the items worth retrying.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use strata_core::{FileTime, Safety};

use crate::apps::{RunningAppWarning, cache_owners};
pub use crate::audit::DeleteMethod;
use crate::audit::{ActionId, ActionSummary, AuditAction, AuditItem, AuditLog, ItemOutcome};
use crate::canon::CanonicalPath;
use crate::error::CleanError;
use crate::expect::{CancelToken, Expected};
use crate::guard::SafetyGuard;
use crate::locks::who_locks_files;
use crate::never::Refusal;
use crate::permanent::delete_permanently;
use crate::preflight::{ItemVerdict, RecycleFit, preflight_item};
use crate::recycle::{RecycleItem, fits_shell_path_limit, recycle};
use crate::volume::{RecycleBinSupport, RecycleUnavailable, volume_info};

/// One queued item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    /// Caller's id (stable across plan, pre-flight and execute).
    pub id: u64,
    /// Path as shown to the user.
    pub path: PathBuf,
    /// What the scan saw.
    pub expected: Expected,
    /// Safety tier from the classifier.
    pub safety: Safety,
}

/// Tunables. Thresholds are confirmed by the UI, not decided here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupConfig {
    /// Permanent deletes above this many bytes need a second confirmation.
    pub large_permanent_bytes: u64,
    /// Files registered with Restart Manager per folder.
    pub max_lock_files: usize,
    /// Items per `IFileOperation` batch.
    pub recycle_batch: usize,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            large_permanent_bytes: 10 * 1024 * 1024 * 1024,
            max_lock_files: crate::locks::DEFAULT_MAX_FILES,
            recycle_batch: 32,
        }
    }
}

/// What the user explicitly confirmed in the review screen.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Acknowledgements {
    /// "Careful" items the user ticked.
    pub careful: BTreeSet<u64>,
    /// The extra confirmation for permanent deletion.
    pub permanent: bool,
    /// The second confirmation for permanent deletes above the threshold.
    pub large_permanent: bool,
    /// Items that cannot be recycled (too large, no Recycle Bin, path too
    /// long) which the user chose to delete permanently instead.
    pub permanent_instead_of_recycle: BTreeSet<u64>,
}

/// The method and confirmations the user chose in the review screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    /// Recycle Bin (default) or permanent.
    pub method: DeleteMethod,
    /// Confirmations.
    pub acks: Acknowledgements,
}

impl Decision {
    /// Recycle with no extra confirmations.
    #[must_use]
    pub fn recycle() -> Self {
        Self {
            method: DeleteMethod::RecycleBin,
            acks: Acknowledgements::default(),
        }
    }
}

/// Count and bytes for one tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierTotal {
    /// Tier.
    pub safety: Safety,
    /// Items.
    pub items: u64,
    /// Bytes.
    pub bytes: u64,
}

/// Items on one volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanVolume {
    /// Mount point.
    pub mount_point: String,
    /// Recycle Bin support.
    pub recycle_bin: RecycleBinSupport,
    /// Items.
    pub items: u64,
    /// Bytes.
    pub bytes: u64,
}

/// Something the review screen should show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanWarning {
    /// Removed: the never-list refuses it.
    Refused {
        /// Item id.
        id: u64,
        /// The refusal.
        refusal: Refusal,
    },
    /// Removed: the same item is queued twice.
    Duplicate {
        /// Item id.
        id: u64,
    },
    /// Removed: inside another queued folder, which covers it.
    Nested {
        /// Item id.
        id: u64,
        /// The containing item.
        inside: u64,
    },
    /// Never tier: will not be deleted.
    NeverTier {
        /// Item id.
        id: u64,
    },
    /// Careful tier: needs a tick.
    NeedsAcknowledgement {
        /// Item id.
        id: u64,
    },
    /// Cannot be recycled; the user must choose permanent delete or skip.
    CannotRecycle {
        /// Item id.
        id: u64,
        /// Fit.
        fit: RecycleFit,
    },
    /// An app that owns this cache is running.
    RunningApp {
        /// Item id.
        id: u64,
        /// The warning.
        warning: RunningAppWarning,
    },
}

/// The reviewed plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// Items that will be considered by pre-flight and execute.
    pub items: Vec<QueueItem>,
    /// Totals by tier over `items`.
    pub totals: Vec<TierTotal>,
    /// Per-volume Recycle Bin availability.
    pub volumes: Vec<PlanVolume>,
    /// Warnings for the review screen.
    pub warnings: Vec<PlanWarning>,
}

impl Plan {
    /// Total bytes of all items.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.items.iter().map(|i| i.expected.size).sum()
    }

    /// A plan of the items in `report` worth retrying (locks, transient OS
    /// errors, cancellations). Run [`preflight`] again before executing it.
    #[must_use]
    pub fn retry(&self, report: &ExecutionReport) -> Self {
        let retry: BTreeSet<u64> = report
            .results
            .iter()
            .filter(|r| match &r.outcome {
                ItemOutcome::Failed { error } | ItemOutcome::Skipped { reason: error } => {
                    error.is_retryable()
                }
                _ => false,
            })
            .map(|r| r.id)
            .collect();
        let items: Vec<QueueItem> = self
            .items
            .iter()
            .filter(|i| retry.contains(&i.id))
            .cloned()
            .collect();
        Self {
            totals: totals(&items),
            volumes: self.volumes.clone(),
            warnings: Vec::new(),
            items,
        }
    }
}

fn totals(items: &[QueueItem]) -> Vec<TierTotal> {
    [
        Safety::Safe,
        Safety::Probably,
        Safety::Careful,
        Safety::Never,
    ]
    .into_iter()
    .map(|s| {
        let of: Vec<&QueueItem> = items.iter().filter(|i| i.safety == s).collect();
        TierTotal {
            safety: s,
            items: of.len() as u64,
            bytes: of.iter().map(|i| i.expected.size).sum(),
        }
    })
    .collect()
}

/// Builds the review plan. No file is opened for writing.
#[must_use]
pub fn plan(guard: &SafetyGuard, queue: Vec<QueueItem>) -> Plan {
    let mut warnings = Vec::new();
    let mut kept: Vec<(QueueItem, CanonicalPath)> = Vec::new();
    for item in queue {
        let canon = match guard.never_list().check_str(item.path.as_os_str()) {
            Ok(c) => c,
            Err(refusal) => {
                warnings.push(PlanWarning::Refused {
                    id: item.id,
                    refusal,
                });
                continue;
            }
        };
        if kept.iter().any(|(_, c)| *c == canon) {
            warnings.push(PlanWarning::Duplicate { id: item.id });
            continue;
        }
        kept.push((item, canon));
    }
    // Drop items inside other queued folders.
    let mut items = Vec::new();
    for (item, canon) in &kept {
        if let Some((outer, _)) = kept.iter().find(|(_, c)| c.is_ancestor_of(canon)) {
            warnings.push(PlanWarning::Nested {
                id: item.id,
                inside: outer.id,
            });
        } else {
            items.push((item.clone(), canon.clone()));
        }
    }

    let mut volumes: Vec<PlanVolume> = Vec::new();
    for (item, canon) in &items {
        match item.safety {
            Safety::Never => warnings.push(PlanWarning::NeverTier { id: item.id }),
            Safety::Careful => warnings.push(PlanWarning::NeedsAcknowledgement { id: item.id }),
            _ => {}
        }
        let vi = volume_info(canon).ok();
        let support = vi.as_ref().map_or(
            RecycleBinSupport::Unavailable {
                reason: RecycleUnavailable::UnknownVolume,
            },
            |v| v.recycle_bin.clone(),
        );
        let mount = vi
            .as_ref()
            .map_or_else(|| canon.to_string(), |v| v.mount_point.clone());
        match volumes.iter_mut().find(|v| v.mount_point == mount) {
            Some(v) => {
                v.items += 1;
                v.bytes += item.expected.size;
            }
            None => volumes.push(PlanVolume {
                mount_point: mount,
                recycle_bin: support.clone(),
                items: 1,
                bytes: item.expected.size,
            }),
        }
        let fit = if !fits_shell_path_limit(canon) {
            RecycleFit::Unavailable {
                reason: RecycleUnavailable::PathTooLong,
            }
        } else {
            match support {
                RecycleBinSupport::Unavailable { reason } => RecycleFit::Unavailable { reason },
                RecycleBinSupport::Available {
                    capacity: Some(c), ..
                } if item.expected.size > c => RecycleFit::TooLarge { capacity: c },
                RecycleBinSupport::Available { .. } => RecycleFit::Fits,
            }
        };
        if fit.needs_decision() {
            warnings.push(PlanWarning::CannotRecycle { id: item.id, fit });
        }
        if !cache_owners(&item.path).is_empty()
            && let Ok(ws) = crate::apps::running_app_warnings(&item.path, &[])
        {
            for warning in ws {
                warnings.push(PlanWarning::RunningApp {
                    id: item.id,
                    warning,
                });
            }
        }
    }
    let items: Vec<QueueItem> = items.into_iter().map(|(i, _)| i).collect();
    Plan {
        totals: totals(&items),
        items,
        volumes,
        warnings,
    }
}

/// Pre-flight for every planned item.
#[must_use]
pub fn preflight(
    guard: &SafetyGuard,
    plan: &Plan,
    acks: &Acknowledgements,
    cfg: &CleanupConfig,
) -> Vec<ItemVerdict> {
    plan.items
        .iter()
        .map(|i| preflight_item(guard, i, acks, cfg))
        .collect()
}

/// Progress events for the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Progress {
    /// Execution began.
    Started {
        /// Items considered.
        items: usize,
        /// Their bytes.
        bytes: u64,
    },
    /// An item is about to be acted on (its log entry is written).
    ItemStarted {
        /// Item id.
        id: u64,
    },
    /// An item finished.
    ItemFinished {
        /// Item id.
        id: u64,
        /// Whether it was removed.
        removed: bool,
    },
    /// Execution ended.
    Finished {
        /// Summary.
        summary: ActionSummary,
    },
}

/// Outcome of one item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemResult {
    /// Item id.
    pub id: u64,
    /// Path.
    pub path: String,
    /// Outcome.
    pub outcome: ItemOutcome,
}

/// Result of [`execute`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionReport {
    /// Audit action id, when the log accepted the action.
    pub action: Option<ActionId>,
    /// Per-item outcomes, in plan order.
    pub results: Vec<ItemResult>,
    /// Totals.
    pub summary: ActionSummary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Recycle,
    Permanent,
}

fn gate(
    item: &QueueItem,
    method: DeleteMethod,
    acks: &Acknowledgements,
    cfg: &CleanupConfig,
) -> Result<Route, CleanError> {
    let path = item.path.display().to_string();
    match item.safety {
        Safety::Never => return Err(CleanError::NeverTier { path }),
        Safety::Careful if !acks.careful.contains(&item.id) => {
            return Err(CleanError::NeedsAcknowledgement {
                path,
                tier: Safety::Careful,
            });
        }
        _ => {}
    }
    let route = match method {
        DeleteMethod::RecycleBin if acks.permanent_instead_of_recycle.contains(&item.id) => {
            Route::Permanent
        }
        DeleteMethod::RecycleBin => Route::Recycle,
        DeleteMethod::Permanent if !acks.permanent => {
            return Err(CleanError::NeedsPermanentConfirmation { path });
        }
        DeleteMethod::Permanent => Route::Permanent,
    };
    if route == Route::Permanent
        && item.expected.size > cfg.large_permanent_bytes
        && !acks.large_permanent
    {
        return Err(CleanError::NeedsLargeDeleteConfirmation {
            path,
            size: item.expected.size,
        });
    }
    Ok(route)
}

fn audit_item(i: &QueueItem) -> AuditItem {
    AuditItem {
        item_id: i.id,
        path: i.path.display().to_string(),
        file_ref: i.expected.file_ref,
        size: i.expected.size,
        safety: i.safety,
    }
}

fn now() -> FileTime {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    FileTime::from_unix_secs(i64::try_from(secs).unwrap_or(i64::MAX))
}

fn with_lock_holders(item: &QueueItem, e: CleanError, cfg: &CleanupConfig) -> CleanError {
    match e {
        CleanError::Locked { path, holders } if holders.is_empty() => {
            let files = crate::locks::lockable_files(&item.path, cfg.max_lock_files);
            CleanError::Locked {
                path,
                holders: who_locks_files(&files).unwrap_or_default(),
            }
        }
        other => other,
    }
}

/// Executes a plan. See the module docs.
///
/// Items fail individually with typed reasons; nothing is retried
/// implicitly except Shell operations that stopped early because of an
/// unrelated item.
pub fn execute(
    guard: &SafetyGuard,
    plan: &Plan,
    decision: &Decision,
    cfg: &CleanupConfig,
    audit: &mut dyn AuditLog,
    progress: &mut dyn FnMut(Progress),
    cancel: &CancelToken,
) -> ExecutionReport {
    let (method, acks) = (decision.method, &decision.acks);
    let mut summary = ActionSummary::default();
    let mut results: Vec<Option<ItemOutcome>> = vec![None; plan.items.len()];
    progress(Progress::Started {
        items: plan.items.len(),
        bytes: plan.total_bytes(),
    });

    let action = AuditAction {
        method,
        started_at: now(),
        items: plan.items.iter().map(audit_item).collect(),
    };
    let action_id = match audit.begin_action(&action) {
        Ok(id) => id,
        Err(e) => {
            // Without a write-ahead record nothing may be touched.
            let results = plan
                .items
                .iter()
                .map(|i| ItemResult {
                    id: i.id,
                    path: i.path.display().to_string(),
                    outcome: ItemOutcome::Skipped {
                        reason: CleanError::AuditLogFailed {
                            path: i.path.display().to_string(),
                            message: e.0.clone(),
                        },
                    },
                })
                .collect::<Vec<_>>();
            summary.skipped = results.len() as u64;
            progress(Progress::Finished { summary });
            return ExecutionReport {
                action: None,
                results,
                summary,
            };
        }
    };

    let mut recycle_idx = Vec::new();
    let mut permanent_idx = Vec::new();
    for (i, item) in plan.items.iter().enumerate() {
        match gate(item, method, acks, cfg) {
            Ok(Route::Recycle) => recycle_idx.push(i),
            Ok(Route::Permanent) => permanent_idx.push(i),
            Err(reason) => results[i] = Some(ItemOutcome::Skipped { reason }),
        }
    }

    let finish = |audit: &mut dyn AuditLog,
                  progress: &mut dyn FnMut(Progress),
                  item: &QueueItem,
                  outcome: ItemOutcome| {
        // The start record already marks the item, so a failed finish
        // record leaves it "interrupted" rather than unrecorded.
        let _ = audit.item_finished(action_id, item.id, &outcome);
        progress(Progress::ItemFinished {
            id: item.id,
            removed: outcome.removed(),
        });
        outcome
    };
    let start = |audit: &mut dyn AuditLog,
                 progress: &mut dyn FnMut(Progress),
                 item: &QueueItem|
     -> Result<(), CleanError> {
        audit
            .item_started(action_id, &audit_item(item))
            .map_err(|e| CleanError::AuditLogFailed {
                path: item.path.display().to_string(),
                message: e.0,
            })?;
        progress(Progress::ItemStarted { id: item.id });
        Ok(())
    };

    for batch in recycle_idx.chunks(cfg.recycle_batch.max(1)) {
        let mut ready = Vec::new();
        for &i in batch {
            if cancel.is_cancelled() {
                results[i] = Some(ItemOutcome::Skipped {
                    reason: CleanError::Cancelled {
                        path: plan.items[i].path.display().to_string(),
                    },
                });
                continue;
            }
            match start(audit, progress, &plan.items[i]) {
                Ok(()) => ready.push(i),
                Err(reason) => results[i] = Some(ItemOutcome::Skipped { reason }),
            }
        }
        if ready.is_empty() {
            continue;
        }
        let items: Vec<RecycleItem> = ready
            .iter()
            .map(|&i| RecycleItem {
                path: plan.items[i].path.clone(),
                expected: plan.items[i].expected,
            })
            .collect();
        let outcomes = recycle(guard, &items, cancel);
        for (&i, r) in ready.iter().zip(outcomes) {
            let outcome = match r {
                Ok(ticket) => ItemOutcome::Recycled { ticket },
                Err(e) => ItemOutcome::Failed {
                    error: with_lock_holders(&plan.items[i], e, cfg),
                },
            };
            results[i] = Some(finish(audit, progress, &plan.items[i], outcome));
        }
    }

    for &i in &permanent_idx {
        let item = &plan.items[i];
        if cancel.is_cancelled() {
            results[i] = Some(ItemOutcome::Skipped {
                reason: CleanError::Cancelled {
                    path: item.path.display().to_string(),
                },
            });
            continue;
        }
        if let Err(reason) = start(audit, progress, item) {
            results[i] = Some(ItemOutcome::Skipped { reason });
            continue;
        }
        let outcome = match delete_permanently(guard, &item.path, &item.expected, cancel) {
            Ok(stats) => ItemOutcome::Deleted { stats },
            Err(e) => ItemOutcome::Failed {
                error: with_lock_holders(item, e, cfg),
            },
        };
        results[i] = Some(finish(audit, progress, item, outcome));
    }

    let results: Vec<ItemResult> = plan
        .items
        .iter()
        .zip(results)
        .map(|(item, outcome)| {
            let outcome = outcome.unwrap_or_else(|| ItemOutcome::Skipped {
                reason: CleanError::Cancelled {
                    path: item.path.display().to_string(),
                },
            });
            match &outcome {
                o if o.removed() => {
                    summary.succeeded += 1;
                    summary.bytes += item.expected.size;
                }
                ItemOutcome::Failed { .. } => summary.failed += 1,
                _ => summary.skipped += 1,
            }
            ItemResult {
                id: item.id,
                path: item.path.display().to_string(),
                outcome,
            }
        })
        .collect();
    summary.cancelled = cancel.is_cancelled();
    let _ = audit.finish_action(action_id, &summary);
    progress(Progress::Finished { summary });
    ExecutionReport {
        action: Some(action_id),
        results,
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::FileRef;

    fn qi(id: u64, safety: Safety, size: u64) -> QueueItem {
        QueueItem {
            id,
            path: format!(r"D:\x\{id}").into(),
            expected: Expected {
                file_ref: FileRef(id),
                is_dir: false,
                size,
                modified: FileTime(0),
            },
            safety,
        }
    }

    #[test]
    fn gates() {
        let cfg = CleanupConfig {
            large_permanent_bytes: 100,
            ..Default::default()
        };
        let mut acks = Acknowledgements::default();
        let r = DeleteMethod::RecycleBin;
        let p = DeleteMethod::Permanent;
        assert!(matches!(
            gate(&qi(1, Safety::Never, 1), r, &acks, &cfg),
            Err(CleanError::NeverTier { .. })
        ));
        assert!(matches!(
            gate(&qi(2, Safety::Careful, 1), r, &acks, &cfg),
            Err(CleanError::NeedsAcknowledgement { .. })
        ));
        acks.careful.insert(2);
        assert_eq!(
            gate(&qi(2, Safety::Careful, 1), r, &acks, &cfg),
            Ok(Route::Recycle)
        );
        assert!(matches!(
            gate(&qi(3, Safety::Safe, 1), p, &acks, &cfg),
            Err(CleanError::NeedsPermanentConfirmation { .. })
        ));
        acks.permanent = true;
        assert_eq!(
            gate(&qi(3, Safety::Safe, 1), p, &acks, &cfg),
            Ok(Route::Permanent)
        );
        assert!(matches!(
            gate(&qi(4, Safety::Safe, 101), p, &acks, &cfg),
            Err(CleanError::NeedsLargeDeleteConfirmation { .. })
        ));
        acks.large_permanent = true;
        assert_eq!(
            gate(&qi(4, Safety::Safe, 101), p, &acks, &cfg),
            Ok(Route::Permanent)
        );
        // Recycle never turns permanent unless the user chose it per item.
        assert_eq!(
            gate(&qi(5, Safety::Safe, 1), r, &acks, &cfg),
            Ok(Route::Recycle)
        );
        acks.permanent_instead_of_recycle.insert(5);
        assert_eq!(
            gate(&qi(5, Safety::Safe, 1), r, &acks, &cfg),
            Ok(Route::Permanent)
        );
    }

    #[test]
    fn totals_by_tier() {
        let t = totals(&[
            qi(1, Safety::Safe, 5),
            qi(2, Safety::Safe, 7),
            qi(3, Safety::Careful, 1),
        ]);
        assert_eq!(
            t[0],
            TierTotal {
                safety: Safety::Safe,
                items: 2,
                bytes: 12
            }
        );
        assert_eq!(
            t[2],
            TierTotal {
                safety: Safety::Careful,
                items: 1,
                bytes: 1
            }
        );
        assert_eq!(t[3].items, 0);
    }
}
