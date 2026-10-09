//! Fixtures shared by the integration tests.

#![allow(dead_code)]

use std::sync::Arc;

use strata_store::{
    DirAggregate, ManualClock, ScannerKind, SnapshotId, SnapshotOptions, Store, Timestamp,
    VolumeKey, VolumeTotals,
};
use tempfile::TempDir;

pub const MIB: u64 = 1024 * 1024;
pub const GIB: u64 = 1024 * MIB;

/// A store in a fresh temp dir with a manual clock at `start`.
pub fn store_at(start: Timestamp) -> (TempDir, Store, Arc<ManualClock>) {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new(start));
    let store = Store::open_with_clock(dir.path(), clock.clone()).unwrap();
    (dir, store, clock)
}

pub fn t0() -> Timestamp {
    Timestamp::from_utc(2026, 10, 9, 12, 0, 0).unwrap()
}

pub fn volume() -> VolumeKey {
    VolumeKey {
        serial: 0xDEAD_BEEF_0000_0001,
        guid_path: r"\\?\Volume{11111111-2222-3333-4444-555555555555}\".into(),
    }
}

pub fn other_volume() -> VolumeKey {
    VolumeKey {
        serial: 2,
        guid_path: r"\\?\Volume{other}\".into(),
    }
}

pub fn totals(used: u64) -> VolumeTotals {
    VolumeTotals {
        total_bytes: 1000 * GIB,
        free_bytes: 1000 * GIB - used,
        allocated_sum: used,
        logical_sum: used - used / 10,
        file_count: 1_000_000,
        dir_count: 100_000,
        scanner: ScannerKind::Mft,
    }
}

pub fn dir(path: &str, allocated: u64) -> DirAggregate {
    DirAggregate {
        path: path.into(),
        allocated,
        logical: allocated,
        files: 10,
    }
}

/// Commits a snapshot with no size floor so tiny test directories are kept.
pub fn snap(store: &Store, used: u64, dirs: Vec<DirAggregate>) -> SnapshotId {
    snap_on(store, &volume(), used, dirs)
}

pub fn snap_on(store: &Store, v: &VolumeKey, used: u64, dirs: Vec<DirAggregate>) -> SnapshotId {
    let mut w = store.begin_snapshot_with(v, totals(used), SnapshotOptions { min_dir_bytes: 0 });
    w.add_dirs(dirs);
    w.commit().unwrap()
}

/// `n` synthetic directories shaped like a real system drive: a few deep
/// trees, sizes spread over 1 MiB-10 GiB, cluster-aligned allocations.
pub fn synthetic_dirs(n: usize, seed: u64) -> Vec<DirAggregate> {
    let mut state = seed | 1;
    let mut next = move || {
        // xorshift64*: deterministic and dependency-free.
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };
    (0..n)
        .map(|i| {
            let r = next();
            let depth = 2 + (r % 6) as usize;
            let mut path = String::from(r"C:\Users\me");
            for level in 0..depth {
                path.push_str(&format!(r"\dir{}_{}", level, (i >> (level * 2)) % 37));
            }
            path.push_str(&format!(r"\leaf{i}"));
            let allocated = ((next() % (10 * GIB)) + MIB) & !4095;
            let logical = allocated - (next() % (allocated / 8 + 1));
            DirAggregate {
                path,
                allocated,
                logical,
                files: next() % 50_000,
            }
        })
        .collect()
}
