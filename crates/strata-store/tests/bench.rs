//! Storage-size and latency benchmarks. Ignored by default; run with
//!
//! ```text
//! cargo test -p strata-store --release --test bench -- --ignored --nocapture
//! ```
//!
//! Results are recorded in `docs/tracks/store.md`.

mod common;

use std::path::Path;
use std::time::Instant;

use common::*;
use rusqlite::{Connection, params};
use strata_store::*;

const DIRS: usize = 50_000;

fn file_bytes(path: &Path) -> u64 {
    let conn = Connection::open(path).unwrap();
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    let pages: i64 = conn
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .unwrap();
    let size: i64 = conn
        .query_row("PRAGMA page_size", [], |r| r.get(0))
        .unwrap();
    (pages * size) as u64
}

/// Simulates the next day: most directories unchanged, some grown.
fn next_day(dirs: &[DirAggregate], day: u64) -> Vec<DirAggregate> {
    dirs.iter()
        .enumerate()
        .map(|(i, d)| {
            let mut d = d.clone();
            if (i as u64 + day).is_multiple_of(10) {
                d.allocated += 4096 * (day + 1) * 37;
                d.logical += 4000 * (day + 1) * 37;
                d.files += day;
            }
            d
        })
        .collect()
}

#[test]
#[ignore = "benchmark"]
fn bench_blob_snapshots() {
    let (tmp, store, clock) = store_at(t0());
    let base = synthetic_dirs(DIRS, 42);
    let db = tmp.path().join("history.db");
    let empty = file_bytes(&db);

    let mut w =
        store.begin_snapshot_with(&volume(), totals(GIB), SnapshotOptions { min_dir_bytes: 0 });
    w.add_dirs(base.clone());
    let start = Instant::now();
    let first = w.commit().unwrap();
    let first_ms = start.elapsed().as_secs_f64() * 1000.0;
    let after_first = file_bytes(&db);

    let mut times = Vec::new();
    let mut ids = vec![first];
    for day in 1..=29 {
        clock.advance_secs(86_400);
        let mut w = store.begin_snapshot_with(
            &volume(),
            totals(GIB + day * MIB),
            SnapshotOptions { min_dir_bytes: 0 },
        );
        w.add_dirs(next_day(&base, day));
        let start = Instant::now();
        ids.push(w.commit().unwrap());
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let after_all = file_bytes(&db);
    let per_snapshot = (after_all - after_first) / 29;

    let start = Instant::now();
    let diff = store
        .diff(
            ids[0],
            *ids.last().unwrap(),
            &DiffOptions {
                min_change: MIB,
                ..DiffOptions::default()
            },
        )
        .unwrap();
    let diff_ms = start.elapsed().as_secs_f64() * 1000.0;

    let start = Instant::now();
    let series = store
        .dir_series(&volume(), path_hash(&base[DIRS / 2].path), 30)
        .unwrap();
    let series_ms = start.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(series.len(), 30);

    clock.advance_secs(200 * 86_400);
    let start = Instant::now();
    let report = store.apply_retention(&RetentionPolicy::default()).unwrap();
    let retention_ms = start.elapsed().as_secs_f64() * 1000.0;

    times.sort_by(f64::total_cmp);
    println!("blob layout, {DIRS} dirs per snapshot");
    println!(
        "  first snapshot (new paths): {:>8.1} ms, file +{} bytes",
        first_ms,
        after_first - empty
    );
    println!(
        "  later snapshots: median {:>6.1} ms, max {:>6.1} ms",
        times[times.len() / 2],
        times[times.len() - 1]
    );
    println!(
        "  bytes per later snapshot: {per_snapshot} ({:.1} B/dir)",
        per_snapshot as f64 / DIRS as f64
    );
    println!(
        "  diff of two snapshots: {diff_ms:.1} ms ({} grown)",
        diff.grown.len()
    );
    println!("  dir_series over 30 snapshots: {series_ms:.2} ms");
    println!(
        "  retention deleting {} snapshots + GC {} paths: {retention_ms:.1} ms",
        report.deleted_snapshots, report.deleted_paths
    );
}

#[test]
#[ignore = "benchmark"]
fn bench_row_table_alternatives() {
    let base = synthetic_dirs(DIRS, 42);
    for (name, ddl, key_is_hash) in [
        (
            "WITHOUT ROWID (snapshot_id, path_id)",
            "CREATE TABLE d (snapshot_id INTEGER, path_id INTEGER, allocated INTEGER,
             logical INTEGER, files INTEGER, PRIMARY KEY (snapshot_id, path_id)) WITHOUT ROWID",
            false,
        ),
        (
            "WITHOUT ROWID (snapshot_id, path_hash)",
            "CREATE TABLE d (snapshot_id INTEGER, path_id INTEGER, allocated INTEGER,
             logical INTEGER, files INTEGER, PRIMARY KEY (snapshot_id, path_id)) WITHOUT ROWID",
            true,
        ),
        (
            "rowid table + index (snapshot_id, path_id)",
            "CREATE TABLE d (snapshot_id INTEGER, path_id INTEGER, allocated INTEGER,
             logical INTEGER, files INTEGER);
             CREATE INDEX d_idx ON d (snapshot_id, path_id)",
            false,
        ),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rows.db");
        let mut conn = Connection::open(&path).unwrap();
        conn.execute_batch(ddl).unwrap();
        let mut sizes = Vec::new();
        let mut ms = Vec::new();
        for snapshot in 0..5u64 {
            let rows = next_day(&base, snapshot);
            let start = Instant::now();
            let tx = conn.transaction().unwrap();
            {
                let mut ins = tx
                    .prepare("INSERT INTO d VALUES (?1, ?2, ?3, ?4, ?5)")
                    .unwrap();
                for (i, d) in rows.iter().enumerate() {
                    let key = if key_is_hash {
                        path_hash(&d.path) as i64
                    } else {
                        i as i64 + 1
                    };
                    ins.execute(params![
                        snapshot as i64,
                        key,
                        d.allocated as i64,
                        d.logical as i64,
                        d.files as i64
                    ])
                    .unwrap();
                }
            }
            tx.commit().unwrap();
            ms.push(start.elapsed().as_secs_f64() * 1000.0);
            sizes.push(file_bytes(&path));
        }
        let per = (sizes[4] - sizes[0]) / 4;
        println!(
            "{name}: {per} bytes/snapshot ({:.1} B/dir), insert {:.1} ms",
            per as f64 / DIRS as f64,
            ms[1]
        );
    }
}
