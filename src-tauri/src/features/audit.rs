//! `strata_clean::audit::AuditLog` over `strata-store`'s undo log, and the
//! crash-recovery pass for actions left in progress.
//!
//! # Write-ahead mapping
//!
//! | `AuditLog` call | Store call | Durability |
//! |---|---|---|
//! | `begin_action` | `begin_action` with **every** item `pending` | committed + fsynced (`state.db` is `synchronous=FULL`) |
//! | `item_started` | none: the item's `pending` row already exists | checked: the item must be in the committed action and `state.db` healthy |
//! | `item_finished` | `complete_item` | committed |
//! | `finish_action` | `finish_action` with a status from the summary | committed |
//!
//! Recording every item before the first one is touched is stricter than the
//! per-item contract: after a crash, every item that might have been acted
//! on is already in the log. [`recover`] then decides per `pending` item
//! whether it is still in place, in the Recycle Bin (a Restore ticket is
//! recovered) or gone.

use std::collections::HashMap;
use std::path::Path;

use strata_clean::audit::{
    ActionId as CleanActionId, ActionSummary as CleanSummary, AuditAction, AuditError, AuditItem,
    AuditLog, DeleteMethod as CleanMethod, ItemOutcome as CleanOutcome,
};
use strata_clean::canon::CanonicalPath;
use strata_clean::flow::{Decision, Plan};
use strata_clean::recycle::{RestoreTicket, find_in_recycle_bin};
use strata_core::FileTime;
use strata_store::{
    ActionId, ActionKind, ActionRecord, ActionStatus, DbHealth, DeleteMethod, ItemOutcome,
    ItemRecord, ItemResult, PlannedItem, RestoreInfo, Store, Timestamp, VolumeKey,
};

// -----------------------------------------------------------------------------
// Adapter
// -----------------------------------------------------------------------------

/// What the store needs per item beyond [`AuditItem`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemMeta {
    /// Last-write time verified in pre-flight.
    pub mtime: FileTime,
    /// Volume holding the item.
    pub volume: VolumeKey,
    /// Method this item will actually use.
    pub method: DeleteMethod,
    /// Classifier rule, if known.
    pub rule_id: Option<String>,
}

/// [`AuditLog`] backed by the store's undo log. One instance per action.
#[derive(Debug)]
pub struct StoreAuditLog {
    store: Store,
    kind: ActionKind,
    meta: HashMap<u64, ItemMeta>,
    current: Option<(ActionId, HashMap<u64, u32>)>,
}

impl StoreAuditLog {
    /// A log for one action whose items are described by `meta` (keyed by
    /// queue item id).
    #[must_use]
    pub fn new(store: Store, kind: ActionKind, meta: HashMap<u64, ItemMeta>) -> Self {
        Self {
            store,
            kind,
            meta,
            current: None,
        }
    }

    /// A cleanup log for `plan` executed with `decision`; volumes are looked
    /// up once per mount point.
    #[must_use]
    pub fn for_plan(store: Store, plan: &Plan, decision: &Decision) -> Self {
        let mut volumes = VolumeCache::default();
        let meta = plan
            .items
            .iter()
            .map(|i| {
                let method = match decision.method {
                    CleanMethod::RecycleBin
                        if !decision.acks.permanent_instead_of_recycle.contains(&i.id) =>
                    {
                        DeleteMethod::Recycle
                    }
                    _ => DeleteMethod::Permanent,
                };
                (
                    i.id,
                    ItemMeta {
                        mtime: i.expected.modified,
                        volume: volumes.key_for(&i.path),
                        method,
                        rule_id: None,
                    },
                )
            })
            .collect();
        Self::new(store, ActionKind::Cleanup, meta)
    }

    /// The store action id of the running action.
    #[must_use]
    pub fn store_action(&self) -> Option<ActionId> {
        self.current.as_ref().map(|(id, _)| *id)
    }

    fn seq_of(&self, action: CleanActionId, item_id: u64) -> Result<(ActionId, u32), AuditError> {
        let (id, seqs) = self
            .current
            .as_ref()
            .ok_or_else(|| AuditError("no action has begun".into()))?;
        if to_clean_id(*id) != action {
            return Err(AuditError(format!("unknown action {}", action.0)));
        }
        let seq = seqs
            .get(&item_id)
            .ok_or_else(|| AuditError(format!("item {item_id} was not written ahead")))?;
        Ok((*id, *seq))
    }
}

fn to_clean_id(id: ActionId) -> CleanActionId {
    CleanActionId(u64::try_from(id.0).unwrap_or(0))
}

/// Maps a clean outcome to the store's record.
#[must_use]
pub fn store_outcome(outcome: &CleanOutcome) -> ItemOutcome {
    match outcome {
        CleanOutcome::Recycled { ticket } => ItemOutcome {
            result: ItemResult::Done,
            error: None,
            restore: Some(RestoreInfo {
                original_path: ticket.original_path.clone(),
                blob: ticket.to_blob(),
            }),
        },
        CleanOutcome::Deleted { .. } => ItemOutcome {
            result: ItemResult::Done,
            error: None,
            restore: None,
        },
        CleanOutcome::Failed { error } => ItemOutcome {
            result: ItemResult::Failed,
            error: Some(error.message()),
            restore: None,
        },
        CleanOutcome::Skipped { reason } => ItemOutcome {
            result: ItemResult::Skipped,
            error: Some(reason.message()),
            restore: None,
        },
    }
}

/// The final action status for a summary.
#[must_use]
pub fn final_status(summary: &CleanSummary) -> ActionStatus {
    if summary.cancelled {
        ActionStatus::Cancelled
    } else if summary.failed > 0 && summary.succeeded > 0 {
        ActionStatus::Partial
    } else if summary.failed > 0 {
        ActionStatus::Failed
    } else {
        ActionStatus::Completed
    }
}

impl AuditLog for StoreAuditLog {
    fn begin_action(&mut self, action: &AuditAction) -> Result<CleanActionId, AuditError> {
        if self.current.is_some() {
            return Err(AuditError("this log already holds an action".into()));
        }
        let mut seqs = HashMap::with_capacity(action.items.len());
        let mut planned = Vec::with_capacity(action.items.len());
        for (seq, item) in action.items.iter().enumerate() {
            let meta = self
                .meta
                .get(&item.item_id)
                .ok_or_else(|| AuditError(format!("item {} is not in the plan", item.item_id)))?;
            let seq = u32::try_from(seq).map_err(|_| AuditError("too many items".into()))?;
            seqs.insert(item.item_id, seq);
            planned.push(planned_item(item, meta));
        }
        let id = self
            .store
            .begin_action(self.kind, &planned)
            .map_err(|e| AuditError(e.to_string()))?;
        self.current = Some((id, seqs));
        Ok(to_clean_id(id))
    }

    fn item_started(&mut self, action: CleanActionId, item: &AuditItem) -> Result<(), AuditError> {
        self.seq_of(action, item.item_id)?;
        // The pending row was committed by begin_action; refuse to act if the
        // database has since gone bad, because the outcome could not be
        // recorded.
        match self.store.health().state {
            DbHealth::Ok => Ok(()),
            other => Err(AuditError(format!(
                "the undo log is unavailable: {other:?}"
            ))),
        }
    }

    fn item_finished(
        &mut self,
        action: CleanActionId,
        item_id: u64,
        outcome: &CleanOutcome,
    ) -> Result<(), AuditError> {
        let (id, seq) = self.seq_of(action, item_id)?;
        self.store
            .complete_item(id, seq, &store_outcome(outcome))
            .map_err(|e| AuditError(e.to_string()))
    }

    fn finish_action(
        &mut self,
        action: CleanActionId,
        summary: &CleanSummary,
    ) -> Result<(), AuditError> {
        let (id, _) = self
            .current
            .as_ref()
            .ok_or_else(|| AuditError("no action has begun".into()))?;
        if to_clean_id(*id) != action {
            return Err(AuditError(format!("unknown action {}", action.0)));
        }
        self.store
            .finish_action(*id, final_status(summary))
            .map_err(|e| AuditError(e.to_string()))
    }
}

fn planned_item(item: &AuditItem, meta: &ItemMeta) -> PlannedItem {
    PlannedItem {
        path: item.path.clone(),
        volume: meta.volume.clone(),
        file_ref: item.file_ref,
        size: item.size,
        mtime: meta.mtime,
        method: meta.method,
        tier: item.safety,
        rule_id: meta.rule_id.clone(),
    }
}

/// Resolves the store's [`VolumeKey`] for paths, once per mount point.
#[derive(Debug, Default)]
pub struct VolumeCache {
    by_mount: HashMap<String, VolumeKey>,
}

impl VolumeCache {
    /// The volume holding `path`. Unresolvable paths get serial 0 and the
    /// path's own root, which keeps the log row valid.
    pub fn key_for(&mut self, path: &Path) -> VolumeKey {
        let fallback = || VolumeKey {
            serial: 0,
            guid_path: String::new(),
        };
        let Ok(canon) = CanonicalPath::parse(path) else {
            return fallback();
        };
        let Ok(info) = strata_clean::volume::volume_info(&canon) else {
            return fallback();
        };
        if let Some(k) = self.by_mount.get(&info.mount_point) {
            return k.clone();
        }
        let key = match info.guid {
            Some(guid) => {
                let guid_path = format!(r"\\?\Volume{guid}\");
                let serial = strata_win::volume::local_volume_info(&guid_path, None, false)
                    .serial
                    .map_or(0, u64::from);
                VolumeKey { serial, guid_path }
            }
            None => VolumeKey {
                serial: 0,
                guid_path: info.mount_point.clone(),
            },
        };
        self.by_mount.insert(info.mount_point, key.clone());
        key
    }
}

// -----------------------------------------------------------------------------
// Crash recovery
// -----------------------------------------------------------------------------

/// What recovery found on disk for one pending item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    /// Something is still at the original path.
    Present,
    /// Nothing is at the original path.
    Gone,
    /// The path could not be checked (volume missing, access denied).
    Unknown(String),
}

/// File-system access used by recovery, injectable for tests.
pub trait RecoveryProbe {
    /// Whether the item is still at `path`.
    fn probe(&self, path: &str) -> ProbeResult;
    /// Recycle Bin entries whose original path is `path`, newest first.
    fn recycle_bin_entries(&self, path: &str) -> Vec<RestoreTicket>;
}

/// The real file system.
#[derive(Debug, Clone, Copy, Default)]
pub struct FsProbe;

impl RecoveryProbe for FsProbe {
    fn probe(&self, path: &str) -> ProbeResult {
        match std::fs::symlink_metadata(path) {
            Ok(_) => ProbeResult::Present,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ProbeResult::Gone,
            Err(e) => ProbeResult::Unknown(e.to_string()),
        }
    }

    fn recycle_bin_entries(&self, path: &str) -> Vec<RestoreTicket> {
        find_in_recycle_bin(Path::new(path)).unwrap_or_default()
    }
}

/// Slack for clock differences between the log and the Recycle Bin's own
/// deletion time.
const RECYCLE_TIME_SLACK_SECS: i64 = 120;

/// Decides the outcome of one item left `pending` by a crash.
#[must_use]
pub fn reconcile_item(
    item: &ItemRecord,
    action_started: Timestamp,
    probe: &dyn RecoveryProbe,
) -> ItemOutcome {
    let path = &item.planned.path;
    match probe.probe(path) {
        ProbeResult::Present => ItemOutcome {
            result: ItemResult::Skipped,
            error: Some("Strata closed before this item was removed; it is still in place".into()),
            restore: None,
        },
        ProbeResult::Unknown(why) => ItemOutcome {
            result: ItemResult::Failed,
            error: Some(format!(
                "Strata closed during the cleanup and the item could not be checked afterwards: {why}"
            )),
            restore: None,
        },
        ProbeResult::Gone if item.planned.method == DeleteMethod::Recycle => {
            let earliest = action_started.0 - RECYCLE_TIME_SLACK_SECS;
            let ticket = probe
                .recycle_bin_entries(path)
                .into_iter()
                .find(|t| Timestamp::from_filetime(t.deleted_at).0 >= earliest);
            match ticket {
                Some(t) => ItemOutcome {
                    result: ItemResult::Done,
                    error: None,
                    restore: Some(RestoreInfo {
                        original_path: t.original_path.clone(),
                        blob: t.to_blob(),
                    }),
                },
                None => ItemOutcome {
                    result: ItemResult::Done,
                    error: Some(
                        "Strata closed during the cleanup; the item is gone but no Recycle Bin entry was found"
                            .into(),
                    ),
                    restore: None,
                },
            }
        }
        ProbeResult::Gone => ItemOutcome {
            result: ItemResult::Done,
            error: Some("Strata closed during the cleanup; the item is gone".into()),
            restore: None,
        },
    }
}

/// What [`recover`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecoveryReport {
    /// Actions closed as interrupted.
    pub actions: u64,
    /// Items found removed.
    pub removed: u64,
    /// Items found still in place.
    pub still_present: u64,
    /// Items that could not be checked.
    pub unknown: u64,
}

/// Reconciles every action left in progress by a crash and closes it as
/// [`ActionStatus::Interrupted`]. Run before cleanup is enabled.
///
/// # Errors
///
/// Store errors; recovery then runs again next launch.
pub fn recover(store: &Store, probe: &dyn RecoveryProbe) -> strata_store::Result<RecoveryReport> {
    let mut report = RecoveryReport::default();
    for ActionRecord { summary, items } in store.recover_incomplete()? {
        for item in items.iter().filter(|i| i.result == ItemResult::Pending) {
            let outcome = reconcile_item(item, summary.started_at, probe);
            match outcome.result {
                ItemResult::Done => report.removed += 1,
                ItemResult::Skipped => report.still_present += 1,
                _ => report.unknown += 1,
            }
            store.complete_item(summary.id, item.seq, &outcome)?;
        }
        store.finish_action(summary.id, ActionStatus::Interrupted)?;
        report.actions += 1;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_clean::CleanError;
    use strata_clean::audit::ActionSummary as Summary;
    use strata_clean::permanent::DeleteStats;
    use strata_core::{FileRef, Safety};

    fn audit_item(id: u64) -> AuditItem {
        AuditItem {
            item_id: id,
            path: format!(r"D:\strata-test\{id}.tmp"),
            file_ref: FileRef(100 + id),
            size: 10 * id,
            safety: Safety::Safe,
        }
    }

    fn meta(method: DeleteMethod) -> ItemMeta {
        ItemMeta {
            mtime: FileTime(5),
            volume: VolumeKey {
                serial: 1,
                guid_path: r"\\?\Volume{test}\".into(),
            },
            method,
            rule_id: None,
        }
    }

    fn log(store: &Store, ids: &[u64]) -> StoreAuditLog {
        StoreAuditLog::new(
            store.clone(),
            ActionKind::Cleanup,
            ids.iter()
                .map(|&i| (i, meta(DeleteMethod::Recycle)))
                .collect(),
        )
    }

    fn ticket(path: &str, deleted_unix: i64) -> RestoreTicket {
        RestoreTicket {
            version: strata_clean::recycle::RESTORE_TICKET_VERSION,
            original_path: path.into(),
            recycled_path: r"D:\$Recycle.Bin\S-1-5-21-1-2-3-1001\$Rabc.tmp".into(),
            info_path: r"D:\$Recycle.Bin\S-1-5-21-1-2-3-1001\$Iabc.tmp".into(),
            deleted_at: FileTime::from_unix_secs(deleted_unix),
            size: 1,
            identity: None,
        }
    }

    #[test]
    fn begin_action_is_durable_before_any_item_starts() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut log = log(&store, &[1, 2]);
        let action = AuditAction {
            method: CleanMethod::RecycleBin,
            started_at: FileTime(0),
            items: vec![audit_item(1), audit_item(2)],
        };
        let id = log.begin_action(&action).unwrap();
        log.item_started(id, &audit_item(1)).unwrap();

        // A second handle sees the committed write-ahead rows, as a fresh
        // process would after a crash at this point.
        let reopened = Store::open(dir.path()).unwrap();
        let pending = reopened.recover_incomplete().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].items.len(), 2);
        assert!(
            pending[0]
                .items
                .iter()
                .all(|i| i.result == ItemResult::Pending)
        );
        assert_eq!(pending[0].items[1].planned.path, audit_item(2).path);
        assert_eq!(pending[0].items[0].planned.mtime, FileTime(5));
    }

    #[test]
    fn full_protocol_records_outcomes_and_status() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut log = StoreAuditLog::new(
            store.clone(),
            ActionKind::Cleanup,
            [
                (1, meta(DeleteMethod::Recycle)),
                (2, meta(DeleteMethod::Recycle)),
                (3, meta(DeleteMethod::Permanent)),
            ]
            .into_iter()
            .collect(),
        );
        let id = log
            .begin_action(&AuditAction {
                method: CleanMethod::RecycleBin,
                started_at: FileTime(0),
                items: vec![audit_item(1), audit_item(2), audit_item(3)],
            })
            .unwrap();
        log.item_started(id, &audit_item(1)).unwrap();
        log.item_finished(
            id,
            1,
            &CleanOutcome::Recycled {
                ticket: ticket(&audit_item(1).path, 0),
            },
        )
        .unwrap();
        log.item_started(id, &audit_item(2)).unwrap();
        log.item_finished(
            id,
            2,
            &CleanOutcome::Failed {
                error: CleanError::NotFound {
                    path: audit_item(2).path,
                },
            },
        )
        .unwrap();
        log.item_finished(
            id,
            3,
            &CleanOutcome::Deleted {
                stats: DeleteStats::default(),
            },
        )
        .unwrap();
        let summary = Summary {
            succeeded: 2,
            failed: 1,
            ..Summary::default()
        };
        log.finish_action(id, &summary).unwrap();

        let rec = store.action(log.store_action().unwrap()).unwrap();
        assert_eq!(rec.summary.status, ActionStatus::Partial);
        assert_eq!(rec.items[0].result, ItemResult::Done);
        let blob = &rec.items[0].restore.as_ref().unwrap().blob;
        assert_eq!(
            RestoreTicket::from_blob(blob).unwrap().original_path,
            audit_item(1).path
        );
        assert_eq!(rec.items[1].result, ItemResult::Failed);
        assert!(
            rec.items[1]
                .error
                .as_deref()
                .unwrap()
                .contains("no longer exists")
        );
        assert_eq!(store.restorable_items(10).unwrap().len(), 1);
        assert!(store.recover_incomplete().unwrap().is_empty());
    }

    #[test]
    fn unknown_items_and_actions_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut l = log(&store, &[1]);
        assert!(l.item_started(CleanActionId(1), &audit_item(1)).is_err());
        let bad = AuditAction {
            method: CleanMethod::RecycleBin,
            started_at: FileTime(0),
            items: vec![audit_item(1), audit_item(9)],
        };
        assert!(l.begin_action(&bad).is_err(), "item 9 has no metadata");
        assert!(
            store.recover_incomplete().unwrap().is_empty(),
            "nothing written"
        );
        let id = l
            .begin_action(&AuditAction {
                items: vec![audit_item(1)],
                ..bad
            })
            .unwrap();
        assert!(l.item_started(id, &audit_item(2)).is_err());
        assert!(
            l.item_started(CleanActionId(id.0 + 1), &audit_item(1))
                .is_err()
        );
    }

    #[test]
    fn status_mapping() {
        let s = |succeeded, failed, cancelled| Summary {
            succeeded,
            failed,
            cancelled,
            ..Summary::default()
        };
        assert_eq!(final_status(&s(3, 0, false)), ActionStatus::Completed);
        assert_eq!(final_status(&s(0, 0, false)), ActionStatus::Completed);
        assert_eq!(final_status(&s(2, 1, false)), ActionStatus::Partial);
        assert_eq!(final_status(&s(0, 2, false)), ActionStatus::Failed);
        assert_eq!(final_status(&s(2, 1, true)), ActionStatus::Cancelled);
    }

    struct FakeProbe {
        present: Vec<String>,
        unknown: Vec<String>,
        bin: Vec<RestoreTicket>,
    }

    impl RecoveryProbe for FakeProbe {
        fn probe(&self, path: &str) -> ProbeResult {
            if self.present.iter().any(|p| p == path) {
                ProbeResult::Present
            } else if self.unknown.iter().any(|p| p == path) {
                ProbeResult::Unknown("access denied".into())
            } else {
                ProbeResult::Gone
            }
        }
        fn recycle_bin_entries(&self, path: &str) -> Vec<RestoreTicket> {
            self.bin
                .iter()
                .filter(|t| t.original_path == path)
                .cloned()
                .collect()
        }
    }

    #[test]
    fn crash_recovery_reconciles_pending_items() {
        let dir = tempfile::tempdir().unwrap();
        let clock = std::sync::Arc::new(strata_store::ManualClock::new(Timestamp(1_000_000)));
        let store = Store::open_with_clock(dir.path(), clock).unwrap();
        let ids = [1, 2, 3, 4, 5];
        let mut l = log(&store, &ids);
        let id = l
            .begin_action(&AuditAction {
                method: CleanMethod::RecycleBin,
                started_at: FileTime(0),
                items: ids.iter().map(|&i| audit_item(i)).collect(),
            })
            .unwrap();
        l.item_finished(
            id,
            1,
            &CleanOutcome::Deleted {
                stats: DeleteStats::default(),
            },
        )
        .unwrap();
        // Crash: items 2..=5 are still pending.
        drop(l);

        let probe = FakeProbe {
            present: vec![audit_item(2).path],
            unknown: vec![audit_item(5).path],
            bin: vec![
                ticket(&audit_item(3).path, 1_000_010),
                // Recycled long before this action: not ours.
                ticket(&audit_item(4).path, 10),
            ],
        };
        let report = recover(&store, &probe).unwrap();
        assert_eq!(
            report,
            RecoveryReport {
                actions: 1,
                removed: 2,
                still_present: 1,
                unknown: 1,
            }
        );
        let rec = store.action(l_id(&store)).unwrap();
        assert_eq!(rec.summary.status, ActionStatus::Interrupted);
        assert_eq!(rec.items[1].result, ItemResult::Skipped);
        assert_eq!(rec.items[2].result, ItemResult::Done);
        assert!(rec.items[2].restore.is_some(), "ticket recovered");
        assert_eq!(rec.items[3].result, ItemResult::Done);
        assert!(rec.items[3].restore.is_none());
        assert_eq!(rec.items[4].result, ItemResult::Failed);
        assert!(store.recover_incomplete().unwrap().is_empty());
        assert_eq!(recover(&store, &probe).unwrap(), RecoveryReport::default());
    }

    fn l_id(store: &Store) -> ActionId {
        store.action_history(1, None).unwrap()[0].id
    }
}
