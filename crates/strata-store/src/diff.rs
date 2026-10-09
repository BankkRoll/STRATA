//! "What changed" between two snapshots, and the home-screen banner summary.
//!
//! # Deepest significant contributor
//!
//! When `C:\Users\me\AppData\Local\Models\llama` grows by 6 GB, every ancestor
//! grows by at least 6 GB too, and a naive "top grown" list is five rows of
//! the same news. With [`DiffOptions::deepest_only`] a directory is dropped
//! from the grown (or shrunk) list when one of its *direct children* is also
//! in that list and accounts for at least [`DiffOptions::dominance`] (default
//! 90%) of the parent's change. Applied at every level, this keeps the deepest
//! directory whose change is not mostly explained by a single child:
//!
//! - growth concentrated in one leaf reports only that leaf;
//! - growth spread over many children (none reaching 90%) reports the parent,
//!   which is the useful answer ("Downloads grew by 3 GB across 40 files");
//! - a child that is below `min_change` never suppresses its parent, so
//!   nothing disappears from the list without a deeper row replacing it.
//!
//! Only directories present in a snapshot (at or above its minimum size) take
//! part, so growth spread over many small subfolders is attributed to their
//! nearest stored ancestor.
//!
//! New and deleted large folders are reported topmost-only instead: a new
//! `node_modules` tree is one row, not one row per large package inside it.
//! "New" means not stored in the older snapshot, which includes a folder that
//! was below that snapshot's minimum size; likewise "deleted" includes a
//! folder that fell below the newer snapshot's minimum.

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use strata_core::SizeMode;

use crate::error::{DbKind, Result, StoreError};
use crate::snapshot::{
    DirSizes, SnapshotId, SnapshotInfo, VolumeKey, find_volume, latest_snapshots, load_dirs,
    load_snapshot,
};
use crate::{Store, i2u};

/// Parameters of a snapshot diff.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DiffOptions {
    /// Which size to compare.
    pub size_mode: SizeMode,
    /// Maximum rows per list.
    pub top_n: usize,
    /// Smallest change (bytes) that makes a directory "grown" or "shrunk".
    pub min_change: u64,
    /// Smallest size (bytes) for the new/deleted large folder lists.
    pub large_threshold: u64,
    /// Suppress parents whose change is explained by one child. See the
    /// module docs.
    pub deepest_only: bool,
    /// Fraction of a parent's change a single child must explain to suppress
    /// the parent. Must be finite and positive; values above 1 only suppress
    /// a parent when a child changed more than it (siblings moved the other
    /// way).
    pub dominance: f64,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            size_mode: SizeMode::Allocated,
            top_n: 20,
            min_change: 64 * 1024 * 1024,
            large_threshold: 1024 * 1024 * 1024,
            deepest_only: true,
            dominance: 0.9,
        }
    }
}

/// One directory in a diff list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirChange {
    /// [`path_hash`](crate::path_hash) of the directory.
    pub path_hash: u64,
    /// Display path as first recorded.
    pub path: String,
    /// Sizes in the older snapshot (`None` when not stored there).
    pub before: Option<DirSizes>,
    /// Sizes in the newer snapshot (`None` when not stored there).
    pub after: Option<DirSizes>,
    /// Change in the selected size mode (positive = grew).
    pub delta: i64,
}

/// Result of [`Store::diff`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotDiff {
    /// Older snapshot.
    pub from: SnapshotInfo,
    /// Newer snapshot.
    pub to: SnapshotInfo,
    /// Change in volume used bytes (`total - free`).
    pub used_delta: i64,
    /// Change in the scanned sum for the selected size mode.
    pub scanned_delta: i64,
    /// Largest growth first.
    pub grown: Vec<DirChange>,
    /// Largest shrinkage first (most negative delta first).
    pub shrunk: Vec<DirChange>,
    /// Large folders not stored in `from`, largest first, topmost only.
    pub new_large: Vec<DirChange>,
    /// Large folders not stored in `to`, largest first, topmost only.
    pub deleted_large: Vec<DirChange>,
}

/// Result of [`Store::since_last_scan`], for the home-screen banner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinceLastScan {
    /// Previous snapshot.
    pub from: SnapshotInfo,
    /// Latest snapshot.
    pub to: SnapshotInfo,
    /// Change in volume used bytes.
    pub used_delta: i64,
    /// Change in the scanned sum for the selected size mode.
    pub scanned_delta: i64,
    /// The deepest significant directory moving in the same direction as
    /// `used_delta` (top grown when it rose, top shrunk when it fell).
    pub biggest: Option<DirChange>,
}

const SQL_PATH_BY_ID: &str = "
SELECT hash, parent_hash, path FROM paths WHERE id = ?1";

struct PathInfo {
    hash: u64,
    parent_hash: Option<u64>,
    path: String,
}

#[derive(Clone, Copy)]
struct Entry {
    id: i64,
    before: Option<DirSizes>,
    after: Option<DirSizes>,
    delta: i64,
}

fn metric(s: Option<DirSizes>, mode: SizeMode) -> u64 {
    s.map_or(0, |s| match mode {
        SizeMode::Allocated => s.allocated,
        SizeMode::Logical => s.logical,
    })
}

fn signed_delta(before: u64, after: u64) -> i64 {
    let d = i128::from(after) - i128::from(before);
    i64::try_from(d).unwrap_or(if d < 0 { i64::MIN } else { i64::MAX })
}

/// Drops entries whose change is explained by one candidate child.
fn keep_deepest(entries: Vec<Entry>, paths: &HashMap<i64, PathInfo>, dominance: f64) -> Vec<Entry> {
    let mut max_child: HashMap<u64, u64> = HashMap::new();
    for e in &entries {
        if let Some(parent) = paths.get(&e.id).and_then(|p| p.parent_hash) {
            let m = max_child.entry(parent).or_default();
            *m = (*m).max(e.delta.unsigned_abs());
        }
    }
    entries
        .into_iter()
        .filter(|e| {
            let Some(info) = paths.get(&e.id) else {
                return true;
            };
            max_child
                .get(&info.hash)
                .is_none_or(|&child| (child as f64) < dominance * e.delta.unsigned_abs() as f64)
        })
        .collect()
}

/// Drops entries whose parent is also an entry.
fn keep_topmost(entries: Vec<Entry>, paths: &HashMap<i64, PathInfo>) -> Vec<Entry> {
    let hashes: HashSet<u64> = entries
        .iter()
        .filter_map(|e| paths.get(&e.id).map(|p| p.hash))
        .collect();
    entries
        .into_iter()
        .filter(|e| {
            paths
                .get(&e.id)
                .and_then(|p| p.parent_hash)
                .is_none_or(|parent| !hashes.contains(&parent))
        })
        .collect()
}

fn load_paths(conn: &Connection, ids: impl Iterator<Item = i64>) -> Result<HashMap<i64, PathInfo>> {
    let mut stmt = conn.prepare_cached(SQL_PATH_BY_ID)?;
    let mut out = HashMap::new();
    for id in ids {
        if out.contains_key(&id) {
            continue;
        }
        let info = stmt
            .query_row([id], |r| {
                Ok(PathInfo {
                    hash: i2u(r.get(0)?),
                    parent_hash: r.get::<_, Option<i64>>(1)?.map(i2u),
                    path: r.get(2)?,
                })
            })
            .optional()?
            .ok_or_else(|| StoreError::Corrupt {
                db: DbKind::History,
                detail: format!("snapshot references missing path id {id}"),
            })?;
        out.insert(id, info);
    }
    Ok(out)
}

fn validate(opts: &DiffOptions) -> Result<()> {
    if !opts.dominance.is_finite() || opts.dominance <= 0.0 {
        return Err(StoreError::InvalidInput(format!(
            "dominance must be finite and positive, got {}",
            opts.dominance
        )));
    }
    Ok(())
}

pub(crate) fn compute_diff(
    conn: &Connection,
    from: SnapshotId,
    to: SnapshotId,
    opts: &DiffOptions,
) -> Result<SnapshotDiff> {
    validate(opts)?;
    let from_info = load_snapshot(conn, from)?;
    let to_info = load_snapshot(conn, to)?;
    if from_info.volume != to_info.volume {
        return Err(StoreError::InvalidInput(format!(
            "snapshots {} and {} belong to different volumes",
            from.0, to.0
        )));
    }
    let mode = opts.size_mode;
    let before: HashMap<i64, DirSizes> = load_dirs(conn, from)?.into_iter().collect();
    let after = load_dirs(conn, to)?;

    let mut entries: Vec<Entry> = Vec::with_capacity(before.len().max(after.len()));
    let mut seen = HashSet::with_capacity(after.len());
    for (id, a) in &after {
        seen.insert(*id);
        let b = before.get(id).copied();
        entries.push(Entry {
            id: *id,
            before: b,
            after: Some(*a),
            delta: signed_delta(metric(b, mode), metric(Some(*a), mode)),
        });
    }
    for (id, b) in &before {
        if !seen.contains(id) {
            entries.push(Entry {
                id: *id,
                before: Some(*b),
                after: None,
                delta: signed_delta(metric(Some(*b), mode), 0),
            });
        }
    }

    let min_change = opts.min_change.max(1);
    let mut grown: Vec<Entry> = Vec::new();
    let mut shrunk: Vec<Entry> = Vec::new();
    let mut new_large: Vec<Entry> = Vec::new();
    let mut deleted_large: Vec<Entry> = Vec::new();
    for e in entries {
        if e.delta > 0 && e.delta.unsigned_abs() >= min_change {
            grown.push(e);
        } else if e.delta < 0 && e.delta.unsigned_abs() >= min_change {
            shrunk.push(e);
        }
        if e.before.is_none() && metric(e.after, mode) >= opts.large_threshold {
            new_large.push(e);
        }
        if e.after.is_none() && metric(e.before, mode) >= opts.large_threshold {
            deleted_large.push(e);
        }
    }

    let ids = grown
        .iter()
        .chain(&shrunk)
        .chain(&new_large)
        .chain(&deleted_large)
        .map(|e| e.id);
    let paths = load_paths(conn, ids)?;

    if opts.deepest_only {
        grown = keep_deepest(grown, &paths, opts.dominance);
        shrunk = keep_deepest(shrunk, &paths, opts.dominance);
    }
    new_large = keep_topmost(new_large, &paths);
    deleted_large = keep_topmost(deleted_large, &paths);

    // Ties break on the display path so output never depends on id order.
    let path_of = |e: &Entry| paths.get(&e.id).map(|p| p.path.as_str());
    grown.sort_by(|a, b| b.delta.cmp(&a.delta).then(path_of(a).cmp(&path_of(b))));
    shrunk.sort_by(|a, b| a.delta.cmp(&b.delta).then(path_of(a).cmp(&path_of(b))));
    new_large.sort_by(|a, b| {
        metric(b.after, mode)
            .cmp(&metric(a.after, mode))
            .then(path_of(a).cmp(&path_of(b)))
    });
    deleted_large.sort_by(|a, b| {
        metric(b.before, mode)
            .cmp(&metric(a.before, mode))
            .then(path_of(a).cmp(&path_of(b)))
    });

    let finish = |list: Vec<Entry>| -> Vec<DirChange> {
        list.into_iter()
            .take(opts.top_n)
            .filter_map(|e| {
                let p = paths.get(&e.id)?;
                Some(DirChange {
                    path_hash: p.hash,
                    path: p.path.clone(),
                    before: e.before,
                    after: e.after,
                    delta: e.delta,
                })
            })
            .collect()
    };

    let scanned = |info: &SnapshotInfo| match mode {
        SizeMode::Allocated => info.totals.allocated_sum,
        SizeMode::Logical => info.totals.logical_sum,
    };
    Ok(SnapshotDiff {
        used_delta: signed_delta(from_info.totals.used_bytes(), to_info.totals.used_bytes()),
        scanned_delta: signed_delta(scanned(&from_info), scanned(&to_info)),
        grown: finish(grown),
        shrunk: finish(shrunk),
        new_large: finish(new_large),
        deleted_large: finish(deleted_large),
        from: from_info,
        to: to_info,
    })
}

impl Store {
    /// Compares two snapshots of the same volume. `from` is normally the
    /// older one; swapping them inverts every delta.
    ///
    /// # Example
    ///
    /// ```
    /// # use strata_store::*;
    /// # let dir = tempfile::tempdir().unwrap();
    /// # let clock = std::sync::Arc::new(ManualClock::new(Timestamp(1_000_000)));
    /// # let store = Store::open_with_clock(dir.path(), clock.clone()).unwrap();
    /// # let volume = VolumeKey { serial: 1, guid_path: "v".into() };
    /// # let totals = VolumeTotals { total_bytes: 100 << 30, free_bytes: 50 << 30,
    /// #     allocated_sum: 50 << 30, logical_sum: 50 << 30, file_count: 1, dir_count: 1,
    /// #     scanner: ScannerKind::Mft };
    /// let dir = |path: &str, gib: u64| DirAggregate {
    ///     path: path.into(), allocated: gib << 30, logical: gib << 30, files: 1,
    /// };
    /// let mut a = store.begin_snapshot(&volume, totals);
    /// a.add_dirs([dir(r"C:\", 40), dir(r"C:\Models", 1)]);
    /// let a = a.commit().unwrap();
    /// clock.advance_secs(86_400);
    /// let mut b = store.begin_snapshot(&volume, totals);
    /// b.add_dirs([dir(r"C:\", 46), dir(r"C:\Models", 7)]);
    /// let b = b.commit().unwrap();
    ///
    /// let diff = store.diff(a, b, &DiffOptions::default()).unwrap();
    /// // C:\ grew 6 GiB, entirely explained by C:\Models, so only the child is listed.
    /// assert_eq!(diff.grown.len(), 1);
    /// assert_eq!(diff.grown[0].path, r"C:\Models");
    /// ```
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] for an unknown snapshot,
    /// [`StoreError::InvalidInput`] for snapshots of different volumes or bad
    /// options.
    pub fn diff(
        &self,
        from: SnapshotId,
        to: SnapshotId,
        opts: &DiffOptions,
    ) -> Result<SnapshotDiff> {
        self.history().read(|c| compute_diff(c, from, to, opts))
    }

    /// Change between the volume's two most recent snapshots, or `None` when
    /// it has fewer than two.
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn since_last_scan(
        &self,
        volume: &VolumeKey,
        opts: &DiffOptions,
    ) -> Result<Option<SinceLastScan>> {
        self.history().read(|c| {
            let Some(vid) = find_volume(c, volume)? else {
                return Ok(None);
            };
            let latest = latest_snapshots(c, vid, 2)?;
            let [to, from] = latest.as_slice() else {
                return Ok(None);
            };
            let opts = DiffOptions { top_n: 1, ..*opts };
            let diff = compute_diff(c, from.id, to.id, &opts)?;
            let biggest = if diff.used_delta >= 0 {
                diff.grown.into_iter().next()
            } else {
                diff.shrunk.into_iter().next()
            };
            Ok(Some(SinceLastScan {
                from: diff.from,
                to: diff.to,
                used_delta: diff.used_delta,
                scanned_delta: diff.scanned_delta,
                biggest,
            }))
        })
    }
}
