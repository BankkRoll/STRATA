//! Snapshot writing, series queries, diffs and retention against a real
//! database.

mod common;

use common::*;
use strata_core::SizeMode;
use strata_store::*;

#[test]
fn snapshot_round_trip_and_metadata() {
    let (_d, store, _c) = store_at(t0());
    let id = snap(
        &store,
        300 * GIB,
        vec![dir(r"C:\A", 5 * GIB), dir(r"C:\B", GIB)],
    );
    let info = store.snapshot(id).unwrap();
    assert_eq!(info.taken_at, t0());
    assert_eq!(info.volume, volume());
    assert_eq!(info.totals, totals(300 * GIB));
    assert_eq!(info.totals.used_bytes(), 300 * GIB);
    assert_eq!(info.stored_dirs, 2);
    assert_eq!(
        store.snapshot_dir(id, r"c:\a\").unwrap(),
        Some(DirSizes {
            allocated: 5 * GIB,
            logical: 5 * GIB,
            files: 10
        })
    );
    assert_eq!(store.snapshot_dir(id, r"C:\nope").unwrap(), None);
    assert!(matches!(
        store.snapshot(SnapshotId(999)),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn min_size_filter_uses_larger_of_both_sizes() {
    let (_d, store, _c) = store_at(t0());
    let mut w = store.begin_snapshot(&volume(), totals(GIB));
    let kept = w.add_dirs([
        dir(r"C:\small", MIB),
        DirAggregate {
            path: r"C:\sparse".into(),
            allocated: MIB,
            logical: 100 * GIB,
            files: 1,
        },
        dir(r"C:\big", GIB),
    ]);
    assert_eq!(kept, 2);
    assert_eq!(w.len(), 2);
    let id = w.commit().unwrap();
    assert_eq!(store.snapshot(id).unwrap().min_dir_bytes, 16 * MIB);
    assert!(store.snapshot_dir(id, r"C:\small").unwrap().is_none());
    assert!(store.snapshot_dir(id, r"C:\sparse").unwrap().is_some());
}

#[test]
fn repeated_paths_keep_the_last_value() {
    let (_d, store, _c) = store_at(t0());
    let id = snap(&store, GIB, vec![dir(r"C:\X", GIB), dir(r"c:\x", 2 * GIB)]);
    assert_eq!(store.snapshot(id).unwrap().stored_dirs, 1);
    assert_eq!(
        store.snapshot_dir(id, r"C:\X").unwrap().unwrap().allocated,
        2 * GIB
    );
}

#[test]
fn dropped_writer_writes_nothing() {
    let (_d, store, _c) = store_at(t0());
    let mut w = store.begin_snapshot(&volume(), totals(GIB));
    w.add_dirs([dir(r"C:\A", GIB)]);
    drop(w);
    assert!(store.snapshots(&volume()).unwrap().is_empty());
}

#[test]
fn empty_snapshot_is_valid() {
    let (_d, store, _c) = store_at(t0());
    let w = store.begin_snapshot(&volume(), totals(GIB));
    assert!(w.is_empty());
    let id = w.commit().unwrap();
    assert_eq!(store.snapshot(id).unwrap().stored_dirs, 0);
}

#[test]
fn usage_series_is_ordered_and_ranged_per_volume() {
    let (_d, store, clock) = store_at(t0());
    for day in 0..5u64 {
        snap(&store, (100 + day) * GIB, vec![]);
        snap_on(&store, &other_volume(), GIB, vec![]);
        clock.advance_secs(86_400);
    }
    let all = store.usage_series(&volume(), None, None).unwrap();
    assert_eq!(all.len(), 5);
    assert!(all.windows(2).all(|w| w[0].at < w[1].at));
    assert_eq!(all[4].used_bytes, 104 * GIB);
    assert_eq!(all[4].free_bytes + all[4].used_bytes, all[4].total_bytes);

    let from = Timestamp(t0().0 + 86_400);
    let to = Timestamp(t0().0 + 3 * 86_400);
    let ranged = store.usage_series(&volume(), Some(from), Some(to)).unwrap();
    assert_eq!(ranged.len(), 3, "bounds are inclusive");
    assert_eq!(ranged[0].at, from);

    let unknown = VolumeKey {
        serial: 9,
        guid_path: "x".into(),
    };
    assert!(store.usage_series(&unknown, None, None).unwrap().is_empty());

    let vols = store.volumes().unwrap();
    assert_eq!(vols.len(), 2);
    assert_eq!(vols[0].snapshot_count, 5);
    assert_eq!(vols[0].first_at, Some(t0()));
}

#[test]
fn dir_series_reports_gaps_and_limits() {
    let (_d, store, clock) = store_at(t0());
    let sizes = [Some(GIB), None, Some(3 * GIB), Some(4 * GIB)];
    for s in sizes {
        let dirs = s.map(|b| vec![dir(r"C:\Models", b)]).unwrap_or_default();
        snap(&store, 10 * GIB, dirs);
        clock.advance_secs(3600);
    }
    let h = path_hash(r"C:\Models");
    let all = store.dir_series(&volume(), h, 10).unwrap();
    let got: Vec<_> = all.iter().map(|p| p.sizes.map(|s| s.allocated)).collect();
    assert_eq!(got, sizes.to_vec());
    let last2 = store.dir_series(&volume(), h, 2).unwrap();
    assert_eq!(last2.len(), 2);
    assert_eq!(last2[1].sizes.unwrap().allocated, 4 * GIB);
    let missing = store
        .dir_series(&volume(), path_hash(r"C:\Nope"), 10)
        .unwrap();
    assert_eq!(missing.len(), 4);
    assert!(missing.iter().all(|p| p.sizes.is_none()));
}

#[test]
fn paths_are_shared_across_snapshots() {
    let (tmp, store, _c) = store_at(t0());
    let dirs = synthetic_dirs(500, 1);
    snap(&store, GIB, dirs.clone());
    snap(&store, GIB, dirs);
    drop(store);
    let conn = rusqlite::Connection::open(tmp.path().join("history.db")).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM paths", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 500);
}

fn models_scenario(store: &Store, clock: &ManualClock) -> (SnapshotId, SnapshotId) {
    let a = snap(
        store,
        100 * GIB,
        vec![
            dir(r"C:\", 100 * GIB),
            dir(r"C:\Users", 60 * GIB),
            dir(r"C:\Users\me", 50 * GIB),
            dir(r"C:\Users\me\AppData", 20 * GIB),
            dir(r"C:\Users\me\AppData\Local", 18 * GIB),
            dir(r"C:\Users\me\AppData\Local\models", 2 * GIB),
            dir(r"C:\Users\me\Downloads", 10 * GIB),
            dir(r"C:\Users\me\Downloads\a", GIB),
            dir(r"C:\Users\me\Downloads\b", GIB),
            dir(r"C:\Games", 30 * GIB),
            dir(r"C:\Games\Old", 25 * GIB),
            dir(r"C:\Games\Old\Data", 20 * GIB),
            dir(r"C:\Temp", 3 * GIB),
        ],
    );
    clock.advance_secs(7 * 86_400);
    let b = snap(
        store,
        93 * GIB,
        vec![
            dir(r"C:\", 93 * GIB),
            dir(r"C:\Users", 69 * GIB),
            dir(r"C:\Users\me", 59 * GIB),
            dir(r"C:\Users\me\AppData", 26 * GIB),
            dir(r"C:\Users\me\AppData\Local", 24 * GIB),
            dir(r"C:\Users\me\AppData\Local\models", 8 * GIB),
            // Downloads grew 3 GiB spread over both children and loose files.
            dir(r"C:\Users\me\Downloads", 13 * GIB),
            dir(r"C:\Users\me\Downloads\a", 2 * GIB),
            dir(r"C:\Users\me\Downloads\b", 2 * GIB),
            dir(r"C:\Games", 5 * GIB),
            dir(r"C:\Dev", 9 * GIB),
            dir(r"C:\Dev\node_modules", 8 * GIB),
            dir(r"C:\Temp", 3 * GIB),
        ],
    );
    (a, b)
}

#[test]
fn diff_reports_deepest_contributors() {
    let (_d, store, clock) = store_at(t0());
    let (a, b) = models_scenario(&store, &clock);
    let opts = DiffOptions {
        min_change: 512 * MIB,
        large_threshold: 4 * GIB,
        ..DiffOptions::default()
    };
    let d = store.diff(a, b, &opts).unwrap();
    assert_eq!(d.used_delta, -7 * GIB as i64);

    let grown: Vec<_> = d.grown.iter().map(|c| (c.path.as_str(), c.delta)).collect();
    // C:\Users (+9) is fully explained by C:\Users\me (+9): suppressed.
    // C:\Users\me (+9): AppData +6 (67%) and Downloads +3 do not dominate.
    // AppData -> Local -> models are each +6: only models survives.
    // Downloads (+3): children +1 each, so it is kept along with them.
    // C:\Dev (+9, new): node_modules (+8) explains 89% < 90%, so both stay.
    assert_eq!(
        grown,
        vec![
            (r"C:\Dev", 9 * GIB as i64),
            (r"C:\Users\me", 9 * GIB as i64),
            (r"C:\Dev\node_modules", 8 * GIB as i64),
            (r"C:\Users\me\AppData\Local\models", 6 * GIB as i64),
            (r"C:\Users\me\Downloads", 3 * GIB as i64),
            (r"C:\Users\me\Downloads\a", GIB as i64),
            (r"C:\Users\me\Downloads\b", GIB as i64),
        ]
    );

    let shrunk: Vec<_> = d.shrunk.iter().map(|c| c.path.as_str()).collect();
    // C:\ (-7) is more than explained by Games (-25), Games by Old (-25).
    // Old\Data (-20) covers only 80% of Old, so both are listed.
    assert_eq!(shrunk, vec![r"C:\Games\Old", r"C:\Games\Old\Data"]);

    let new: Vec<_> = d.new_large.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(new, vec![r"C:\Dev"], "topmost only");
    assert!(d.new_large[0].before.is_none());

    let deleted: Vec<_> = d.deleted_large.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(deleted, vec![r"C:\Games\Old"], "topmost only");
    assert_eq!(d.deleted_large[0].delta, -25 * GIB as i64);
}

#[test]
fn diff_without_heuristic_lists_every_ancestor() {
    let (_d, store, clock) = store_at(t0());
    let (a, b) = models_scenario(&store, &clock);
    let opts = DiffOptions {
        min_change: 512 * MIB,
        deepest_only: false,
        top_n: 100,
        ..DiffOptions::default()
    };
    let d = store.diff(a, b, &opts).unwrap();
    let grown: Vec<_> = d.grown.iter().map(|c| c.path.as_str()).collect();
    assert!(grown.contains(&r"C:\Users"));
    assert!(grown.contains(&r"C:\Users\me\AppData"));
    assert_eq!(grown.len(), 10);
}

#[test]
fn diff_top_n_threshold_mode_and_direction() {
    let (_d, store, clock) = store_at(t0());
    let a = snap(
        &store,
        GIB,
        vec![DirAggregate {
            path: r"C:\Sparse".into(),
            allocated: GIB,
            logical: GIB,
            files: 1,
        }],
    );
    clock.advance_secs(60);
    let b = snap(
        &store,
        GIB,
        vec![DirAggregate {
            path: r"C:\Sparse".into(),
            allocated: GIB,
            logical: 50 * GIB,
            files: 1,
        }],
    );
    let alloc = store.diff(a, b, &DiffOptions::default()).unwrap();
    assert!(alloc.grown.is_empty());
    let logical = store
        .diff(
            a,
            b,
            &DiffOptions {
                size_mode: SizeMode::Logical,
                ..DiffOptions::default()
            },
        )
        .unwrap();
    assert_eq!(logical.grown[0].delta, 49 * GIB as i64);
    let reversed = store
        .diff(
            b,
            a,
            &DiffOptions {
                size_mode: SizeMode::Logical,
                top_n: 0,
                ..DiffOptions::default()
            },
        )
        .unwrap();
    assert!(reversed.shrunk.is_empty(), "top_n = 0");
    let reversed = store
        .diff(
            b,
            a,
            &DiffOptions {
                size_mode: SizeMode::Logical,
                ..DiffOptions::default()
            },
        )
        .unwrap();
    assert_eq!(reversed.shrunk[0].delta, -49 * GIB as i64);
}

#[test]
fn diff_rejects_bad_input() {
    let (_d, store, _c) = store_at(t0());
    let a = snap(&store, GIB, vec![]);
    let b = snap_on(&store, &other_volume(), GIB, vec![]);
    assert!(matches!(
        store.diff(a, b, &DiffOptions::default()),
        Err(StoreError::InvalidInput(_))
    ));
    let bad = DiffOptions {
        dominance: f64::NAN,
        ..DiffOptions::default()
    };
    assert!(matches!(
        store.diff(a, a, &bad),
        Err(StoreError::InvalidInput(_))
    ));
    assert!(matches!(
        store.diff(a, SnapshotId(77), &DiffOptions::default()),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn since_last_scan_compares_latest_two() {
    let (_d, store, clock) = store_at(t0());
    assert!(
        store
            .since_last_scan(&volume(), &DiffOptions::default())
            .unwrap()
            .is_none()
    );
    let (_, b) = models_scenario(&store, &clock);
    assert!(
        store
            .since_last_scan(&volume(), &DiffOptions::default())
            .unwrap()
            .is_some()
    );
    clock.advance_secs(86_400);
    let c = snap(
        &store,
        99 * GIB,
        vec![
            dir(r"C:\", 99 * GIB),
            dir(r"C:\Users", 75 * GIB),
            dir(r"C:\Users\me", 65 * GIB),
            dir(r"C:\Users\me\AppData", 32 * GIB),
            dir(r"C:\Users\me\AppData\Local", 30 * GIB),
            dir(r"C:\Users\me\AppData\Local\models", 14 * GIB),
        ],
    );
    let s = store
        .since_last_scan(&volume(), &DiffOptions::default())
        .unwrap()
        .unwrap();
    assert_eq!(s.from.id, b);
    assert_eq!(s.to.id, c);
    assert_eq!(s.used_delta, 6 * GIB as i64);
    let biggest = s.biggest.unwrap();
    assert_eq!(biggest.path, r"C:\Users\me\AppData\Local\models");
    assert_eq!(biggest.delta, 6 * GIB as i64);
}

#[test]
fn retention_deletes_and_collects_paths() {
    let (tmp, store, clock) = store_at(t0());
    // 100 daily snapshots, each with one directory unique to that day.
    for day in 0..100 {
        snap(
            &store,
            GIB,
            vec![dir(r"C:\Shared", GIB), dir(&format!(r"C:\Day{day}"), GIB)],
        );
        clock.advance_secs(86_400);
    }
    clock.advance_secs(-86_400);
    let report = store.apply_retention(&RetentionPolicy::default()).unwrap();
    let left = store.snapshots(&volume()).unwrap();
    assert_eq!(report.deleted_snapshots as usize, 100 - left.len());
    // 31 recent days (0..=30 days old) plus ~9 weekly survivors in 31..=90.
    assert!((38..=41).contains(&left.len()), "{}", left.len());
    assert_eq!(report.deleted_paths, report.deleted_snapshots);

    drop(store);
    let conn = rusqlite::Connection::open(tmp.path().join("history.db")).unwrap();
    let paths: i64 = conn
        .query_row("SELECT count(*) FROM paths", [], |r| r.get(0))
        .unwrap();
    assert_eq!(paths as usize, left.len() + 1, "one per day + shared");
    let blobs: i64 = conn
        .query_row("SELECT count(*) FROM snapshot_dirs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(blobs as usize, left.len(), "blobs cascade with snapshots");
}

#[test]
fn retention_is_idempotent() {
    let (_d, store, clock) = store_at(t0());
    for _ in 0..60 {
        snap(&store, GIB, vec![dir(r"C:\A", GIB)]);
        clock.advance_secs(86_400);
    }
    let first = store.apply_retention(&RetentionPolicy::default()).unwrap();
    assert!(first.deleted_snapshots > 0);
    let second = store.apply_retention(&RetentionPolicy::default()).unwrap();
    assert_eq!(second, RetentionReport::default());
}

#[test]
fn clear_history_keeps_other_data() {
    let (_d, store, _c) = store_at(t0());
    snap(&store, GIB, vec![dir(r"C:\A", GIB)]);
    store
        .upsert_hashes(
            &volume(),
            &[CachedHash {
                key: HashKey {
                    file_ref: strata_core::FileRef(1),
                    size: 1,
                    mtime: strata_core::FileTime(1),
                },
                partial: 1,
                full: None,
            }],
        )
        .unwrap();
    store.clear_history().unwrap();
    assert!(store.snapshots(&volume()).unwrap().is_empty());
    let hit = store
        .lookup_hashes(
            &volume(),
            &[HashKey {
                file_ref: strata_core::FileRef(1),
                size: 1,
                mtime: strata_core::FileTime(1),
            }],
        )
        .unwrap();
    assert!(hit[0].is_some());
}

#[test]
fn fifty_thousand_dirs_round_trip() {
    let (_d, store, _c) = store_at(t0());
    let dirs = synthetic_dirs(50_000, 7);
    let mut w =
        store.begin_snapshot_with(&volume(), totals(GIB), SnapshotOptions { min_dir_bytes: 0 });
    w.add_dirs(dirs.clone());
    let start = std::time::Instant::now();
    let id = w.commit().unwrap();
    let elapsed = start.elapsed();
    // Generous bound so unoptimized CI builds pass; the release benchmark
    // in tests/bench.rs measures the real number.
    assert!(elapsed.as_secs() < 10, "{elapsed:?}");
    assert_eq!(store.snapshot(id).unwrap().stored_dirs, 50_000);
    for d in dirs.iter().step_by(997) {
        let got = store.snapshot_dir(id, &d.path).unwrap().unwrap();
        assert_eq!(
            (got.allocated, got.logical, got.files),
            (d.allocated, d.logical, d.files)
        );
    }
}
