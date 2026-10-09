//! History, snapshots and diffs for the UI (SPEC Â§18).
//!
//! Volumes are named by the UI's `volumeId` (the volume GUID path from
//! `list_volumes`). The store keys snapshots by `(serial, GUID path)`, so a
//! volume id resolves to the most recently used key with that GUID path;
//! the serial only differs after a reformat, and then the newest history is
//! the one that matters. Times are Unix milliseconds; 64-bit path hashes
//! are strings (they exceed JavaScript's exact integer range).

use serde::{Deserialize, Serialize};
use strata_core::SizeMode;
use strata_store::{
    DiffOptions, DirChange, DirPoint, DirSizes, ScannerKind, SinceLastScan, SnapshotDiff,
    SnapshotId, SnapshotInfo, Store, Timestamp, UsagePoint, VolumeKey, VolumeSummary, path_hash,
};
use tauri::{AppHandle, Runtime};

use super::error::{FeatureResult, blocking};

fn ms(t: Timestamp) -> i64 {
    t.0.saturating_mul(1000)
}

fn ts_from_ms(v: i64) -> Timestamp {
    Timestamp(v.div_euclid(1000))
}

/// Whether two volume ids name the same volume (case and trailing
/// separator do not matter).
#[must_use]
pub fn same_volume_id(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim_end_matches('\\').to_ascii_uppercase();
    norm(a) == norm(b)
}

/// The store key for a UI volume id, if that volume has history.
#[must_use]
pub fn resolve_volume(volumes: &[VolumeSummary], volume_id: &str) -> Option<VolumeKey> {
    volumes
        .iter()
        .filter(|v| same_volume_id(&v.key.guid_path, volume_id))
        .max_by_key(|v| v.last_at)
        .map(|v| v.key.clone())
}

fn key(store: &Store, volume_id: &str) -> FeatureResult<Option<VolumeKey>> {
    Ok(resolve_volume(&store.volumes()?, volume_id))
}

/// One volume with history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryVolume {
    /// Volume id (GUID path).
    pub volume_id: String,
    /// Volume serial, as a string.
    pub serial: String,
    /// Snapshots stored.
    pub snapshot_count: u64,
    /// Oldest snapshot, Unix ms.
    pub first_ms: Option<i64>,
    /// Newest snapshot, Unix ms.
    pub last_ms: Option<i64>,
}

/// A usage point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsagePointDto {
    /// Snapshot id.
    pub snapshot_id: i64,
    /// Unix ms.
    pub at_ms: i64,
    /// Capacity.
    pub total_bytes: u64,
    /// Used.
    pub used_bytes: u64,
    /// Free.
    pub free_bytes: u64,
    /// Î£ allocated of scanned files.
    pub allocated_sum: u64,
    /// Î£ logical of scanned files.
    pub logical_sum: u64,
}

impl From<&UsagePoint> for UsagePointDto {
    fn from(p: &UsagePoint) -> Self {
        Self {
            snapshot_id: p.snapshot.0,
            at_ms: ms(p.at),
            total_bytes: p.total_bytes,
            used_bytes: p.used_bytes,
            free_bytes: p.free_bytes,
            allocated_sum: p.allocated_sum,
            logical_sum: p.logical_sum,
        }
    }
}

/// Snapshot metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotDto {
    /// Snapshot id.
    pub id: i64,
    /// Unix ms.
    pub at_ms: i64,
    /// Capacity.
    pub total_bytes: u64,
    /// Free.
    pub free_bytes: u64,
    /// Î£ allocated.
    pub allocated_sum: u64,
    /// Î£ logical.
    pub logical_sum: u64,
    /// Files.
    pub file_count: u64,
    /// Directories.
    pub dir_count: u64,
    /// `mft` or `walker`.
    pub scanner: ScannerKind,
    /// Directory rows stored.
    pub stored_dirs: u64,
}

impl From<&SnapshotInfo> for SnapshotDto {
    fn from(s: &SnapshotInfo) -> Self {
        Self {
            id: s.id.0,
            at_ms: ms(s.taken_at),
            total_bytes: s.totals.total_bytes,
            free_bytes: s.totals.free_bytes,
            allocated_sum: s.totals.allocated_sum,
            logical_sum: s.totals.logical_sum,
            file_count: s.totals.file_count,
            dir_count: s.totals.dir_count,
            scanner: s.totals.scanner,
            stored_dirs: s.stored_dirs,
        }
    }
}

/// One directory in a diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirChangeDto {
    /// `path_hash` as a decimal string.
    pub path_hash: String,
    /// Display path.
    pub path: String,
    /// Sizes before (`allocated`, `logical`, `files`).
    pub before: Option<DirSizes>,
    /// Sizes after.
    pub after: Option<DirSizes>,
    /// Change in the requested size mode.
    pub delta_bytes: i64,
}

impl From<&DirChange> for DirChangeDto {
    fn from(c: &DirChange) -> Self {
        Self {
            path_hash: c.path_hash.to_string(),
            path: c.path.clone(),
            before: c.before,
            after: c.after,
            delta_bytes: c.delta,
        }
    }
}

/// "What changed" between two snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffDto {
    /// Older snapshot.
    pub from: SnapshotDto,
    /// Newer snapshot.
    pub to: SnapshotDto,
    /// Change in used bytes.
    pub used_delta: i64,
    /// Change in the scanned sum.
    pub scanned_delta: i64,
    /// Largest growth first.
    pub grown: Vec<DirChangeDto>,
    /// Largest shrinkage first.
    pub shrunk: Vec<DirChangeDto>,
    /// New large folders.
    pub new_large: Vec<DirChangeDto>,
    /// Deleted large folders.
    pub deleted_large: Vec<DirChangeDto>,
}

impl From<&SnapshotDiff> for DiffDto {
    fn from(d: &SnapshotDiff) -> Self {
        let list = |v: &[DirChange]| v.iter().map(Into::into).collect();
        Self {
            from: (&d.from).into(),
            to: (&d.to).into(),
            used_delta: d.used_delta,
            scanned_delta: d.scanned_delta,
            grown: list(&d.grown),
            shrunk: list(&d.shrunk),
            new_large: list(&d.new_large),
            deleted_large: list(&d.deleted_large),
        }
    }
}

/// Diff tuning from the UI; omitted fields use the defaults.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffRequest {
    /// `allocated` or `logical`; default from settings.
    pub size_mode: Option<SizeMode>,
    /// Rows per list (max 200).
    pub top_n: Option<usize>,
    /// Smallest change in bytes.
    pub min_change: Option<u64>,
    /// Smallest size for new/deleted lists.
    pub large_threshold: Option<u64>,
}

impl DiffRequest {
    /// Store options, falling back to `default_mode`.
    #[must_use]
    pub fn options(&self, default_mode: SizeMode) -> DiffOptions {
        let d = DiffOptions::default();
        DiffOptions {
            size_mode: self.size_mode.unwrap_or(default_mode),
            top_n: self.top_n.unwrap_or(d.top_n).clamp(1, 200),
            min_change: self.min_change.unwrap_or(d.min_change),
            large_threshold: self.large_threshold.unwrap_or(d.large_threshold),
            ..d
        }
    }
}

/// A sparkline point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirPointDto {
    /// Snapshot id.
    pub snapshot_id: i64,
    /// Unix ms.
    pub at_ms: i64,
    /// Sizes, or `null` when absent or below that snapshot's threshold.
    pub sizes: Option<DirSizes>,
}

impl From<&DirPoint> for DirPointDto {
    fn from(p: &DirPoint) -> Self {
        Self {
            snapshot_id: p.snapshot.0,
            at_ms: ms(p.at),
            sizes: p.sizes,
        }
    }
}

/// The home-screen banner, in the UI's `SinceLastScan` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SinceLastScanDto {
    /// Change in used bytes since the previous snapshot.
    pub delta_bytes: i64,
    /// When the previous snapshot was taken, Unix ms.
    pub since_ms: i64,
    /// The directory that explains most of it.
    pub biggest: Option<BiggestDto>,
}

/// Biggest contributor in [`SinceLastScanDto`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BiggestDto {
    /// Display path.
    pub path: String,
    /// Its change.
    pub delta_bytes: i64,
}

impl From<&SinceLastScan> for SinceLastScanDto {
    fn from(s: &SinceLastScan) -> Self {
        Self {
            delta_bytes: s.used_delta,
            since_ms: ms(s.from.taken_at),
            biggest: s.biggest.as_ref().map(|b| BiggestDto {
                path: b.path.clone(),
                delta_bytes: b.delta,
            }),
        }
    }
}

fn default_mode(store: &Store) -> SizeMode {
    store
        .load_settings()
        .map(|s| s.scan.default_size_mode)
        .unwrap_or_default()
}

/// Volumes that have history.
#[tauri::command]
pub async fn history_volumes<R: Runtime>(app: AppHandle<R>) -> FeatureResult<Vec<HistoryVolume>> {
    let store = super::store::handle(&app)?;
    blocking(move || {
        Ok(store
            .volumes()?
            .iter()
            .map(|v| HistoryVolume {
                volume_id: v.key.guid_path.clone(),
                serial: v.key.serial.to_string(),
                snapshot_count: v.snapshot_count,
                first_ms: v.first_at.map(ms),
                last_ms: v.last_at.map(ms),
            })
            .collect())
    })
    .await
}

/// Volume usage over time (line chart). `fromMs`/`toMs` bound the range.
#[tauri::command]
pub async fn history_series<R: Runtime>(
    app: AppHandle<R>,
    volume_id: String,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
) -> FeatureResult<Vec<UsagePointDto>> {
    let store = super::store::handle(&app)?;
    blocking(move || {
        let Some(k) = key(&store, &volume_id)? else {
            return Ok(Vec::new());
        };
        Ok(store
            .usage_series(&k, from_ms.map(ts_from_ms), to_ms.map(ts_from_ms))?
            .iter()
            .map(Into::into)
            .collect())
    })
    .await
}

/// Snapshots of a volume, oldest first.
#[tauri::command]
pub async fn history_snapshots<R: Runtime>(
    app: AppHandle<R>,
    volume_id: String,
) -> FeatureResult<Vec<SnapshotDto>> {
    let store = super::store::handle(&app)?;
    blocking(move || {
        let Some(k) = key(&store, &volume_id)? else {
            return Ok(Vec::new());
        };
        Ok(store.snapshots(&k)?.iter().map(Into::into).collect())
    })
    .await
}

/// "What changed" between two snapshots of one volume.
#[tauri::command]
pub async fn history_diff<R: Runtime>(
    app: AppHandle<R>,
    from: i64,
    to: i64,
    options: Option<DiffRequest>,
) -> FeatureResult<DiffDto> {
    let store = super::store::handle(&app)?;
    blocking(move || {
        let opts = options.unwrap_or_default().options(default_mode(&store));
        Ok((&store.diff(SnapshotId(from), SnapshotId(to), &opts)?).into())
    })
    .await
}

/// Sizes of one directory over the last `lastN` snapshots (sparkline).
#[tauri::command]
pub async fn history_dir_series<R: Runtime>(
    app: AppHandle<R>,
    volume_id: String,
    path: String,
    last_n: Option<usize>,
) -> FeatureResult<Vec<DirPointDto>> {
    let store = super::store::handle(&app)?;
    blocking(move || {
        let Some(k) = key(&store, &volume_id)? else {
            return Ok(Vec::new());
        };
        Ok(store
            .dir_series(&k, path_hash(&path), last_n.unwrap_or(30).clamp(1, 365))?
            .iter()
            .map(Into::into)
            .collect())
    })
    .await
}

/// The home-screen "since last scan" banner, or `null` with fewer than two
/// snapshots.
#[tauri::command]
pub async fn history_since_last_scan<R: Runtime>(
    app: AppHandle<R>,
    volume_id: String,
) -> FeatureResult<Option<SinceLastScanDto>> {
    let store = super::store::handle(&app)?;
    blocking(move || {
        let Some(k) = key(&store, &volume_id)? else {
            return Ok(None);
        };
        let opts = DiffOptions {
            size_mode: default_mode(&store),
            ..DiffOptions::default()
        };
        Ok(store.since_last_scan(&k, &opts)?.as_ref().map(Into::into))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_store::{DirAggregate, VolumeTotals};

    fn totals(free: u64) -> VolumeTotals {
        VolumeTotals {
            total_bytes: 1 << 40,
            free_bytes: free,
            allocated_sum: 0,
            logical_sum: 0,
            file_count: 1,
            dir_count: 1,
            scanner: ScannerKind::Walker,
        }
    }

    #[test]
    fn volume_ids_resolve_to_store_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let k = VolumeKey {
            serial: 7,
            guid_path: r"\\?\Volume{00000000-0000-0000-0000-000000000001}\".into(),
        };
        let gib = 1u64 << 30;
        for (free, size) in [(500 * gib, gib), (490 * gib, 11 * gib)] {
            let mut w = store.begin_snapshot(&k, totals(free));
            w.add_dirs([DirAggregate {
                path: r"C:\Models".into(),
                allocated: size,
                logical: size,
                files: 1,
            }]);
            w.commit().unwrap();
        }
        let vols = store.volumes().unwrap();
        let id = r"\\?\volume{00000000-0000-0000-0000-000000000001}";
        assert_eq!(resolve_volume(&vols, id), Some(k.clone()));
        assert_eq!(resolve_volume(&vols, r"\\?\Volume{other}\"), None);

        let since = store
            .since_last_scan(&k, &DiffOptions::default())
            .unwrap()
            .unwrap();
        let dto = SinceLastScanDto::from(&since);
        assert_eq!(dto.delta_bytes, i64::try_from(10 * gib).unwrap());
        assert_eq!(dto.biggest.unwrap().path, r"C:\Models");
        let v = serde_json::to_value(SinceLastScanDto::from(&since)).unwrap();
        assert!(v.get("sinceMs").is_some() && v.get("deltaBytes").is_some());
    }

    #[test]
    fn diff_request_defaults_and_clamps() {
        let o = DiffRequest {
            top_n: Some(10_000),
            ..DiffRequest::default()
        }
        .options(SizeMode::Logical);
        assert_eq!(o.top_n, 200);
        assert_eq!(o.size_mode, SizeMode::Logical);
        assert_eq!(o.min_change, DiffOptions::default().min_change);
        assert_eq!(ts_from_ms(1_999), Timestamp(1));
        assert_eq!(ts_from_ms(-1), Timestamp(-1));
    }

    #[test]
    fn path_hashes_are_strings() {
        let c = DirChange {
            path_hash: u64::MAX,
            path: "x".into(),
            before: None,
            after: None,
            delta: 1,
        };
        let v = serde_json::to_value(DirChangeDto::from(&c)).unwrap();
        assert_eq!(v["pathHash"], u64::MAX.to_string());
    }
}
