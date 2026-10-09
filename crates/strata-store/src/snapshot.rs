//! Volume snapshots: writing them and reading series back.
//!
//! A snapshot is one row of volume totals plus the aggregates of every
//! directory at or above a minimum size, keyed by [`path_hash`]. Display paths
//! are deduplicated across snapshots in the `paths` table; the per-snapshot
//! rows are packed into one blob (see [`crate::codec`]).
//!
//! The input API is independent of the in-memory index: the caller walks its
//! own tree and feeds [`DirAggregate`]s to a [`SnapshotWriter`].

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::codec::{self, CODEC_V1};
use crate::error::{DbKind, Result, StoreError};
use crate::path::{normalize_path, normalized_parent, path_hash};
use crate::{Store, i2u, u2i};

// -----------------------------------------------------------------------------
// Types
// -----------------------------------------------------------------------------

/// Stable identity of a volume across drive-letter changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VolumeKey {
    /// Volume serial number (64-bit on NTFS, 32-bit elsewhere).
    pub serial: u64,
    /// Volume GUID path, e.g. `\\?\Volume{...}\`.
    pub guid_path: String,
}

/// Which scanner produced a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScannerKind {
    /// Raw MFT scanner (`strata-ntfs`).
    Mft,
    /// Fallback directory walker (`strata-walk`).
    Walker,
}

impl ScannerKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Mft => "mft",
            Self::Walker => "walker",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "mft" => Some(Self::Mft),
            "walker" => Some(Self::Walker),
            _ => None,
        }
    }
}

/// Volume-level totals recorded with each snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeTotals {
    /// Volume capacity in bytes.
    pub total_bytes: u64,
    /// Free bytes reported by the volume.
    pub free_bytes: u64,
    /// Sum of allocated sizes over everything scanned.
    pub allocated_sum: u64,
    /// Sum of logical sizes over everything scanned.
    pub logical_sum: u64,
    /// Files scanned.
    pub file_count: u64,
    /// Directories scanned.
    pub dir_count: u64,
    /// Scanner that produced the numbers.
    pub scanner: ScannerKind,
}

impl VolumeTotals {
    /// Used bytes as the volume reports them (`total - free`).
    #[must_use]
    pub const fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.free_bytes)
    }
}

/// Aggregate sizes of one directory subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DirSizes {
    /// Allocated bytes of the whole subtree.
    pub allocated: u64,
    /// Logical bytes of the whole subtree.
    pub logical: u64,
    /// Files in the whole subtree.
    pub files: u64,
}

/// One directory fed into a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirAggregate {
    /// Display path (stored once in `paths`; hashed via [`path_hash`]).
    pub path: String,
    /// Allocated bytes of the subtree.
    pub allocated: u64,
    /// Logical bytes of the subtree.
    pub logical: u64,
    /// Files in the subtree.
    pub files: u64,
}

/// Row id of a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SnapshotId(pub i64);

/// Snapshot metadata (everything except the directory rows).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    /// Row id.
    pub id: SnapshotId,
    /// Volume the snapshot belongs to.
    pub volume: VolumeKey,
    /// When the scan behind it was taken.
    pub taken_at: Timestamp,
    /// Volume totals.
    pub totals: VolumeTotals,
    /// Directory size threshold used when it was written.
    pub min_dir_bytes: u64,
    /// Directory rows stored.
    pub stored_dirs: u64,
}

/// Options for writing a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotOptions {
    /// Directories whose allocated *and* logical sizes are both below this are
    /// dropped. Taking the larger of the two keeps sparse and compressed
    /// folders visible in either size mode.
    pub min_dir_bytes: u64,
}

impl Default for SnapshotOptions {
    /// 16 MiB: on a typical system drive this keeps 5-50k directories, which
    /// covers everything a "what grew" view can usefully show.
    fn default() -> Self {
        Self {
            min_dir_bytes: 16 * 1024 * 1024,
        }
    }
}

/// One point of [`Store::usage_series`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsagePoint {
    /// Snapshot the point comes from.
    pub snapshot: SnapshotId,
    /// When it was taken.
    pub at: Timestamp,
    /// Volume capacity.
    pub total_bytes: u64,
    /// Used bytes reported by the volume.
    pub used_bytes: u64,
    /// Free bytes reported by the volume.
    pub free_bytes: u64,
    /// Σ allocated over scanned files.
    pub allocated_sum: u64,
    /// Σ logical over scanned files.
    pub logical_sum: u64,
}

/// One point of [`Store::dir_series`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirPoint {
    /// Snapshot the point comes from.
    pub snapshot: SnapshotId,
    /// When it was taken.
    pub at: Timestamp,
    /// Sizes, or `None` when the directory was absent or below that
    /// snapshot's minimum size.
    pub sizes: Option<DirSizes>,
}

/// Summary of one volume's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSummary {
    /// The volume.
    pub key: VolumeKey,
    /// Snapshots stored.
    pub snapshot_count: u64,
    /// Oldest snapshot time.
    pub first_at: Option<Timestamp>,
    /// Newest snapshot time.
    pub last_at: Option<Timestamp>,
}

// -----------------------------------------------------------------------------
// SQL
// -----------------------------------------------------------------------------

const SQL_INSERT_VOLUME: &str = "
INSERT INTO volumes (serial, guid_path)
VALUES (?1, ?2)
ON CONFLICT (serial, guid_path) DO NOTHING";

const SQL_SELECT_VOLUME: &str = "
SELECT id FROM volumes WHERE serial = ?1 AND guid_path = ?2";

const SQL_SELECT_PATH_ID: &str = "
SELECT id FROM paths WHERE hash = ?1";

const SQL_INSERT_PATH: &str = "
INSERT INTO paths (hash, parent_hash, path)
VALUES (?1, ?2, ?3)";

const SQL_INSERT_SNAPSHOT: &str = "
INSERT INTO snapshots (
    volume_id, taken_at, total_bytes, free_bytes, allocated_sum, logical_sum,
    file_count, dir_count, scanner, min_dir_bytes, stored_dirs
)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)";

const SQL_INSERT_SNAPSHOT_DIRS: &str = "
INSERT INTO snapshot_dirs (snapshot_id, codec, data)
VALUES (?1, ?2, ?3)";

const SQL_SNAPSHOT_SELECT: &str = "
SELECT s.id, v.serial, v.guid_path, s.taken_at, s.total_bytes, s.free_bytes,
       s.allocated_sum, s.logical_sum, s.file_count, s.dir_count, s.scanner,
       s.min_dir_bytes, s.stored_dirs
FROM snapshots AS s
JOIN volumes AS v ON v.id = s.volume_id";

const SQL_SNAPSHOT_DIRS: &str = "
SELECT codec, data FROM snapshot_dirs WHERE snapshot_id = ?1";

const SQL_USAGE_SERIES: &str = "
SELECT s.id, s.taken_at, s.total_bytes, s.free_bytes, s.allocated_sum, s.logical_sum
FROM snapshots AS s
WHERE s.volume_id = ?1 AND s.taken_at >= ?2 AND s.taken_at <= ?3
ORDER BY s.taken_at, s.id";

const SQL_LAST_N_SNAPSHOTS: &str = "
SELECT s.id, s.taken_at, d.codec, d.data
FROM snapshots AS s
JOIN snapshot_dirs AS d ON d.snapshot_id = s.id
WHERE s.volume_id = ?1
ORDER BY s.taken_at DESC, s.id DESC
LIMIT ?2";

const SQL_VOLUME_SUMMARIES: &str = "
SELECT v.serial, v.guid_path, count(s.id), min(s.taken_at), max(s.taken_at)
FROM volumes AS v
LEFT JOIN snapshots AS s ON s.volume_id = v.id
GROUP BY v.id
ORDER BY v.id";

const SQL_CLEAR_SNAPSHOTS: &str = "
DELETE FROM snapshots;
DELETE FROM paths;";

// -----------------------------------------------------------------------------
// Row helpers
// -----------------------------------------------------------------------------

pub(crate) fn ensure_volume(conn: &Connection, key: &VolumeKey) -> Result<i64> {
    conn.prepare_cached(SQL_INSERT_VOLUME)?
        .execute(params![u2i(key.serial), key.guid_path])?;
    Ok(conn
        .prepare_cached(SQL_SELECT_VOLUME)?
        .query_row(params![u2i(key.serial), key.guid_path], |r| r.get(0))?)
}

pub(crate) fn find_volume(conn: &Connection, key: &VolumeKey) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached(SQL_SELECT_VOLUME)?
        .query_row(params![u2i(key.serial), key.guid_path], |r| r.get(0))
        .optional()?)
}

fn bad_column(idx: usize, what: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        idx,
        rusqlite::types::Type::Text,
        format!("unrecognized {what}").into(),
    )
}

pub(crate) fn snapshot_from_row(r: &Row<'_>) -> rusqlite::Result<SnapshotInfo> {
    let scanner: String = r.get(10)?;
    Ok(SnapshotInfo {
        id: SnapshotId(r.get(0)?),
        volume: VolumeKey {
            serial: i2u(r.get(1)?),
            guid_path: r.get(2)?,
        },
        taken_at: Timestamp(r.get(3)?),
        totals: VolumeTotals {
            total_bytes: i2u(r.get(4)?),
            free_bytes: i2u(r.get(5)?),
            allocated_sum: i2u(r.get(6)?),
            logical_sum: i2u(r.get(7)?),
            file_count: i2u(r.get(8)?),
            dir_count: i2u(r.get(9)?),
            scanner: ScannerKind::parse(&scanner).ok_or_else(|| bad_column(10, "scanner"))?,
        },
        min_dir_bytes: i2u(r.get(11)?),
        stored_dirs: i2u(r.get(12)?),
    })
}

pub(crate) fn load_snapshot(conn: &Connection, id: SnapshotId) -> Result<SnapshotInfo> {
    conn.prepare_cached(&format!("{SQL_SNAPSHOT_SELECT} WHERE s.id = ?1"))?
        .query_row([id.0], snapshot_from_row)
        .optional()?
        .ok_or_else(|| StoreError::NotFound(format!("snapshot {}", id.0)))
}

/// Latest snapshots of a volume, newest first.
///
/// "Newest" is commit order, not `taken_at`: after the system clock is set
/// back, a snapshot taken under the wrong (later) time must not outrank the
/// scans that followed it.
pub(crate) fn latest_snapshots(
    conn: &Connection,
    volume_id: i64,
    limit: usize,
) -> Result<Vec<SnapshotInfo>> {
    let mut stmt = conn.prepare_cached(&format!(
        "{SQL_SNAPSHOT_SELECT} WHERE s.volume_id = ?1 ORDER BY s.id DESC LIMIT ?2"
    ))?;
    let rows = stmt
        .query_map(
            params![volume_id, i64::try_from(limit).unwrap_or(i64::MAX)],
            snapshot_from_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn blob_error(snapshot: i64, what: &str) -> StoreError {
    StoreError::Corrupt {
        db: DbKind::History,
        detail: format!("snapshot {snapshot} directory blob: {what}"),
    }
}

fn check_codec(snapshot: i64, codec: i64) -> Result<()> {
    if codec == CODEC_V1 {
        Ok(())
    } else {
        Err(blob_error(snapshot, &format!("unknown codec {codec}")))
    }
}

/// Every `(path id, sizes)` row of a snapshot, sorted by path id.
pub(crate) fn load_dirs(conn: &Connection, id: SnapshotId) -> Result<Vec<(i64, DirSizes)>> {
    let (codec_id, data): (i64, Vec<u8>) = conn
        .prepare_cached(SQL_SNAPSHOT_DIRS)?
        .query_row([id.0], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?
        .ok_or_else(|| StoreError::NotFound(format!("snapshot {}", id.0)))?;
    check_codec(id.0, codec_id)?;
    codec::decode(&data).map_err(|e| blob_error(id.0, e.0))
}

/// Visits every path id referenced by any snapshot blob.
pub(crate) fn for_each_referenced_path(conn: &Connection, mut f: impl FnMut(i64)) -> Result<()> {
    let mut stmt = conn.prepare("SELECT snapshot_id, codec, data FROM snapshot_dirs")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        check_codec(id, row.get(1)?)?;
        let data = row.get_ref(2)?.as_blob().map_err(rusqlite::Error::from)?;
        codec::for_each(data, |path_id, _| {
            f(path_id);
            true
        })
        .map_err(|e| blob_error(id, e.0))?;
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Writer
// -----------------------------------------------------------------------------

/// Collects directory aggregates for one snapshot and writes them in a single
/// transaction on [`commit`](Self::commit). Dropping it without committing
/// writes nothing.
///
/// Created by [`Store::begin_snapshot`].
#[derive(Debug)]
pub struct SnapshotWriter {
    store: Store,
    volume: VolumeKey,
    totals: VolumeTotals,
    taken_at: Timestamp,
    min_dir_bytes: u64,
    /// Keyed by path hash so a repeated path replaces the earlier entry.
    dirs: HashMap<u64, PendingDir>,
}

#[derive(Debug)]
struct PendingDir {
    path: String,
    parent_hash: Option<u64>,
    sizes: DirSizes,
}

impl SnapshotWriter {
    /// Adds directories, skipping those below the minimum size. Returns how
    /// many were kept. May be called any number of times before `commit`.
    pub fn add_dirs<I>(&mut self, dirs: I) -> usize
    where
        I: IntoIterator<Item = DirAggregate>,
    {
        let mut kept = 0;
        for d in dirs {
            if d.allocated.max(d.logical) < self.min_dir_bytes {
                continue;
            }
            let normalized = normalize_path(&d.path);
            let hash = xxhash_rust::xxh3::xxh3_64(normalized.as_bytes());
            let parent_hash =
                normalized_parent(&normalized).map(|p| xxhash_rust::xxh3::xxh3_64(p.as_bytes()));
            self.dirs.insert(
                hash,
                PendingDir {
                    path: d.path,
                    parent_hash,
                    sizes: DirSizes {
                        allocated: d.allocated,
                        logical: d.logical,
                        files: d.files,
                    },
                },
            );
            kept += 1;
        }
        kept
    }

    /// Directories collected so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.dirs.len()
    }

    /// Whether no directories have been collected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dirs.is_empty()
    }

    /// Writes the snapshot atomically and returns its id.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if the history database is damaged, or any
    /// SQLite error; nothing is written in either case.
    pub fn commit(self) -> Result<SnapshotId> {
        let Self {
            store,
            volume,
            totals,
            taken_at,
            min_dir_bytes,
            dirs,
        } = self;
        store.history().write(|tx| {
            let volume_id = ensure_volume(tx, &volume)?;
            let mut select = tx.prepare_cached(SQL_SELECT_PATH_ID)?;
            let mut insert = tx.prepare_cached(SQL_INSERT_PATH)?;
            let mut rows = Vec::with_capacity(dirs.len());
            // Path order makes new ids deterministic and gives siblings
            // adjacent ids, which keeps the blob's id deltas at one byte.
            let mut ordered: Vec<_> = dirs.iter().collect();
            ordered.sort_unstable_by(|a, b| a.1.path.cmp(&b.1.path));
            for (hash, d) in ordered {
                let existing: Option<i64> =
                    select.query_row([u2i(*hash)], |r| r.get(0)).optional()?;
                let id = match existing {
                    Some(id) => id,
                    None => {
                        insert.execute(params![u2i(*hash), d.parent_hash.map(u2i), d.path])?;
                        tx.last_insert_rowid()
                    }
                };
                rows.push((id, d.sizes));
            }
            rows.sort_unstable_by_key(|r| r.0);
            tx.prepare_cached(SQL_INSERT_SNAPSHOT)?.execute(params![
                volume_id,
                taken_at.0,
                u2i(totals.total_bytes),
                u2i(totals.free_bytes),
                u2i(totals.allocated_sum),
                u2i(totals.logical_sum),
                u2i(totals.file_count),
                u2i(totals.dir_count),
                totals.scanner.as_str(),
                u2i(min_dir_bytes),
                rows.len() as i64,
            ])?;
            let snapshot_id = tx.last_insert_rowid();
            tx.prepare_cached(SQL_INSERT_SNAPSHOT_DIRS)?
                .execute(params![snapshot_id, CODEC_V1, codec::encode(&rows)])?;
            Ok(SnapshotId(snapshot_id))
        })
    }
}

// -----------------------------------------------------------------------------
// Store API
// -----------------------------------------------------------------------------

impl Store {
    /// Starts a snapshot of `volume` stamped with the store clock's current
    /// time, using default [`SnapshotOptions`].
    ///
    /// # Example
    ///
    /// ```
    /// use strata_store::{DirAggregate, ScannerKind, Store, VolumeKey, VolumeTotals};
    /// let dir = tempfile::tempdir().unwrap();
    /// let store = Store::open(dir.path()).unwrap();
    /// let volume = VolumeKey { serial: 0xABCD, guid_path: r"\\?\Volume{1}\".into() };
    /// let totals = VolumeTotals {
    ///     total_bytes: 500 << 30, free_bytes: 200 << 30,
    ///     allocated_sum: 290 << 30, logical_sum: 280 << 30,
    ///     file_count: 1_000_000, dir_count: 150_000, scanner: ScannerKind::Mft,
    /// };
    /// let mut snap = store.begin_snapshot(&volume, totals);
    /// snap.add_dirs([DirAggregate {
    ///     path: r"C:\Users".into(), allocated: 100 << 30, logical: 98 << 30, files: 400_000,
    /// }]);
    /// let id = snap.commit().unwrap();
    /// assert_eq!(store.snapshot(id).unwrap().stored_dirs, 1);
    /// ```
    #[must_use]
    pub fn begin_snapshot(&self, volume: &VolumeKey, totals: VolumeTotals) -> SnapshotWriter {
        self.begin_snapshot_with(volume, totals, SnapshotOptions::default())
    }

    /// Like [`begin_snapshot`](Self::begin_snapshot) with explicit options.
    #[must_use]
    pub fn begin_snapshot_with(
        &self,
        volume: &VolumeKey,
        totals: VolumeTotals,
        options: SnapshotOptions,
    ) -> SnapshotWriter {
        SnapshotWriter {
            store: self.clone(),
            volume: volume.clone(),
            totals,
            taken_at: self.now(),
            min_dir_bytes: options.min_dir_bytes,
            dirs: HashMap::new(),
        }
    }

    /// Metadata of one snapshot.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] if it does not exist.
    pub fn snapshot(&self, id: SnapshotId) -> Result<SnapshotInfo> {
        self.history().read(|c| load_snapshot(c, id))
    }

    /// All snapshots of a volume, oldest first.
    ///
    /// # Errors
    ///
    /// Database errors only; an unknown volume yields an empty list.
    pub fn snapshots(&self, volume: &VolumeKey) -> Result<Vec<SnapshotInfo>> {
        self.history().read(|c| {
            let Some(vid) = find_volume(c, volume)? else {
                return Ok(Vec::new());
            };
            let mut list = latest_snapshots(c, vid, usize::MAX)?;
            list.reverse();
            Ok(list)
        })
    }

    /// The newest snapshot of a volume, if any. Use its `taken_at` to decide
    /// when the periodic live snapshot is due.
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn latest_snapshot(&self, volume: &VolumeKey) -> Result<Option<SnapshotInfo>> {
        self.history().read(|c| {
            let Some(vid) = find_volume(c, volume)? else {
                return Ok(None);
            };
            Ok(latest_snapshots(c, vid, 1)?.into_iter().next())
        })
    }

    /// Every volume with stored history.
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn volumes(&self) -> Result<Vec<VolumeSummary>> {
        self.history().read(|c| {
            let mut stmt = c.prepare_cached(SQL_VOLUME_SUMMARIES)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(VolumeSummary {
                        key: VolumeKey {
                            serial: i2u(r.get(0)?),
                            guid_path: r.get(1)?,
                        },
                        snapshot_count: i2u(r.get(2)?),
                        first_at: r.get::<_, Option<i64>>(3)?.map(Timestamp),
                        last_at: r.get::<_, Option<i64>>(4)?.map(Timestamp),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Volume usage over time for a line chart, oldest first, limited to
    /// snapshots taken in `[from, to]` (inclusive; `None` means unbounded).
    ///
    /// # Example
    ///
    /// ```
    /// # use strata_store::{Store, VolumeKey};
    /// # let dir = tempfile::tempdir().unwrap();
    /// # let store = Store::open(dir.path()).unwrap();
    /// let volume = VolumeKey { serial: 1, guid_path: "v".into() };
    /// let series = store.usage_series(&volume, None, None).unwrap();
    /// assert!(series.is_empty());
    /// ```
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn usage_series(
        &self,
        volume: &VolumeKey,
        from: Option<Timestamp>,
        to: Option<Timestamp>,
    ) -> Result<Vec<UsagePoint>> {
        self.history().read(|c| {
            let Some(vid) = find_volume(c, volume)? else {
                return Ok(Vec::new());
            };
            let mut stmt = c.prepare_cached(SQL_USAGE_SERIES)?;
            let rows = stmt
                .query_map(
                    params![
                        vid,
                        from.map_or(i64::MIN, |t| t.0),
                        to.map_or(i64::MAX, |t| t.0)
                    ],
                    |r| {
                        let total = i2u(r.get(2)?);
                        let free = i2u(r.get(3)?);
                        Ok(UsagePoint {
                            snapshot: SnapshotId(r.get(0)?),
                            at: Timestamp(r.get(1)?),
                            total_bytes: total,
                            used_bytes: total.saturating_sub(free),
                            free_bytes: free,
                            allocated_sum: i2u(r.get(4)?),
                            logical_sum: i2u(r.get(5)?),
                        })
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Sizes of one directory over the volume's last `last_n` snapshots,
    /// oldest first, for the detail panel's sparkline.
    ///
    /// # Example
    ///
    /// ```
    /// # use strata_store::{Store, VolumeKey, path_hash};
    /// # let dir = tempfile::tempdir().unwrap();
    /// # let store = Store::open(dir.path()).unwrap();
    /// # let volume = VolumeKey { serial: 1, guid_path: "v".into() };
    /// let points = store.dir_series(&volume, path_hash(r"C:\Users"), 30).unwrap();
    /// for p in points {
    ///     println!("{}: {:?}", p.at, p.sizes.map(|s| s.allocated));
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Database errors, or [`StoreError::Corrupt`] for a damaged blob.
    pub fn dir_series(
        &self,
        volume: &VolumeKey,
        path_hash: u64,
        last_n: usize,
    ) -> Result<Vec<DirPoint>> {
        self.history().read(|c| {
            let Some(vid) = find_volume(c, volume)? else {
                return Ok(Vec::new());
            };
            let path_id: Option<i64> = c
                .prepare_cached(SQL_SELECT_PATH_ID)?
                .query_row([u2i(path_hash)], |r| r.get(0))
                .optional()?;
            let mut stmt = c.prepare_cached(SQL_LAST_N_SNAPSHOTS)?;
            let mut rows = stmt.query(params![vid, i64::try_from(last_n).unwrap_or(i64::MAX)])?;
            let mut points = Vec::new();
            while let Some(row) = rows.next()? {
                let id: i64 = row.get(0)?;
                check_codec(id, row.get(2)?)?;
                let sizes = match path_id {
                    Some(pid) => codec::find(
                        row.get_ref(3)?.as_blob().map_err(rusqlite::Error::from)?,
                        pid,
                    )
                    .map_err(|e| blob_error(id, e.0))?,
                    None => None,
                };
                points.push(DirPoint {
                    snapshot: SnapshotId(id),
                    at: Timestamp(row.get(1)?),
                    sizes,
                });
            }
            points.reverse();
            Ok(points)
        })
    }

    /// Sizes of one directory in one snapshot.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] for an unknown snapshot.
    pub fn snapshot_dir(&self, id: SnapshotId, path: &str) -> Result<Option<DirSizes>> {
        let hash = path_hash(path);
        self.history().read(|c| {
            let path_id: Option<i64> = c
                .prepare_cached(SQL_SELECT_PATH_ID)?
                .query_row([u2i(hash)], |r| r.get(0))
                .optional()?;
            let (codec_id, data): (i64, Vec<u8>) = c
                .prepare_cached(SQL_SNAPSHOT_DIRS)?
                .query_row([id.0], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?
                .ok_or_else(|| StoreError::NotFound(format!("snapshot {}", id.0)))?;
            check_codec(id.0, codec_id)?;
            match path_id {
                Some(pid) => codec::find(&data, pid).map_err(|e| blob_error(id.0, e.0)),
                None => Ok(None),
            }
        })
    }

    /// Deletes every snapshot and stored path ("Clear history" in settings).
    /// Activity data and the hash cache are separate; see
    /// [`clear_activity`](Self::clear_activity) and
    /// [`clear_hash_cache`](Self::clear_hash_cache).
    ///
    /// # Errors
    ///
    /// Database errors only.
    pub fn clear_history(&self) -> Result<()> {
        self.history().write(|tx| {
            tx.execute_batch(SQL_CLEAR_SNAPSHOTS)?;
            Ok(())
        })
    }
}
