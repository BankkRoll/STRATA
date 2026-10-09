//! Cleanup flow under failure: a crash between acting and recording (with
//! the in-memory log and with the SQLite undo log), an item too large for
//! the Recycle Bin, an item locked between pre-flight and action, and case
//! variants of one name queued together.
//!
//! Every Recycle Bin entry a test creates is restored or purged (exactly
//! that `$R`/`$I` pair) before the test ends.

mod common;
mod harden_support;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use common::{Holder, guard};
use harden_support::HardenDir;
use strata_clean::audit::{
    ActionId, ActionSummary, AuditAction, AuditError, AuditEvent, AuditItem, AuditLog, ItemOutcome,
    MemoryAuditLog,
};
use strata_clean::flow::{
    Acknowledgements, CleanupConfig, Decision, DeleteMethod, Plan, PlanWarning, QueueItem, execute,
    plan, preflight,
};
use strata_clean::preflight::{RecycleFit, Verdict};
use strata_clean::recycle::{RestoreTicket, find_in_recycle_bin, restore};
use strata_clean::{CancelToken, CleanError, Expected};
use strata_core::Safety;

// -----------------------------------------------------------------------------
// Fixtures
// -----------------------------------------------------------------------------

/// Purges the bin entries of tickets that were not restored.
#[derive(Default)]
struct BinCleanup(Vec<RestoreTicket>);

impl Drop for BinCleanup {
    fn drop(&mut self) {
        for t in &self.0 {
            assert!(
                t.recycled_path.to_uppercase().contains(r"\$RECYCLE.BIN\"),
                "refusing to purge outside the bin: {}",
                t.recycled_path
            );
            let r = Path::new(&t.recycled_path);
            if r.is_dir() {
                common::unlink_reparse_points(r);
                let _ = std::fs::remove_dir_all(r);
            } else {
                let _ = std::fs::remove_file(r);
            }
            let _ = std::fs::remove_file(&t.info_path);
        }
    }
}

fn qi(id: u64, p: &Path) -> QueueItem {
    let facts = guard().check_path(p).unwrap().facts;
    QueueItem {
        id,
        path: p.to_path_buf(),
        expected: Expected::from_facts(&facts),
        safety: Safety::Safe,
    }
}

fn run(p: &Plan, decision: &Decision, cfg: &CleanupConfig, log: &mut dyn AuditLog) {
    let _ = execute(
        guard(),
        p,
        decision,
        cfg,
        log,
        &mut |_| {},
        &CancelToken::new(),
    );
}

/// Simulates the process dying right after item `crash_after` was acted on
/// but before its outcome was recorded. Also checks write-ahead ordering:
/// every item still exists when its start record is written.
struct CrashingLog {
    inner: MemoryAuditLog,
    paths: Vec<(u64, PathBuf)>,
    crash_after: u64,
}

impl AuditLog for CrashingLog {
    fn begin_action(&mut self, action: &AuditAction) -> Result<ActionId, AuditError> {
        self.inner.begin_action(action)
    }

    fn item_started(&mut self, action: ActionId, item: &AuditItem) -> Result<(), AuditError> {
        let path = &self
            .paths
            .iter()
            .find(|(id, _)| *id == item.item_id)
            .unwrap()
            .1;
        assert!(
            path.exists(),
            "{} was gone before its start record",
            path.display()
        );
        self.inner.item_started(action, item)
    }

    fn item_finished(
        &mut self,
        action: ActionId,
        item_id: u64,
        outcome: &ItemOutcome,
    ) -> Result<(), AuditError> {
        if item_id == self.crash_after {
            panic!("simulated crash");
        }
        self.inner.item_finished(action, item_id, outcome)
    }

    fn finish_action(
        &mut self,
        action: ActionId,
        summary: &ActionSummary,
    ) -> Result<(), AuditError> {
        self.inner.finish_action(action, summary)
    }
}

fn tickets_of(log: &MemoryAuditLog) -> Vec<RestoreTicket> {
    log.events
        .iter()
        .filter_map(|e| match e {
            AuditEvent::Finished(_, _, ItemOutcome::Recycled { ticket }) => Some(ticket.clone()),
            _ => None,
        })
        .collect()
}

/// After a crash, every item is accounted for: never started means still on
/// disk, finished-as-removed means gone, and an interrupted item is either
/// still on disk or recoverable from the Recycle Bin (which is restored).
fn assert_recoverable(log: &MemoryAuditLog, paths: &[(u64, PathBuf)], recycle: bool) -> usize {
    let started: Vec<u64> = log
        .events
        .iter()
        .filter_map(|e| match e {
            AuditEvent::Started(_, i) => Some(i.item_id),
            _ => None,
        })
        .collect();
    let interrupted: Vec<u64> = log.interrupted().into_iter().map(|(_, id)| id).collect();
    let mut recovered = 0;
    for (id, p) in paths {
        if !started.contains(id) {
            assert!(p.exists(), "item {id} was never started but is gone");
            continue;
        }
        if interrupted.contains(id) {
            // A permanent delete that was interrupted may legitimately be
            // gone: the start record is exactly what says so.
            if p.exists() || !recycle {
                continue;
            }
            let found = find_in_recycle_bin(p).unwrap();
            let t = found.first().unwrap_or_else(|| {
                panic!("interrupted item {id} is neither on disk nor in the bin")
            });
            restore(t).unwrap();
            assert!(p.exists());
            recovered += 1;
        } else {
            let removed = log
                .events
                .iter()
                .any(|e| matches!(e, AuditEvent::Finished(_, j, o) if j == id && o.removed()));
            assert_eq!(!p.exists(), removed, "item {id} disagrees with its record");
        }
    }
    recovered
}

fn files(t: &HardenDir, n: u64) -> Vec<(u64, PathBuf)> {
    (1..=n)
        .map(|i| {
            (
                i,
                t.file(&format!("f{i}.bin"), format!("item {i}").as_bytes()),
            )
        })
        .collect()
}

// -----------------------------------------------------------------------------
// Crash between acting and recording
// -----------------------------------------------------------------------------

fn crash_case(method: DeleteMethod, batch: usize, crash_after: u64) {
    let t = HardenDir::new("flow-crash");
    let paths = files(&t, 5);
    let p = plan(guard(), paths.iter().map(|(id, p)| qi(*id, p)).collect());
    let decision = Decision {
        method,
        acks: Acknowledgements {
            permanent: true,
            ..Default::default()
        },
    };
    let cfg = CleanupConfig {
        recycle_batch: batch,
        ..Default::default()
    };
    let mut log = CrashingLog {
        inner: MemoryAuditLog::new(),
        paths: paths.clone(),
        crash_after,
    };
    let crashed = catch_unwind(AssertUnwindSafe(|| run(&p, &decision, &cfg, &mut log)));
    assert!(crashed.is_err(), "the simulated crash did not happen");
    let _bin = BinCleanup(tickets_of(&log.inner));
    let interrupted = log.inner.interrupted();
    assert!(
        interrupted.iter().any(|(_, id)| *id == crash_after),
        "{interrupted:?}"
    );
    let recycle = method == DeleteMethod::RecycleBin;
    let recovered = assert_recoverable(&log.inner, &paths, recycle);
    if recycle {
        assert!(recovered >= 1, "the crashed item was not recovered");
    }
}

#[test]
fn crash_after_recycling_one_by_one_leaves_a_recoverable_log() {
    crash_case(DeleteMethod::RecycleBin, 1, 3);
}

#[test]
fn crash_after_a_recycle_batch_leaves_a_recoverable_log() {
    crash_case(DeleteMethod::RecycleBin, 32, 2);
}

#[test]
fn crash_after_a_permanent_delete_leaves_a_consistent_log() {
    crash_case(DeleteMethod::Permanent, 1, 4);
}

// -----------------------------------------------------------------------------
// The same crash through the SQLite undo log
// -----------------------------------------------------------------------------

/// The app's adapter shape: the store records every item as pending up
/// front, so `item_started` has nothing more to persist.
struct StoreLog {
    store: strata_store::Store,
    action: Option<strata_store::ActionId>,
    seq: Vec<u64>,
    crash_after: u64,
}

impl AuditLog for StoreLog {
    fn begin_action(&mut self, action: &AuditAction) -> Result<ActionId, AuditError> {
        let items: Vec<strata_store::PlannedItem> = action
            .items
            .iter()
            .map(|i| strata_store::PlannedItem {
                path: i.path.clone(),
                volume: strata_store::VolumeKey {
                    serial: 1,
                    guid_path: r"\\?\Volume{test}\".into(),
                },
                file_ref: i.file_ref,
                size: i.size,
                mtime: strata_core::FileTime(0),
                method: match action.method {
                    DeleteMethod::RecycleBin => strata_store::DeleteMethod::Recycle,
                    DeleteMethod::Permanent => strata_store::DeleteMethod::Permanent,
                },
                tier: i.safety,
                rule_id: None,
            })
            .collect();
        self.seq = action.items.iter().map(|i| i.item_id).collect();
        let id = self
            .store
            .begin_action(strata_store::ActionKind::Cleanup, &items)
            .map_err(|e| AuditError(e.to_string()))?;
        self.action = Some(id);
        Ok(ActionId(id.0 as u64))
    }

    fn item_started(&mut self, _: ActionId, _: &AuditItem) -> Result<(), AuditError> {
        Ok(())
    }

    fn item_finished(
        &mut self,
        _: ActionId,
        item_id: u64,
        outcome: &ItemOutcome,
    ) -> Result<(), AuditError> {
        if item_id == self.crash_after {
            panic!("simulated crash");
        }
        let seq = self.seq.iter().position(|&s| s == item_id).unwrap() as u32;
        let o = match outcome {
            ItemOutcome::Recycled { ticket } => strata_store::ItemOutcome {
                result: strata_store::ItemResult::Done,
                error: None,
                restore: Some(strata_store::RestoreInfo {
                    original_path: ticket.original_path.clone(),
                    blob: ticket.to_blob(),
                }),
            },
            ItemOutcome::Deleted { .. } => strata_store::ItemOutcome {
                result: strata_store::ItemResult::Done,
                error: None,
                restore: None,
            },
            ItemOutcome::Failed { error } => strata_store::ItemOutcome {
                result: strata_store::ItemResult::Failed,
                error: Some(error.to_string()),
                restore: None,
            },
            ItemOutcome::Skipped { .. } => strata_store::ItemOutcome {
                result: strata_store::ItemResult::Skipped,
                error: None,
                restore: None,
            },
        };
        self.store
            .complete_item(self.action.unwrap(), seq, &o)
            .map_err(|e| AuditError(e.to_string()))
    }

    fn finish_action(&mut self, _: ActionId, _: &ActionSummary) -> Result<(), AuditError> {
        self.store
            .finish_action(self.action.unwrap(), strata_store::ActionStatus::Completed)
            .map_err(|e| AuditError(e.to_string()))
    }
}

#[test]
fn crash_mid_recycle_is_recovered_from_the_sqlite_undo_log() {
    let t = HardenDir::new("flow-crash-store");
    let paths = files(&t, 4);
    let db = t.dir("store");
    let store = strata_store::Store::open(&db).unwrap();
    let p = plan(guard(), paths.iter().map(|(id, p)| qi(*id, p)).collect());
    let cfg = CleanupConfig {
        recycle_batch: 1,
        ..Default::default()
    };
    let mut log = StoreLog {
        store,
        action: None,
        seq: Vec::new(),
        crash_after: 3,
    };
    let crashed = catch_unwind(AssertUnwindSafe(|| {
        run(&p, &Decision::recycle(), &cfg, &mut log);
    }));
    assert!(crashed.is_err());
    drop(log);

    // Next launch.
    let store = strata_store::Store::open(&db).unwrap();
    let pending = store.recover_incomplete().unwrap();
    assert_eq!(pending.len(), 1);
    let rec = &pending[0];
    let mut bin = BinCleanup::default();
    for item in &rec.items {
        let path = PathBuf::from(&item.planned.path);
        match item.result {
            strata_store::ItemResult::Done => {
                assert!(!path.exists());
                let blob = &item.restore.as_ref().unwrap().blob;
                bin.0.push(RestoreTicket::from_blob(blob).unwrap());
            }
            strata_store::ItemResult::Pending => {
                if path.exists() {
                    continue;
                }
                // Acted on before the crash: recover its ticket and restore.
                let t = find_in_recycle_bin(&path).unwrap().remove(0);
                restore(&t).unwrap();
                assert!(path.exists());
                store
                    .complete_item(
                        rec.summary.id,
                        item.seq,
                        &strata_store::ItemOutcome {
                            result: strata_store::ItemResult::Skipped,
                            error: Some("restored after an interrupted cleanup".into()),
                            restore: None,
                        },
                    )
                    .unwrap();
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    let done = rec
        .items
        .iter()
        .filter(|i| i.result == strata_store::ItemResult::Done)
        .count();
    assert_eq!(done, 2, "items 1 and 2 were recorded before the crash");
    assert!(paths[3].1.exists(), "item 4 was never touched");
    store
        .finish_action(rec.summary.id, strata_store::ActionStatus::Interrupted)
        .unwrap();
    assert!(store.recover_incomplete().unwrap().is_empty());
}

// -----------------------------------------------------------------------------
// Recycle Bin capacity and locks
// -----------------------------------------------------------------------------

fn make_sparse(p: &Path, len: u64) -> bool {
    std::fs::write(p, b"").unwrap();
    let ok = std::process::Command::new("fsutil")
        .args(["sparse", "setflag"])
        .arg(p)
        .output()
        .is_ok_and(|o| o.status.success());
    if ok {
        std::fs::OpenOptions::new()
            .write(true)
            .open(p)
            .unwrap()
            .set_len(len)
            .unwrap();
    }
    ok
}

#[test]
fn item_larger_than_the_recycle_bin_is_never_destroyed_silently() {
    let t = HardenDir::new("flow-too-large");
    let f = t.path.join("huge.sparse");
    // 8 TiB logical (under the 16 TiB NTFS limit at 4 KiB clusters), a few KiB allocated: larger than any Recycle Bin.
    if !make_sparse(&f, 8 << 40) {
        eprintln!("skipped: sparse files unavailable");
        return;
    }
    let p = plan(guard(), vec![qi(1, &f)]);
    let fit = p.warnings.iter().find_map(|w| match w {
        PlanWarning::CannotRecycle { id: 1, fit } => Some(*fit),
        _ => None,
    });
    // Without a recorded capacity the plan cannot know; the delete sink
    // still stops the Shell from destroying it.
    assert!(
        matches!(
            fit,
            None | Some(RecycleFit::TooLarge { .. } | RecycleFit::Unavailable { .. })
        ),
        "{fit:?}"
    );

    let mut log = MemoryAuditLog::new();
    let report = execute(
        guard(),
        &p,
        &Decision::recycle(),
        &CleanupConfig::default(),
        &mut log,
        &mut |_| {},
        &CancelToken::new(),
    );
    let mut bin = BinCleanup::default();
    match &report.results[0].outcome {
        ItemOutcome::Recycled { ticket } => {
            // The Shell accepted it; it must really be in the bin.
            assert!(Path::new(&ticket.recycled_path).exists());
            bin.0.push(ticket.clone());
        }
        ItemOutcome::Failed { error } => {
            assert!(
                matches!(
                    error,
                    CleanError::WouldDeletePermanently { .. }
                        | CleanError::TooLargeForRecycleBin { .. }
                ),
                "{error:?}"
            );
            assert!(f.exists(), "a failed recycle removed the file");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn item_locked_between_preflight_and_recycle_fails_and_survives() {
    let t = HardenDir::new("flow-lock-race");
    let free = t.file("free.txt", b"free");
    let locked = t.file("locked.txt", b"locked");
    let p = plan(guard(), vec![qi(1, &free), qi(2, &locked)]);
    let verdicts = preflight(
        guard(),
        &p,
        &Acknowledgements::default(),
        &CleanupConfig::default(),
    );
    assert!(
        verdicts
            .iter()
            .all(|v| matches!(v.verdict, Verdict::Ready { .. })),
        "{verdicts:?}"
    );
    let holder = Holder::spawn(&locked);
    let mut log = MemoryAuditLog::new();
    let report = execute(
        guard(),
        &p,
        &Decision::recycle(),
        &CleanupConfig::default(),
        &mut log,
        &mut |_| {},
        &CancelToken::new(),
    );
    let _bin = BinCleanup(tickets_of(&log));
    assert!(report.results[0].outcome.removed());
    match &report.results[1].outcome {
        ItemOutcome::Failed { error } => assert!(error.is_retryable(), "{error:?}"),
        other => panic!("{other:?}"),
    }
    drop(holder);
    assert!(locked.exists());
    assert_eq!(std::fs::read(&locked).unwrap(), b"locked");
}

// -----------------------------------------------------------------------------
// Case variants
// -----------------------------------------------------------------------------

#[test]
fn case_variants_of_one_name_act_on_the_requested_item_only() {
    let t = HardenDir::new("flow-case");
    let upper = t.file("A.txt", b"upper");
    let lower = t.path.join("a.txt");
    let first = qi(1, &upper);
    let mut second = first.clone();
    second.id = 2;
    second.path = lower.clone();
    let p = plan(guard(), vec![first, second]);
    assert_eq!(p.items.len(), 1);
    assert_eq!(p.items[0].id, 1);
    assert_eq!(p.items[0].path, upper);
    assert!(
        p.warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::Duplicate { id: 2 }))
    );
    let mut log = MemoryAuditLog::new();
    let report = execute(
        guard(),
        &p,
        &Decision::recycle(),
        &CleanupConfig::default(),
        &mut log,
        &mut |_| {},
        &CancelToken::new(),
    );
    let _bin = BinCleanup(tickets_of(&log));
    assert_eq!(report.results.len(), 1);
    assert_eq!(report.results[0].id, 1);
    assert!(report.results[0].outcome.removed());
    assert!(!upper.exists());
}
