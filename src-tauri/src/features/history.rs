//! History, snapshots and diffs for the UI (`ui/src/lib/history.ts`).
//!
//! Volumes are named by the UI's `volumeId` (the volume GUID path from
//! `list_volumes`). The store keys snapshots by `(serial, GUID path)`, so a
//! volume id resolves to the most recently used key with that GUID path;
//! the serial only differs after a reformat, and then the newest history is
//! the one that matters. Times are Unix milliseconds.

use std::sync::Arc;

use serde::Serialize;
use strata_core::SizeMode;
use strata_index::EntryId;
use strata_store::{
    DiffOptions, DirChange, DirSizes, ScannerKind, SinceLastScan, SnapshotDiff, SnapshotId,
    SnapshotInfo, Store, Timestamp, VolumeKey, VolumeSummary, path_hash,
};
use tauri::{AppHandle, Manager, Runtime};

use super::error::{FeatureError, FeatureResult, blocking};
use crate::model::VolumeData;
use crate::state::{AppState, read};

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

fn app_state<R: Runtime>(app: &AppHandle<R>) -> Option<Arc<AppState>> {
    app.try_state::<Arc<AppState>>().map(|s| s.inner().clone())
}

/// Runs `f` with the published index of the volume whose GUID path is
/// `guid_path`, if one is indexed.
fn with_index<T>(state: &AppState, guid_path: &str, f: impl FnOnce(&VolumeData) -> T) -> Option<T> {
    let ids: Vec<String> = crate::state::lock(&state.sessions)
        .keys()
        .cloned()
        .collect();
    let id = ids.into_iter().find(|i| same_volume_id(i, guid_path))?;
    let s = state.existing_session(&id)?;
    let g = read(&s.data);
    g.as_ref().map(f)
}

/// The entry at `path` in `data`, matching names case-insensitively.
#[must_use]
pub fn find_path(data: &VolumeData, path: &str) -> Option<EntryId> {
    let ix = &data.index;
    let root = data.root_path.trim_end_matches('\\');
    let rest = path
        .get(..root.len())
        .filter(|p| p.eq_ignore_ascii_case(root))
        .map(|_| &path[root.len()..])?;
    let mut cur = ix.root();
    for part in rest.split('\\').filter(|p| !p.is_empty()) {
        cur = ix
            .children(cur)
            .find(|&k| ix.name_lossy(k).eq_ignore_ascii_case(part))?;
    }
    Some(cur)
}

/// One stored snapshot (`SnapshotInfo` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotDto {
    /// Snapshot id.
    pub id: i64,
    /// Unix ms.
    pub taken_ms: i64,
    /// Capacity.
    pub total_bytes: u64,
    /// Used.
    pub used_bytes: u64,
    /// Free.
    pub free_bytes: u64,
    /// Sum of allocated sizes scanned.
    pub allocated_sum: u64,
    /// Sum of logical sizes scanned.
    pub logical_sum: u64,
    /// Files.
    pub files: u64,
    /// Directories.
    pub dirs: u64,
    /// `mft` or `walker`.
    pub scanner: ScannerKind,
}

impl From<&SnapshotInfo> for SnapshotDto {
    fn from(s: &SnapshotInfo) -> Self {
        let t = &s.totals;
        Self {
            id: s.id.0,
            taken_ms: ms(s.taken_at),
            total_bytes: t.total_bytes,
            used_bytes: t.total_bytes.saturating_sub(t.free_bytes),
            free_bytes: t.free_bytes,
            allocated_sum: t.allocated_sum,
            logical_sum: t.logical_sum,
            files: t.file_count,
            dirs: t.dir_count,
            scanner: t.scanner,
        }
    }
}

/// A usage point (`UsagePoint` in the UI).
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
}

/// One directory in a diff (`DirChange` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirChangeDto {
    /// Display path.
    pub path: String,
    /// Sizes before.
    pub before: Option<DirSizes>,
    /// Sizes after.
    pub after: Option<DirSizes>,
    /// Change in the requested size mode.
    pub delta: i64,
    /// Wire id in the current index, when the folder still exists.
    pub entry_id: Option<u32>,
}

/// "What changed" between two snapshots (`SnapshotDiff` in the UI).
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

fn diff_dto(d: &SnapshotDiff, data: Option<&VolumeData>) -> DiffDto {
    let list = |v: &[DirChange]| {
        v.iter()
            .map(|c| DirChangeDto {
                path: c.path.clone(),
                before: c.before,
                after: c.after,
                delta: c.delta,
                entry_id: data.and_then(|d| find_path(d, &c.path).map(|e| d.wire(e))),
            })
            .collect()
    };
    DiffDto {
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

/// A directory's size at one snapshot (`DirPoint` in the UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DirPointDto {
    /// Unix ms.
    #[serde(rename = "atMs")]
    pub at_ms: i64,
    /// Allocated bytes, `null` when absent or below the snapshot's minimum.
    pub allocated: Option<u64>,
    /// Logical bytes.
    pub logical: Option<u64>,
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

/// Snapshots of a volume, oldest first (`history_snapshots`).
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
        let mut v: Vec<SnapshotDto> = store.snapshots(&k)?.iter().map(Into::into).collect();
        v.sort_by_key(|s| s.taken_ms);
        Ok(v)
    })
    .await
}

/// Volume usage over time (`history_usage`); `null` bounds are open.
#[tauri::command]
pub async fn history_usage<R: Runtime>(
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
            .map(|p| UsagePointDto {
                snapshot_id: p.snapshot.0,
                at_ms: ms(p.at),
                total_bytes: p.total_bytes,
                used_bytes: p.used_bytes,
                free_bytes: p.free_bytes,
            })
            .collect())
    })
    .await
}

/// "What changed" between two snapshots of one volume (`history_diff`).
#[tauri::command]
pub async fn history_diff<R: Runtime>(
    app: AppHandle<R>,
    from_id: i64,
    to_id: i64,
    size_mode: Option<SizeMode>,
    top_n: Option<usize>,
) -> FeatureResult<DiffDto> {
    let store = super::store::handle(&app)?;
    let state = app_state(&app);
    blocking(move || {
        let d = DiffOptions::default();
        let opts = DiffOptions {
            size_mode: size_mode.unwrap_or_else(|| default_mode(&store)),
            top_n: top_n.unwrap_or(d.top_n).clamp(1, 200),
            ..d
        };
        let diff = store.diff(SnapshotId(from_id), SnapshotId(to_id), &opts)?;
        let dto = state
            .as_deref()
            .and_then(|st| {
                with_index(st, &diff.to.volume.guid_path, |data| {
                    diff_dto(&diff, Some(data))
                })
            })
            .unwrap_or_else(|| diff_dto(&diff, None));
        Ok(dto)
    })
    .await
}

/// Sizes of one folder over the last `lastN` snapshots, oldest first
/// (`history_dir_series`).
#[tauri::command]
pub async fn history_dir_series<R: Runtime>(
    app: AppHandle<R>,
    volume_id: String,
    id: u32,
    last_n: Option<usize>,
) -> FeatureResult<Vec<DirPointDto>> {
    let store = super::store::handle(&app)?;
    let state = app_state(&app).ok_or_else(|| FeatureError::internal("app state missing"))?;
    blocking(move || {
        let path = crate::commands::with_data(&state, &volume_id, |d| {
            d.resolve(id)
                .map(|e| d.path(e))
                .ok_or_else(|| FeatureError::not_found("that folder is no longer in the index"))
        })?;
        let Some(k) = key(&store, &volume_id)? else {
            return Ok(Vec::new());
        };
        let mut points: Vec<DirPointDto> = store
            .dir_series(&k, path_hash(&path), last_n.unwrap_or(30).clamp(1, 365))?
            .iter()
            .map(|p| DirPointDto {
                at_ms: ms(p.at),
                allocated: p.sizes.map(|s| s.allocated),
                logical: p.sizes.map(|s| s.logical),
            })
            .collect();
        points.sort_by_key(|p| p.at_ms);
        Ok(points)
    })
    .await
}

/// The home-screen "since last scan" banner, or `null` with fewer than two
/// snapshots (`history_since_last_scan`).
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

        let snaps = store.snapshots(&k).unwrap();
        let s = SnapshotDto::from(&snaps[0]);
        assert_eq!(s.used_bytes, s.total_bytes - s.free_bytes);
        let v = serde_json::to_value(&s).unwrap();
        assert!(v.get("takenMs").is_some() && v.get("files").is_some());
        let diff = store
            .diff(snaps[0].id, snaps[1].id, &DiffOptions::default())
            .unwrap();
        let d = diff_dto(&diff, None);
        assert_eq!(d.grown[0].path, r"C:\Models");
        assert_eq!(d.grown[0].entry_id, None);
    }

    #[test]
    fn ms_conversion_floors() {
        assert_eq!(ts_from_ms(1_999), Timestamp(1));
        assert_eq!(ts_from_ms(-1), Timestamp(-1));
    }
}
