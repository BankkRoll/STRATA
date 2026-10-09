//! Time edge cases with an injected clock: DST transitions, the clock set
//! back or far forward, timestamps before 1970 and 1601, and the extremes of
//! the timestamp range. Nothing may panic, and retention must never drop
//! the snapshot the user scanned last.

mod common;

use common::{GIB, dir, snap, store_at, t0, volume};
use strata_core::{FileRef, FileTime, Safety};
use strata_store::{
    ActionKind, ActionStatus, Clock, DeleteMethod, DiffOptions, ItemOutcome, ItemResult,
    PlannedItem, RetentionPolicy, Timestamp,
};

const DAY: i64 = 86_400;

fn at(y: i64, m: u32, d: u32, h: u32, min: u32) -> Timestamp {
    Timestamp::from_utc(y, m, d, h, min, 0).unwrap()
}

fn ids(store: &strata_store::Store) -> Vec<i64> {
    store
        .snapshots(&volume())
        .unwrap()
        .into_iter()
        .map(|s| s.id.0)
        .collect()
}

#[test]
fn extreme_calendar_inputs_are_rejected_not_panicking() {
    for year in [
        i64::MAX,
        i64::MIN,
        i64::MAX / 2,
        -1_000_000_000_000,
        300_000_000_000,
    ] {
        assert_eq!(Timestamp::from_utc(year, 1, 1, 0, 0, 0), None, "{year}");
    }
    assert!(Timestamp::from_utc(1600, 12, 31, 23, 59, 59).is_some());
    assert!(Timestamp::from_utc(-4713, 11, 24, 12, 0, 0).is_some());
    let far = Timestamp::from_utc(200_000_000_000, 12, 31, 23, 59, 59).unwrap();
    assert_eq!(far.to_civil().year, 200_000_000_000);
}

#[test]
fn extreme_timestamps_format_and_bucket_without_panicking() {
    for t in [
        Timestamp(i64::MIN),
        Timestamp(i64::MIN + 1),
        Timestamp(-11_644_473_601), // one second before 1601
        Timestamp(-1),
        Timestamp(0),
        Timestamp(i64::MAX),
    ] {
        let _ = t.to_string();
        let _ = t.to_compact_string();
        let _ = t.iso_week_index();
        let _ = t.hour_start();
        let _ = t.minus_days(u32::MAX);
        let c = t.to_civil();
        assert!((1..=12).contains(&c.month) && (1..=31).contains(&c.day));
    }
    assert_eq!(Timestamp(i64::MIN).minus_days(1), Timestamp(i64::MIN));
    let pre_1601 = Timestamp(-11_644_473_601).to_civil();
    assert_eq!(
        (pre_1601.year, pre_1601.month, pre_1601.day),
        (1600, 12, 31)
    );
}

#[test]
fn filetime_conversions_cover_the_whole_range() {
    assert_eq!(Timestamp::from_filetime(FileTime(0)), at(1601, 1, 1, 0, 0));
    let max = Timestamp::from_filetime(FileTime(u64::MAX));
    assert_eq!(max.to_civil().year, 60056);
}

#[test]
fn retention_across_dst_transitions_is_utc_only() {
    // Hourly snapshots across the 2026 EU (03-29) and US (03-08) DST
    // starts. Local clocks skip an hour; UTC does not, so each hour is a
    // distinct snapshot and weekly thinning keeps exactly one per ISO week.
    let start = at(2026, 3, 7, 0, 0);
    let (_d, store, clock) = store_at(start);
    let mut taken = Vec::new();
    for h in 0..(24 * 26) {
        if h % 6 == 0 {
            clock.set(Timestamp(start.0 + h * 3600));
            taken.push((snap(&store, GIB, vec![dir(r"C:\", GIB)]), clock.now()));
        }
    }
    let before = ids(&store).len();
    assert_eq!(before, taken.len());
    clock.set(at(2026, 6, 1, 0, 0));
    store
        .apply_retention(&RetentionPolicy {
            keep_days: 0,
            thin_after_days: 30,
        })
        .unwrap();
    let kept = store.snapshots(&volume()).unwrap();
    let weeks: std::collections::BTreeSet<i64> =
        taken.iter().map(|(_, t)| t.iso_week_index()).collect();
    assert_eq!(kept.len(), weeks.len());
    for k in &kept {
        let week = k.taken_at.iso_week_index();
        let last = taken
            .iter()
            .filter(|(_, t)| t.iso_week_index() == week)
            .map(|(id, _)| *id)
            .max()
            .unwrap();
        assert_eq!(k.id, last, "kept a snapshot that was not its week's last");
    }
}

#[test]
fn clock_set_back_keeps_the_real_latest_scan() {
    let (_d, store, clock) = store_at(t0());
    // The clock ran a year ahead for a while, then was corrected.
    clock.set(Timestamp(t0().0 + 365 * DAY));
    let wrong = snap(&store, 10 * GIB, vec![dir(r"C:\", 10 * GIB)]);
    clock.set(t0());
    let a = snap(&store, 20 * GIB, vec![dir(r"C:\", 20 * GIB)]);
    clock.advance_secs(DAY);
    let b = snap(&store, 25 * GIB, vec![dir(r"C:\", 25 * GIB)]);

    let latest = store.latest_snapshot(&volume()).unwrap().unwrap();
    assert_eq!(latest.id, b);
    let s = store
        .since_last_scan(&volume(), &DiffOptions::default())
        .unwrap()
        .unwrap();
    assert_eq!((s.from.id, s.to.id), (a, b));
    assert_eq!(s.used_delta, 5 * GIB as i64);

    // Nothing is old relative to the corrected clock: nothing is deleted.
    let r = store.apply_retention(&RetentionPolicy::default()).unwrap();
    assert_eq!(r.deleted_snapshots, 0);
    assert_eq!(ids(&store), vec![wrong.0, a.0, b.0]);
}

#[test]
fn clock_far_in_the_future_never_drops_the_last_scan() {
    let (_d, store, clock) = store_at(t0());
    let mut last = None;
    for _ in 0..10 {
        last = Some(snap(&store, GIB, vec![dir(r"C:\", GIB)]));
        clock.advance_secs(DAY);
    }
    clock.set(at(3000, 1, 1, 0, 0));
    store.apply_retention(&RetentionPolicy::default()).unwrap();
    assert_eq!(ids(&store), vec![last.unwrap().0]);
    // And a later scan under the corrected clock still becomes the latest.
    clock.set(t0());
    let next = snap(&store, 2 * GIB, vec![dir(r"C:\", 2 * GIB)]);
    assert_eq!(store.latest_snapshot(&volume()).unwrap().unwrap().id, next);
}

#[test]
fn clocks_before_1970_and_1601_work_end_to_end() {
    for now in [
        at(1969, 7, 20, 20, 17),
        at(1500, 1, 1, 0, 0),
        Timestamp(i64::MIN / 2),
    ] {
        let (_d, store, clock) = store_at(now);
        let a = snap(&store, GIB, vec![dir(r"C:\", GIB)]);
        clock.advance_secs(DAY);
        let b = snap(&store, 2 * GIB, vec![dir(r"C:\", 2 * GIB)]);
        assert_eq!(store.snapshot(a).unwrap().taken_at, now);
        assert!(
            store
                .since_last_scan(&volume(), &DiffOptions::default())
                .unwrap()
                .is_some()
        );
        store.apply_retention(&RetentionPolicy::default()).unwrap();
        assert_eq!(ids(&store), vec![a.0, b.0]);
        store.prune_activity(30).unwrap();

        let item = PlannedItem {
            path: r"C:\x.tmp".into(),
            volume: volume(),
            file_ref: FileRef(1),
            size: 1,
            mtime: FileTime(0),
            method: DeleteMethod::Permanent,
            tier: Safety::Safe,
            rule_id: None,
        };
        let id = store.begin_action(ActionKind::Cleanup, &[item]).unwrap();
        clock.advance_secs(-10 * DAY);
        store
            .complete_item(
                id,
                0,
                &ItemOutcome {
                    result: ItemResult::Done,
                    error: None,
                    restore: None,
                },
            )
            .unwrap();
        store.finish_action(id, ActionStatus::Completed).unwrap();
        let rec = store.action(id).unwrap();
        assert_eq!(rec.summary.started_at, Timestamp(now.0 + DAY));
        assert_eq!(rec.summary.done_count, 1);
    }
}

#[test]
fn retention_at_the_ends_of_the_clock_range_does_not_panic() {
    for now in [Timestamp(i64::MIN), Timestamp(i64::MAX)] {
        let (_d, store, clock) = store_at(t0());
        snap(&store, GIB, vec![dir(r"C:\", GIB)]);
        clock.advance_secs(DAY);
        snap(&store, GIB, vec![dir(r"C:\", GIB)]);
        clock.set(now);
        store.apply_retention(&RetentionPolicy::default()).unwrap();
        store.prune_activity(u32::MAX).unwrap();
        assert!(!ids(&store).is_empty());
    }
}
