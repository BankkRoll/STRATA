//! The duplicate finder over the indexes (`ui/src/lib/dupes.ts`).
//!
//! - Candidates come from the published indexes (files of at least the
//!   minimum size with a real file reference); `strata-dupes` measures,
//!   hashes and groups them on a background thread, with progress on
//!   `dupes://status` and cancel. Hashes are cached in the store
//!   ([`StoreHashCache`]), so a cancelled or repeated run resumes from them.
//! - Live updates drop groups whose files changed or vanished and
//!   invalidate their cached hashes ([`on_live_changes`]).
//! - Queueing and hardlinking re-check the guardrail (never every copy of a
//!   group) with `strata_dupes::Selection` before anything reaches the
//!   cleanup queue or a consent prompt.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use strata_clean::CancelToken;
use strata_clean::consent::Prompt;
use strata_core::{EntryFlags, FileRef, FileTime, Safety};
use strata_dupes::hardlink::ReplaceWithHardlinks;
use strata_dupes::{
    CacheError, CachedHash, Candidate, DuplicateReport, HashCache, HashKey, KeepContext,
    KeepReason, Phase, Progress, ScanConfig, ScanOutcome, Selection, SelectionRequest, VolumeKey,
    find_duplicates,
};
use tauri::{AppHandle, Emitter, Runtime};

use crate::state::{AppState, lock, read};

/// Volume serial → volume id and store key of the volumes in a report.
pub type VolumeMap = HashMap<u64, (String, VolumeKey)>;

/// A reported file in the current index: wire id, shown tier, modified.
type Found = (u32, Option<Safety>, u32);

/// Event carrying the [`DupeScanStatus`].
pub const DUPES_STATUS: &str = "dupes://status";

// -----------------------------------------------------------------------------
// Store cache
// -----------------------------------------------------------------------------

/// [`HashCache`] over the store's hash table.
#[derive(Debug, Clone)]
pub struct StoreHashCache(pub strata_store::Store);

fn store_key(v: &VolumeKey) -> strata_store::VolumeKey {
    strata_store::VolumeKey {
        serial: v.serial,
        guid_path: v.guid_path.to_string(),
    }
}

impl HashCache for StoreHashCache {
    fn lookup(
        &self,
        volume: &VolumeKey,
        keys: &[HashKey],
    ) -> Result<Vec<Option<CachedHash>>, CacheError> {
        let keys: Vec<strata_store::HashKey> = keys
            .iter()
            .map(|k| strata_store::HashKey {
                file_ref: k.file_ref,
                size: k.size,
                mtime: k.mtime,
            })
            .collect();
        self.0
            .lookup_hashes(&store_key(volume), &keys)
            .map(|v| {
                v.into_iter()
                    .map(|o| {
                        o.map(|c| CachedHash {
                            key: HashKey {
                                file_ref: c.key.file_ref,
                                size: c.key.size,
                                mtime: c.key.mtime,
                            },
                            partial: c.partial,
                            full: c.full,
                        })
                    })
                    .collect()
            })
            .map_err(|e| CacheError(e.to_string()))
    }

    fn upsert(&self, volume: &VolumeKey, entries: &[CachedHash]) -> Result<(), CacheError> {
        let rows: Vec<strata_store::CachedHash> = entries
            .iter()
            .map(|c| strata_store::CachedHash {
                key: strata_store::HashKey {
                    file_ref: c.key.file_ref,
                    size: c.key.size,
                    mtime: c.key.mtime,
                },
                partial: c.partial,
                full: c.full,
            })
            .collect();
        self.0
            .upsert_hashes(&store_key(volume), &rows)
            .map_err(|e| CacheError(e.to_string()))
    }

    fn invalidate(&self, volume: &VolumeKey, refs: &[FileRef]) -> Result<u64, CacheError> {
        self.0
            .invalidate_hashes(&store_key(volume), refs)
            .map_err(|e| CacheError(e.to_string()))
    }
}

// -----------------------------------------------------------------------------
// State
// -----------------------------------------------------------------------------

/// Scan progress (`DupeScanStatus.progress` in the UI).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DupeProgress {
    /// Files done in this phase.
    pub files_done: u64,
    /// Files in this phase.
    pub files_total: u64,
    /// Bytes done.
    pub bytes_done: u64,
    /// Bytes in this phase.
    pub bytes_total: u64,
    /// Seconds left, when known.
    pub eta_secs: Option<f64>,
}

/// The finder's state (`DupeScanStatus` in the UI).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DupeScanStatus {
    /// `idle`, `running`, `done`, `cancelled` or `error`.
    pub state: &'static str,
    /// `grouping`, `partial_hash`, `full_hash` while running.
    pub phase: Option<&'static str>,
    /// Progress while running.
    pub progress: Option<DupeProgress>,
    /// Unix ms of the last completed run.
    pub last_run_ms: Option<i64>,
    /// Groups found.
    pub groups: u64,
    /// Bytes wasted by duplicates.
    pub wasted_bytes: u64,
    /// What went wrong, for `error`.
    pub message: Option<String>,
}

impl Default for DupeScanStatus {
    fn default() -> Self {
        Self {
            state: "idle",
            phase: None,
            progress: None,
            last_run_ms: None,
            groups: 0,
            wasted_bytes: 0,
            message: None,
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    status: DupeScanStatus,
    report: Option<DuplicateReport>,
    /// Volume serial → volume id and store key.
    volumes: VolumeMap,
    cancel: Option<CancelToken>,
    downloads: Vec<String>,
}

/// The finder's state in [`AppState`].
#[derive(Debug, Default)]
pub struct DupeState {
    inner: Mutex<Inner>,
    prompts: crate::features::consent::PendingConsents<Vec<Prompt<ReplaceWithHardlinks>>>,
}

impl DupeState {
    /// The current status.
    #[must_use]
    pub fn status(&self) -> DupeScanStatus {
        lock(&self.inner).status.clone()
    }
}

fn phase_name(p: Phase) -> Option<&'static str> {
    match p {
        Phase::Grouping | Phase::Measuring => Some("grouping"),
        Phase::PartialHash => Some("partial_hash"),
        Phase::FullHash => Some("full_hash"),
        Phase::Finished => None,
    }
}

fn emit<R: Runtime>(app: &AppHandle<R>, state: &AppState) {
    let _ = app.emit(DUPES_STATUS, state.dupes.status());
}

fn path_key(p: &str) -> String {
    p.trim_end_matches('\\').to_lowercase()
}

fn downloads() -> Vec<PathBuf> {
    strata_win::known::known_folders()
        .map(|kf| {
            kf.users
                .iter()
                .filter_map(|u| u.folders.get(&strata_core::known::KnownFolder::Downloads))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn temp_dirs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = strata_win::known::temp_dir().into_iter().collect();
    if let Ok(w) = std::env::var("SystemRoot") {
        v.push(Path::new(&w).join("Temp"));
    }
    v
}

// -----------------------------------------------------------------------------
// Running
// -----------------------------------------------------------------------------

/// Index candidates of one volume.
fn volume_candidates(
    state: &AppState,
    volume_id: &str,
    min_size: u64,
) -> Option<(VolumeKey, Vec<Candidate>)> {
    let (serial, guid) = {
        let reg = lock(&state.registry);
        let e = reg.volumes.get(volume_id)?;
        (
            u64::from(e.info.serial.unwrap_or(0)),
            e.info.guid_path.clone()?,
        )
    };
    let key = VolumeKey::new(serial, guid);
    let session = state.existing_session(volume_id)?;
    let g = read(&session.data);
    let data = g.as_ref().filter(|d| !d.preview)?;
    let ix = &data.index;
    let mut out = Vec::new();
    ix.for_each_in_subtree(ix.root(), |id| {
        if ix.is_dir(id) || ix.own_logical(id) < min_size {
            return;
        }
        let flags = ix.flags(id);
        if flags.contains(EntryFlags::VIRTUAL) {
            return;
        }
        let Some(file_ref) = ix.file_ref(id) else {
            return;
        };
        out.push(Candidate {
            volume: key.clone(),
            file_ref,
            path: PathBuf::from(data.path(id)),
            size: ix.own_logical(id),
            mtime: ix
                .times(id)
                .map_or(FileTime(0), |t| t.modified.to_filetime()),
            flags,
        });
    });
    Some((key, out))
}

/// Starts a scan of `volume_ids` on a background thread.
///
/// # Errors
///
/// A scan is already running, or no listed volume is indexed.
pub fn start<R: Runtime>(
    app: &AppHandle<R>,
    state: &Arc<AppState>,
    volume_ids: &[String],
    min_size: u64,
) -> Result<(), String> {
    let cancel = CancelToken::new();
    {
        let mut g = lock(&state.dupes.inner);
        if g.cancel.is_some() {
            return Err("a duplicate scan is already running".into());
        }
        g.cancel = Some(cancel.clone());
        g.status = DupeScanStatus {
            state: "running",
            phase: Some("grouping"),
            last_run_ms: g.status.last_run_ms,
            ..DupeScanStatus::default()
        };
    }
    emit(app, state);
    let app = app.clone();
    let st = state.clone();
    let ids = volume_ids.to_vec();
    let spawned = std::thread::Builder::new()
        .name("strata-dupes".into())
        .spawn(move || run(&app, &st, &ids, min_size.max(1), &cancel));
    if let Err(e) = spawned {
        lock(&state.dupes.inner).cancel = None;
        return Err(e.to_string());
    }
    Ok(())
}

fn run<R: Runtime>(
    app: &AppHandle<R>,
    state: &Arc<AppState>,
    volume_ids: &[String],
    min_size: u64,
    cancel: &CancelToken,
) {
    let mut candidates = Vec::new();
    let mut volumes = HashMap::new();
    for id in volume_ids {
        if let Some((key, mut c)) = volume_candidates(state, id, min_size) {
            volumes.insert(key.serial, (id.clone(), key));
            candidates.append(&mut c);
        }
    }
    let dl = downloads();
    let cfg = ScanConfig {
        min_size,
        keep: KeepContext {
            downloads: dl.clone(),
            temp_dirs: temp_dirs(),
            rules: Vec::new(),
        },
        ..ScanConfig::default()
    };
    let memory = strata_dupes::MemoryHashCache::new();
    let store_cache = state.store().cloned().map(StoreHashCache);
    let cache: &dyn HashCache = match &store_cache {
        Some(c) => c,
        None => &memory,
    };
    let last = Mutex::new(Instant::now() - Duration::from_secs(1));
    let progress = |p: &Progress| {
        {
            let mut l = lock(&last);
            if l.elapsed() < Duration::from_millis(250) && p.phase != Phase::Finished {
                return;
            }
            *l = Instant::now();
        }
        {
            let mut g = lock(&state.dupes.inner);
            g.status.phase = phase_name(p.phase);
            g.status.progress = Some(DupeProgress {
                files_done: p.files_done,
                files_total: p.files_total,
                bytes_done: p.bytes_done,
                bytes_total: p.bytes_total,
                eta_secs: p.eta_secs,
            });
        }
        emit(app, state);
    };
    let outcome = find_duplicates(candidates, &cfg, cache, cancel, &progress);
    {
        let mut g = lock(&state.dupes.inner);
        g.cancel = None;
        g.volumes = volumes;
        g.downloads = dl
            .iter()
            .map(|d| path_key(&d.display().to_string()))
            .collect();
        match outcome {
            ScanOutcome::Completed(report) => {
                g.status = DupeScanStatus {
                    state: "done",
                    last_run_ms: Some(crate::model::unix_ms()),
                    groups: report.groups.len() as u64,
                    wasted_bytes: report.groups.iter().map(|g| g.wasted_bytes()).sum(),
                    ..DupeScanStatus::default()
                };
                g.report = Some(report);
            }
            ScanOutcome::Cancelled(_) => {
                g.status = DupeScanStatus {
                    state: "cancelled",
                    last_run_ms: g.status.last_run_ms,
                    ..DupeScanStatus::default()
                };
            }
        }
    }
    emit(app, state);
}

/// Cancels a running scan; it resumes from the hash cache next time.
pub fn cancel(state: &AppState) {
    if let Some(c) = lock(&state.dupes.inner).cancel.as_ref() {
        c.cancel();
    }
}

/// Drops groups with a file that live updates changed or removed on
/// `volume_id`, and invalidates their cached hashes. Call with the index
/// after the change was applied.
pub fn on_live_changes(
    state: &AppState,
    volume_id: &str,
    index: &strata_index::Index,
    changes: &strata_index::ChangeSet,
) {
    let updated: HashSet<u32> = changes.updated.iter().map(|e| e.0).collect();
    let mut g = lock(&state.dupes.inner);
    let Some(serial) = g
        .volumes
        .iter()
        .find(|(_, (id, _))| id == volume_id)
        .map(|(s, _)| *s)
    else {
        return;
    };
    let Some(report) = g.report.as_mut() else {
        return;
    };
    let mut stale_refs = Vec::new();
    report.groups.retain(|grp| {
        let dirty = grp.files.iter().any(|f| {
            f.volume_serial == serial
                && index
                    .lookup(f.file_ref)
                    .is_none_or(|id| updated.contains(&id.0))
        });
        if dirty {
            stale_refs.extend(
                grp.files
                    .iter()
                    .filter(|f| f.volume_serial == serial)
                    .map(|f| f.file_ref),
            );
        }
        !dirty
    });
    if stale_refs.is_empty() {
        return;
    }
    let groups = report.groups.len() as u64;
    let wasted = report.groups.iter().map(|g| g.wasted_bytes()).sum();
    g.status.groups = groups;
    g.status.wasted_bytes = wasted;
    let key = g.volumes.get(&serial).map(|(_, k)| k.clone());
    drop(g);
    if let (Some(store), Some(key)) = (state.store(), key) {
        let _ = store.invalidate_hashes(&store_key(&key), &stale_refs);
    }
}

// -----------------------------------------------------------------------------
// Groups
// -----------------------------------------------------------------------------

/// One copy (`DupeFile` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DupeFile {
    /// Index within the group.
    pub file_id: usize,
    /// Volume.
    pub volume_id: String,
    /// Wire id in the current index (0 when the file is gone).
    pub entry_id: u32,
    /// Path.
    pub path: String,
    /// Modified (Unix ms).
    pub modified_ms: Option<i64>,
    /// Tier, when a rule claims it.
    pub safety: Option<&'static str>,
    /// In a Downloads folder.
    pub in_downloads: bool,
}

/// The suggested copy to keep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DupeKeep {
    /// Its index.
    pub file_id: usize,
    /// `oldest`, `shortest_path`, `not_in_downloads` or `user_rule`.
    pub reason: &'static str,
    /// The engine's explanation.
    pub explain: String,
}

/// One group (`DupeGroup` in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DupeGroup {
    /// Group id.
    pub id: u64,
    /// Size of one copy.
    pub size: u64,
    /// `size × (copies − 1)`.
    pub wasted_bytes: u64,
    /// The copies.
    pub files: Vec<DupeFile>,
    /// What to keep.
    pub keep: DupeKeep,
    /// All copies on one volume.
    pub same_volume: bool,
}

/// A page of groups.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DupeGroups {
    /// Groups in the report.
    pub total: u64,
    /// The page, by wasted bytes.
    pub groups: Vec<DupeGroup>,
}

fn keep_reason(r: &KeepReason) -> &'static str {
    match r {
        KeepReason::UserRule { .. } => "user_rule",
        KeepReason::NotInTempOrCache | KeepReason::NotInDownloads => "not_in_downloads",
        KeepReason::Oldest => "oldest",
        KeepReason::ShortestPath | KeepReason::FirstByPath => "shortest_path",
    }
}

/// Where a reported file is now: its volume and current entry.
fn locate(
    state: &AppState,
    volumes: &VolumeMap,
    serial: u64,
    fr: FileRef,
) -> Option<(String, Option<Found>)> {
    let (volume_id, _) = volumes.get(&serial)?;
    let session = state.existing_session(volume_id)?;
    let g = read(&session.data);
    let found = g.as_ref().and_then(|d| {
        let id = d.index.lookup(fr)?;
        Some((
            d.wire(id),
            d.class(id)
                .map(|c| c.safety)
                .filter(|_| crate::classify::safety_code(d.class_bits(id)) != 0),
            d.modified(id),
        ))
    });
    Some((volume_id.clone(), found))
}

/// A page of groups by wasted bytes.
#[must_use]
pub fn groups(state: &AppState, offset: usize, limit: usize) -> DupeGroups {
    let (report, volumes, downloads) = {
        let g = lock(&state.dupes.inner);
        (g.report.clone(), g.volumes.clone(), g.downloads.clone())
    };
    let Some(report) = report else {
        return DupeGroups {
            total: 0,
            groups: Vec::new(),
        };
    };
    let mut sorted: Vec<&strata_dupes::DuplicateGroup> = report.groups.iter().collect();
    sorted.sort_by_key(|g| std::cmp::Reverse(g.wasted_bytes()));
    let total = sorted.len() as u64;
    let groups = sorted
        .into_iter()
        .skip(offset)
        .take(limit.clamp(1, 500))
        .map(|grp| {
            let files: Vec<DupeFile> = grp
                .files
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    let path = f.path.display().to_string();
                    let key = path_key(&path);
                    let (volume_id, found) =
                        locate(state, &volumes, f.volume_serial, f.file_ref).unwrap_or_default();
                    DupeFile {
                        file_id: i,
                        volume_id,
                        entry_id: found.map_or(0, |x| x.0),
                        modified_ms: found
                            .and_then(|x| crate::model::epoch2000_to_ms(x.2))
                            .or_else(|| Some(f.mtime.to_unix_secs() * 1000)),
                        safety: found.and_then(|x| x.1).map(|s| match s {
                            Safety::Safe => "safe",
                            Safety::Probably => "probably",
                            Safety::Careful => "careful",
                            Safety::Never => "never",
                        }),
                        in_downloads: downloads.iter().any(|d| key.starts_with(d.as_str())),
                        path,
                    }
                })
                .collect();
            let first = grp.files.first().map(|f| f.volume_serial);
            DupeGroup {
                id: grp.id,
                size: grp.size,
                wasted_bytes: grp.wasted_bytes(),
                same_volume: grp.files.iter().all(|f| Some(f.volume_serial) == first),
                keep: DupeKeep {
                    file_id: grp.keep.index,
                    reason: keep_reason(&grp.keep.reason),
                    explain: grp.keep.reason.message(),
                },
                files,
            }
        })
        .collect();
    DupeGroups { total, groups }
}

/// Copies the user selected in one group.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DupeSelection {
    /// Group id.
    pub group_id: u64,
    /// Selected copies (indexes in the group).
    pub file_ids: Vec<usize>,
}

/// Validates selections against the report: never every copy of a group.
///
/// # Errors
///
/// No report, an unknown group, or a selection that covers every copy.
pub fn select(report: &DuplicateReport, selections: &[DupeSelection]) -> Result<Selection, String> {
    let mut sel = Selection::new(report);
    for s in selections {
        let grp = report
            .group(s.group_id)
            .ok_or_else(|| "that duplicate group is out of date; scan again".to_owned())?;
        // The keeper is the suggestion unless the user selected it; then the
        // first unselected copy keeps the data.
        let keep = (0..grp.files.len())
            .find(|i| !s.file_ids.contains(i) && *i == grp.keep.index)
            .or_else(|| (0..grp.files.len()).find(|i| !s.file_ids.contains(i)));
        sel.apply(&SelectionRequest {
            group: s.group_id,
            keep,
            marked: s.file_ids.clone(),
        })
        .map_err(|e| e.to_string())?;
    }
    Ok(sel)
}

/// The report and per-volume ids, for queueing.
#[must_use]
pub fn report_and_volumes(state: &AppState) -> Option<(DuplicateReport, VolumeMap)> {
    let g = lock(&state.dupes.inner);
    Some((g.report.clone()?, g.volumes.clone()))
}

/// Selected copies by volume id as current wire ids.
#[must_use]
pub fn selected_entries(
    state: &AppState,
    report: &DuplicateReport,
    volumes: &VolumeMap,
    selection: &Selection,
) -> BTreeMap<String, Vec<u32>> {
    let mut out: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for grp in &report.groups {
        let Some(gs) = selection.group(grp.id) else {
            continue;
        };
        for i in gs.marked() {
            let f = &grp.files[i];
            if let Some((v, Some((wire, _, _)))) =
                locate(state, volumes, f.volume_serial, f.file_ref)
            {
                out.entry(v).or_default().push(wire);
            }
        }
    }
    out
}

/// Stores hardlink prompts under a numbered id.
///
/// # Errors
///
/// The OS random source failed.
pub fn offer_hardlinks(
    state: &AppState,
    prompts: Vec<Prompt<ReplaceWithHardlinks>>,
) -> crate::error::CmdResult<(u64, i64)> {
    state.dupes.prompts.offer_numbered(prompts, Instant::now())
}

/// Takes confirmed hardlink prompts.
///
/// # Errors
///
/// Unknown, expired or too-fast confirmations.
pub fn take_hardlinks(
    state: &AppState,
    id: u64,
) -> crate::error::CmdResult<Vec<Prompt<ReplaceWithHardlinks>>> {
    Ok(state.dupes.prompts.take_numbered(id, Instant::now())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_reasons_map_to_the_ui_set() {
        assert_eq!(keep_reason(&KeepReason::Oldest), "oldest");
        assert_eq!(
            keep_reason(&KeepReason::NotInTempOrCache),
            "not_in_downloads"
        );
        assert_eq!(keep_reason(&KeepReason::FirstByPath), "shortest_path");
    }

    #[test]
    fn status_serializes_in_the_ui_shape() {
        let v = serde_json::to_value(DupeScanStatus::default()).unwrap();
        assert_eq!(v["state"], "idle");
        assert!(v.get("lastRunMs").is_some() && v.get("wastedBytes").is_some());
    }
}
