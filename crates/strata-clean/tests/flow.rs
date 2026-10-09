//! End-to-end: plan, pre-flight, execute with the write-ahead audit log.

mod common;

use std::path::Path;

use common::{Holder, TestDir, guard};
use strata_clean::audit::{AuditEvent, ItemOutcome, MemoryAuditLog};
use strata_clean::flow::{
    Acknowledgements, CleanupConfig, Decision, DeleteMethod, PlanWarning, Progress, QueueItem,
    execute, plan, preflight,
};
use strata_clean::preflight::{RecycleFit, Verdict};
use strata_clean::recycle::restore;
use strata_clean::{CancelToken, CleanError, Expected};
use strata_core::Safety;

fn qi(id: u64, p: &Path, safety: Safety) -> QueueItem {
    let facts = guard().check_path(p).unwrap().facts;
    QueueItem {
        id,
        path: p.to_path_buf(),
        expected: Expected::from_facts(&facts),
        safety,
    }
}

fn run(
    plan: &strata_clean::flow::Plan,
    method: DeleteMethod,
    acks: &Acknowledgements,
    log: &mut MemoryAuditLog,
) -> (strata_clean::flow::ExecutionReport, Vec<Progress>) {
    let mut events = Vec::new();
    let report = execute(
        guard(),
        plan,
        &Decision {
            method,
            acks: acks.clone(),
        },
        &CleanupConfig::default(),
        log,
        &mut |p| events.push(p),
        &CancelToken::new(),
    );
    (report, events)
}

fn assert_write_ahead(log: &MemoryAuditLog) {
    for (pos, e) in log.events.iter().enumerate() {
        if let AuditEvent::Finished(a, id, outcome) = e {
            if matches!(outcome, ItemOutcome::Skipped { .. }) {
                continue;
            }
            let started = log.events[..pos].iter().any(
                |s| matches!(s, AuditEvent::Started(b, item) if b == a && item.item_id == *id),
            );
            assert!(started, "item {id} finished without a prior start record");
        }
    }
    assert!(matches!(log.events.first(), Some(AuditEvent::Begin(..))));
    assert!(matches!(log.events.last(), Some(AuditEvent::End(..))));
}

#[test]
fn recycle_flow_with_tiers_and_restore() {
    let t = TestDir::new("flow-recycle");
    let safe = t.file("cache.bin", b"cache");
    let careful_ok = t.file("model.gguf", b"weights");
    let careful_no = t.file("notes.txt", b"notes");
    let never = t.file("history.db", b"history");
    let dir = t.dir("cachedir");
    t.file(r"cachedir\x.tmp", b"x");

    let queue = vec![
        qi(1, &safe, Safety::Safe),
        qi(2, &careful_ok, Safety::Careful),
        qi(3, &careful_no, Safety::Careful),
        qi(4, &never, Safety::Never),
        qi(5, &dir, Safety::Probably),
        qi(6, &dir.join("x.tmp"), Safety::Safe),
        qi(7, &safe, Safety::Safe),
        QueueItem {
            path: r"C:\Windows\System32".into(),
            ..qi(8, &safe, Safety::Safe)
        },
    ];
    let p = plan(guard(), queue);
    assert_eq!(p.items.len(), 5, "{:?}", p.warnings);
    assert!(
        p.warnings
            .contains(&PlanWarning::Nested { id: 6, inside: 5 })
    );
    assert!(p.warnings.contains(&PlanWarning::Duplicate { id: 7 }));
    assert!(
        p.warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::Refused { id: 8, .. }))
    );
    assert!(p.warnings.contains(&PlanWarning::NeverTier { id: 4 }));
    assert!(
        p.warnings
            .contains(&PlanWarning::NeedsAcknowledgement { id: 3 })
    );
    assert_eq!(p.volumes.len(), 1);

    let mut acks = Acknowledgements::default();
    acks.careful.insert(2);
    let verdicts = preflight(guard(), &p, &acks, &CleanupConfig::default());
    let v = |id| verdicts.iter().find(|v| v.id == id).unwrap();
    assert!(matches!(
        v(1).verdict,
        Verdict::Ready {
            recycle: RecycleFit::Fits | RecycleFit::Unknown
        }
    ));
    assert!(matches!(
        v(3).verdict,
        Verdict::Blocked {
            error: CleanError::NeedsAcknowledgement { .. }
        }
    ));
    assert!(matches!(
        v(4).verdict,
        Verdict::Blocked {
            error: CleanError::NeverTier { .. }
        }
    ));

    let mut log = MemoryAuditLog::new();
    let (report, events) = run(&p, DeleteMethod::RecycleBin, &acks, &mut log);
    assert_write_ahead(&log);
    assert!(log.interrupted().is_empty());
    assert_eq!(report.summary.succeeded, 3);
    assert_eq!(report.summary.skipped, 2);
    assert!(!safe.exists() && !careful_ok.exists() && !dir.exists());
    assert!(careful_no.exists() && never.exists());
    assert!(matches!(
        events.first(),
        Some(Progress::Started { items: 5, .. })
    ));
    assert!(matches!(events.last(), Some(Progress::Finished { .. })));

    for r in &report.results {
        if let ItemOutcome::Recycled { ticket } = &r.outcome {
            restore(ticket).unwrap();
        }
    }
    assert!(safe.exists() && careful_ok.exists() && dir.join("x.tmp").exists());
}

#[test]
fn permanent_requires_confirmations() {
    let t = TestDir::new("flow-perm");
    let a = t.file("a.bin", &[0u8; 64]);
    let b = t.file("b.bin", &[0u8; 64]);
    let p = plan(
        guard(),
        vec![qi(1, &a, Safety::Safe), qi(2, &b, Safety::Safe)],
    );

    let mut log = MemoryAuditLog::new();
    let (report, _) = run(
        &p,
        DeleteMethod::Permanent,
        &Acknowledgements::default(),
        &mut log,
    );
    assert!(report.results.iter().all(|r| matches!(
        r.outcome,
        ItemOutcome::Skipped {
            reason: CleanError::NeedsPermanentConfirmation { .. }
        }
    )));
    assert!(a.exists() && b.exists());

    let cfg = CleanupConfig {
        large_permanent_bytes: 10,
        ..Default::default()
    };
    let acks = Acknowledgements {
        permanent: true,
        ..Default::default()
    };
    let mut log = MemoryAuditLog::new();
    let report = execute(
        guard(),
        &p,
        &Decision {
            method: DeleteMethod::Permanent,
            acks: acks.clone(),
        },
        &cfg,
        &mut log,
        &mut |_| {},
        &CancelToken::new(),
    );
    assert!(report.results.iter().all(|r| matches!(
        r.outcome,
        ItemOutcome::Skipped {
            reason: CleanError::NeedsLargeDeleteConfirmation { .. }
        }
    )));
    assert!(a.exists());

    let acks = Acknowledgements {
        permanent: true,
        large_permanent: true,
        ..Default::default()
    };
    let mut log = MemoryAuditLog::new();
    let report = execute(
        guard(),
        &p,
        &Decision {
            method: DeleteMethod::Permanent,
            acks: acks.clone(),
        },
        &cfg,
        &mut log,
        &mut |_| {},
        &CancelToken::new(),
    );
    assert_eq!(report.summary.succeeded, 2);
    assert_eq!(report.summary.bytes, 128);
    assert!(!a.exists() && !b.exists());
    assert_write_ahead(&log);
}

#[test]
fn audit_failure_means_the_item_is_not_touched() {
    let t = TestDir::new("flow-audit");
    let a = t.file("a.txt", b"a");
    let b = t.file("b.txt", b"b");
    let p = plan(
        guard(),
        vec![qi(1, &a, Safety::Safe), qi(2, &b, Safety::Safe)],
    );
    let mut log = MemoryAuditLog::new();
    log.fail_start_for = Some(1);
    let acks = Acknowledgements {
        permanent: true,
        ..Default::default()
    };
    let (report, _) = run(&p, DeleteMethod::Permanent, &acks, &mut log);
    assert!(matches!(
        report.results[0].outcome,
        ItemOutcome::Skipped {
            reason: CleanError::AuditLogFailed { .. }
        }
    ));
    assert!(a.exists());
    assert!(!b.exists());
}

#[test]
fn locked_item_fails_with_holders_then_retry_succeeds() {
    let t = TestDir::new("flow-retry");
    let free = t.file("free.txt", b"f");
    let locked = t.file("locked.txt", b"l");
    let p = plan(
        guard(),
        vec![qi(1, &free, Safety::Safe), qi(2, &locked, Safety::Safe)],
    );
    let acks = Acknowledgements {
        permanent: true,
        ..Default::default()
    };
    let holder = Holder::spawn(&locked);

    let verdicts = preflight(guard(), &p, &acks, &CleanupConfig::default());
    match &verdicts[1].verdict {
        Verdict::Blocked {
            error: CleanError::Locked { holders, .. },
        } => assert!(holders.iter().any(|h| h.pid == holder.0.id())),
        other => panic!("{other:?}"),
    }

    let mut log = MemoryAuditLog::new();
    let (report, _) = run(&p, DeleteMethod::Permanent, &acks, &mut log);
    assert!(report.results[0].outcome.removed());
    match &report.results[1].outcome {
        ItemOutcome::Failed {
            error: CleanError::Locked { holders, .. },
        } => assert!(
            holders.iter().any(|h| h.pid == holder.0.id()),
            "{holders:?}"
        ),
        other => panic!("{other:?}"),
    }
    drop(holder);
    let retry = p.retry(&report);
    assert_eq!(retry.items.len(), 1);
    let mut log = MemoryAuditLog::new();
    let (report, _) = run(&retry, DeleteMethod::Permanent, &acks, &mut log);
    assert!(report.results[0].outcome.removed());
    assert!(!locked.exists());
}

#[test]
fn changed_items_are_blocked_and_not_deleted() {
    let t = TestDir::new("flow-changed");
    let f = t.file("f.txt", b"one");
    let p = plan(guard(), vec![qi(1, &f, Safety::Safe)]);
    std::fs::write(&f, b"two!").unwrap();
    let verdicts = preflight(
        guard(),
        &p,
        &Acknowledgements::default(),
        &CleanupConfig::default(),
    );
    assert!(matches!(
        verdicts[0].verdict,
        Verdict::Blocked {
            error: CleanError::Changed { .. }
        }
    ));
    let mut log = MemoryAuditLog::new();
    let (report, _) = run(
        &p,
        DeleteMethod::RecycleBin,
        &Acknowledgements::default(),
        &mut log,
    );
    assert!(matches!(
        report.results[0].outcome,
        ItemOutcome::Failed {
            error: CleanError::Changed { .. }
        }
    ));
    assert!(f.exists());
}

#[test]
fn cancelled_execution_touches_nothing() {
    let t = TestDir::new("flow-cancel");
    let f = t.file("f.txt", b"x");
    let p = plan(guard(), vec![qi(1, &f, Safety::Safe)]);
    let c = CancelToken::new();
    c.cancel();
    let mut log = MemoryAuditLog::new();
    let report = execute(
        guard(),
        &p,
        &Decision::recycle(),
        &CleanupConfig::default(),
        &mut log,
        &mut |_| {},
        &c,
    );
    assert!(report.summary.cancelled);
    assert!(matches!(
        report.results[0].outcome,
        ItemOutcome::Skipped {
            reason: CleanError::Cancelled { .. }
        }
    ));
    assert!(f.exists());
}

#[test]
fn too_long_for_recycle_bin_needs_explicit_permanent_choice() {
    let t = TestDir::new("flow-long");
    let mut d = t.path.clone();
    for _ in 0..28 {
        d.push("abcdefghij");
    }
    std::fs::create_dir_all(&d).unwrap();
    let f = d.join("deep.txt");
    std::fs::write(&f, b"deep").unwrap();
    let p = plan(guard(), vec![qi(1, &f, Safety::Safe)]);
    assert!(
        p.warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::CannotRecycle { id: 1, .. }))
    );

    let mut log = MemoryAuditLog::new();
    let (report, _) = run(
        &p,
        DeleteMethod::RecycleBin,
        &Acknowledgements::default(),
        &mut log,
    );
    assert!(matches!(
        report.results[0].outcome,
        ItemOutcome::Failed {
            error: CleanError::RecycleBinUnavailable { .. }
        }
    ));
    assert!(f.exists());

    let mut acks = Acknowledgements::default();
    acks.permanent_instead_of_recycle.insert(1);
    let mut log = MemoryAuditLog::new();
    let (report, _) = run(&p, DeleteMethod::RecycleBin, &acks, &mut log);
    assert!(matches!(
        report.results[0].outcome,
        ItemOutcome::Deleted { .. }
    ));
    assert!(!f.exists());
}
