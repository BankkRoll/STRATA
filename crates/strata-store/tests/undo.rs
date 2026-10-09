//! Undo/audit log: write-ahead protocol, crash recovery and queries.

mod common;

use std::sync::Arc;

use common::*;
use strata_core::{FileRef, FileTime, Safety};
use strata_store::*;

fn item(n: u64, method: DeleteMethod) -> PlannedItem {
    PlannedItem {
        path: format!(r"C:\Users\me\AppData\Local\Temp\file{n}.tmp"),
        volume: volume(),
        file_ref: FileRef::from_parts(1000 + n, 3),
        size: n * MIB,
        mtime: FileTime(133_000_000_000_000_000 + n),
        method,
        tier: Safety::Probably,
        rule_id: n.is_multiple_of(2).then(|| "temp.user".to_owned()),
    }
}

fn done(restore: Option<RestoreInfo>) -> ItemOutcome {
    ItemOutcome {
        result: ItemResult::Done,
        error: None,
        restore,
    }
}

#[test]
fn full_protocol_records_everything() {
    let (_d, store, clock) = store_at(t0());
    let items = vec![
        item(1, DeleteMethod::Recycle),
        item(2, DeleteMethod::Recycle),
        item(3, DeleteMethod::Permanent),
    ];
    let id = store.begin_action(ActionKind::Cleanup, &items).unwrap();
    clock.advance_secs(5);
    store
        .complete_item(
            id,
            0,
            &done(Some(RestoreInfo {
                original_path: items[0].path.clone(),
                blob: vec![0xDE, 0xAD],
            })),
        )
        .unwrap();
    store
        .complete_item(
            id,
            1,
            &ItemOutcome {
                result: ItemResult::Failed,
                error: Some("in use by Discord.exe (PID 1234)".into()),
                restore: None,
            },
        )
        .unwrap();
    store.finish_action(id, ActionStatus::Partial).unwrap();

    let rec = store.action(id).unwrap();
    assert_eq!(rec.summary.status, ActionStatus::Partial);
    assert_eq!(rec.summary.kind, ActionKind::Cleanup);
    assert_eq!(rec.summary.started_at, t0());
    assert_eq!(rec.summary.finished_at, Some(Timestamp(t0().0 + 5)));
    assert_eq!(rec.summary.item_count, 3);
    assert_eq!(rec.summary.done_count, 1);
    assert_eq!(rec.summary.failed_count, 1);
    assert_eq!(rec.summary.bytes_done, MIB);
    assert_eq!(rec.items.len(), 3);
    assert_eq!(rec.items[0].planned, items[0]);
    assert_eq!(
        rec.items[0].restore.as_ref().unwrap().blob,
        vec![0xDE, 0xAD]
    );
    assert_eq!(rec.items[1].result, ItemResult::Failed);
    assert!(rec.items[1].error.as_deref().unwrap().contains("Discord"));
    assert_eq!(
        rec.items[2].result,
        ItemResult::Skipped,
        "pending -> skipped"
    );
    assert_eq!(rec.items[2].planned.rule_id, None);
}

#[test]
fn write_ahead_survives_crash_and_is_recovered() {
    let (d, store, _c) = store_at(t0());
    let items: Vec<_> = (1..=4).map(|n| item(n, DeleteMethod::Recycle)).collect();
    let crashed = store.begin_action(ActionKind::Cleanup, &items).unwrap();
    store.complete_item(crashed, 0, &done(None)).unwrap();
    let finished = store
        .begin_action(ActionKind::Tool, &[item(9, DeleteMethod::Tool)])
        .unwrap();
    store
        .finish_action(finished, ActionStatus::Completed)
        .unwrap();
    // Simulate the process dying: no finish_action, store dropped.
    drop(store);

    let store = Store::open_with_clock(d.path(), Arc::new(ManualClock::new(t0()))).unwrap();
    let pending = store.recover_incomplete().unwrap();
    assert_eq!(pending.len(), 1);
    let rec = &pending[0];
    assert_eq!(rec.summary.id, crashed);
    assert_eq!(rec.summary.status, ActionStatus::InProgress);
    assert_eq!(rec.items[0].result, ItemResult::Done);
    assert!(
        rec.items[1..]
            .iter()
            .all(|i| i.result == ItemResult::Pending)
    );

    // Reconcile: item 1 turned out to be gone already, the rest untouched.
    store.complete_item(crashed, 1, &done(None)).unwrap();
    store
        .finish_action(crashed, ActionStatus::Interrupted)
        .unwrap();
    assert!(store.recover_incomplete().unwrap().is_empty());
    let rec = store.action(crashed).unwrap();
    assert_eq!(rec.summary.status, ActionStatus::Interrupted);
    assert_eq!(rec.summary.done_count, 2);
}

#[test]
fn protocol_misuse_is_rejected() {
    let (_d, store, _c) = store_at(t0());
    assert!(matches!(
        store.begin_action(ActionKind::Cleanup, &[]),
        Err(StoreError::InvalidInput(_))
    ));
    let id = store
        .begin_action(ActionKind::Cleanup, &[item(1, DeleteMethod::Recycle)])
        .unwrap();
    assert!(matches!(
        store.complete_item(id, 5, &done(None)),
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store.complete_item(
            id,
            0,
            &ItemOutcome {
                result: ItemResult::Pending,
                error: None,
                restore: None
            }
        ),
        Err(StoreError::InvalidInput(_))
    ));
    assert!(matches!(
        store.complete_item(ActionId(99), 0, &done(None)),
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store.finish_action(id, ActionStatus::InProgress),
        Err(StoreError::InvalidInput(_))
    ));
    store.finish_action(id, ActionStatus::Cancelled).unwrap();
    assert!(matches!(
        store.finish_action(id, ActionStatus::Completed),
        Err(StoreError::InvalidInput(_))
    ));
    assert!(matches!(
        store.complete_item(id, 0, &done(None)),
        Err(StoreError::InvalidInput(_))
    ));
}

#[test]
fn restorable_items_and_mark_restored() {
    let (_d, store, clock) = store_at(t0());
    let items = vec![
        item(1, DeleteMethod::Recycle),
        item(2, DeleteMethod::Permanent),
        item(3, DeleteMethod::Recycle),
    ];
    let id = store.begin_action(ActionKind::Cleanup, &items).unwrap();
    for seq in 0..3 {
        clock.advance_secs(1);
        let restore = (seq != 1).then(|| RestoreInfo {
            original_path: items[seq as usize].path.clone(),
            blob: vec![seq as u8],
        });
        store.complete_item(id, seq, &done(restore)).unwrap();
    }
    store.finish_action(id, ActionStatus::Completed).unwrap();

    let r = store.restorable_items(10).unwrap();
    let paths: Vec<_> = r.iter().map(|i| i.planned.path.as_str()).collect();
    assert_eq!(paths, vec![items[2].path.as_str(), items[0].path.as_str()]);
    assert_eq!(store.restorable_items(1).unwrap().len(), 1);

    store.mark_restored(r[0].id).unwrap();
    assert!(matches!(
        store.mark_restored(r[0].id),
        Err(StoreError::NotFound(_))
    ));
    let r = store.restorable_items(10).unwrap();
    assert_eq!(r.len(), 1);
    let rec = store.action(id).unwrap();
    assert!(rec.items[2].restored_at.is_some());
}

#[test]
fn history_pages_newest_first() {
    let (_d, store, _c) = store_at(t0());
    let ids: Vec<_> = (0..5)
        .map(|n| {
            let id = store
                .begin_action(ActionKind::Duplicates, &[item(n, DeleteMethod::Recycle)])
                .unwrap();
            store.finish_action(id, ActionStatus::Completed).unwrap();
            id
        })
        .collect();
    let page1 = store.action_history(2, None).unwrap();
    assert_eq!(
        page1.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![ids[4], ids[3]]
    );
    let page2 = store.action_history(10, Some(ids[3])).unwrap();
    assert_eq!(
        page2.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![ids[2], ids[1], ids[0]]
    );
    assert!(matches!(
        store.action(ActionId(1234)),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn large_values_round_trip() {
    let (_d, store, _c) = store_at(t0());
    let mut it = item(1, DeleteMethod::RebootDelete);
    it.file_ref = FileRef(u64::MAX);
    it.mtime = FileTime(u64::MAX);
    it.volume.serial = u64::MAX;
    it.tier = Safety::Careful;
    it.path = format!(r"\\?\C:\{}\😀\עברית", "a".repeat(400));
    let id = store
        .begin_action(ActionKind::Cleanup, &[it.clone()])
        .unwrap();
    assert_eq!(store.action(id).unwrap().items[0].planned, it);
}
