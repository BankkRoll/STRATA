//! Scan jobs: choosing a scanner, running it on its own thread, publishing
//! progress and results, and writing the history snapshot.
//!
//! Scanner choice:
//! - `standard`: the walker.
//! - `auto`: the MFT scan through the helper when one is already connected
//!   and the volume is local NTFS; otherwise the walker. Never prompts.
//! - `fast`: like `auto`, but launches the helper (UAC prompt) first. If
//!   the user declines, or this build has no helper, the scan falls back to
//!   the walker and the helper status records why (the UI's "Standard scan"
//!   banner).
//!
//! A helper that crashes mid-scan leaves the previous index published (if
//! any), records the error on the volume and flips the helper state to
//! disconnected.

use std::sync::Arc;

use strata_store::{DirAggregate, ScannerKind as StoreScanner, Store, VolumeKey, VolumeTotals};
use strata_win::volume::{DriveKind, FileSystemKind};
use tauri::{AppHandle, Emitter, Runtime};

pub use crate::helper::HELPER_CHANGED;

use crate::error::{CmdResult, CommandError, ErrorCode};
use crate::model::{ScannerUsed, VolumeData};
use crate::scan::{Ingest, ScanObserver, ScanProgress, ScanTarget, ShadowBytes, VolumeFacts, walk};
use crate::state::{AppState, ScanCancel, Session, lock, read};
use crate::volumes::to_dto;

/// Event carrying the full volume list.
pub const VOLUMES_CHANGED: &str = "volumes://changed";

/// Requested scanner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScanMode {
    /// MFT if a helper is connected.
    Auto,
    /// MFT, launching the helper if needed.
    Fast,
    /// The walker.
    Standard,
}

/// The wire volume list.
pub fn volume_list(state: &AppState) -> Vec<crate::volumes::VolumeDto> {
    let reg = lock(&state.registry);
    reg.volumes
        .iter()
        .map(|(id, e)| {
            let session = state.existing_session(id);
            let guard = session.as_ref().map(|s| read(&s.data));
            to_dto(
                e,
                guard.as_ref().and_then(|g| g.as_ref()),
                state.live.mode(id).is_some(),
            )
        })
        .collect()
}

/// Emits `volumes://changed`.
pub fn emit_volumes<R: Runtime>(app: &AppHandle<R>, state: &AppState) {
    let _ = app.emit(VOLUMES_CHANGED, volume_list(state));
}

struct Observer<R: Runtime> {
    app: AppHandle<R>,
    state: Arc<AppState>,
    volume_id: String,
}

impl<R: Runtime> ScanObserver for Observer<R> {
    fn progress(&self, p: &ScanProgress) {
        if let Some(e) = lock(&self.state.registry).volumes.get_mut(&self.volume_id) {
            e.status.progress = Some(*p);
        }
        emit_volumes(&self.app, &self.state);
    }

    fn preview(&self) {
        emit_volumes(&self.app, &self.state);
    }
}

/// Starts a scan of `volume_id` on a new thread.
///
/// # Errors
///
/// Unknown, absent, locked or busy volumes.
pub fn start<R: Runtime>(
    app: &AppHandle<R>,
    state: &Arc<AppState>,
    volume_id: &str,
    mode: ScanMode,
) -> CmdResult<()> {
    let (target, ntfs_local) = {
        let mut reg = lock(&state.registry);
        let e = reg
            .volumes
            .get_mut(volume_id)
            .ok_or_else(|| CommandError::not_found("unknown volume"))?;
        if !e.present {
            return Err(CommandError::unavailable("the volume is not connected"));
        }
        if e.info.bitlocker == strata_win::volume::BitLockerState::Locked || !e.info.ready {
            return Err(CommandError::unavailable(
                "the volume is locked or not ready; unlock it in Windows first",
            ));
        }
        if e.status.running {
            return Err(CommandError::new(
                ErrorCode::Busy,
                "a scan of this volume is already running",
            ));
        }
        let root = e
            .info
            .root_path()
            .ok_or_else(|| CommandError::unavailable("the volume has no path to scan"))?;
        let root_display = crate::shell::strip_verbatim(&root.to_string_lossy());
        let facts = e.info.guid_path.clone().map(|guid_path| VolumeFacts {
            guid_path,
            serial: u64::from(e.info.serial.unwrap_or(0)),
            total_bytes: e.info.total_bytes.unwrap_or(0),
            free_bytes: e.info.free_bytes.unwrap_or(0),
        });
        e.status.running = true;
        e.status.progress = None;
        e.status.error = None;
        e.status.notice = None;
        let ntfs_local = e.info.filesystem == FileSystemKind::Ntfs
            && matches!(e.info.kind, DriveKind::Fixed | DriveKind::Removable);
        (
            ScanTarget {
                root,
                root_display,
                volume: facts,
            },
            ntfs_local,
        )
    };
    let session = state.session(volume_id);
    let cancel = Arc::new(ScanCancel::default());
    *lock(&session.cancel) = Some(cancel.clone());
    emit_volumes(app, state);

    let app = app.clone();
    let state = state.clone();
    let volume_id = volume_id.to_owned();
    let spawned = std::thread::Builder::new()
        .name("strata-scan".into())
        .spawn(move || {
            let result = run(
                &app, &state, &session, &volume_id, target, mode, ntfs_local, &cancel,
            );
            *lock(&session.cancel) = None;
            if let Some(e) = lock(&state.registry).volumes.get_mut(&volume_id) {
                e.status.running = false;
                e.status.progress = None;
                e.status.error = result.err();
            }
            emit_volumes(&app, &state);
        });
    spawned
        .map(|_| ())
        .map_err(|e| CommandError::internal(e.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn run<R: Runtime>(
    app: &AppHandle<R>,
    state: &Arc<AppState>,
    session: &Session,
    volume_id: &str,
    target: ScanTarget,
    mode: ScanMode,
    ntfs_local: bool,
    cancel: &ScanCancel,
) -> Result<(), String> {
    let engine = state.engine.wait()?;
    let observer = Observer {
        app: app.clone(),
        state: state.clone(),
        volume_id: volume_id.to_owned(),
    };
    let helper = if !ntfs_local || mode == ScanMode::Standard {
        None
    } else if mode == ScanMode::Fast {
        crate::helper::elevate(app, state).ok()
    } else {
        state.helper.client()
    };
    // The index is about to be replaced; its live updates stop with it.
    crate::live::stop(state, volume_id);
    let facts = target.volume.clone();
    let mut journal = None;
    let (scanner, cancelled) = if let (Some(client), Some(f)) = (helper, facts.as_ref()) {
        // Changes made while the scan runs are replayed from here.
        journal = client.query_usn_journal(f.guid_path.clone()).ok().flatten();
        let mut ingest = Ingest::new(
            engine,
            target,
            session.data.clone(),
            ScannerUsed::Mft,
            false,
        );
        let stats = crate::helper::scan_volume(
            &client,
            &f.guid_path,
            &mut ingest,
            &cancel.helper,
            &observer,
        )
        .map_err(|e| e.to_string())?;
        ingest
            .finish(stats.cancelled, shadow_bytes(Some(f)))
            .map_err(|e| e.to_string())?;
        (StoreScanner::Mft, stats.cancelled)
    } else {
        let mut ingest = Ingest::new(
            engine,
            target,
            session.data.clone(),
            ScannerUsed::Walker,
            true,
        );
        let stats = walk(
            &mut ingest,
            strata_walk::WalkOptions::default(),
            &cancel.walker,
            &observer,
        )
        .map_err(|e| e.to_string())?;
        ingest
            .finish(stats.partial, shadow_bytes(facts.as_ref()))
            .map_err(|e| e.to_string())?;
        (StoreScanner::Walker, stats.cancelled)
    };
    if let Some(j) = journal
        && let Some(data) = write(&session.data).as_mut()
    {
        data.index.set_usn_position(j.journal_id, j.next_usn);
    }
    emit_volumes(app, state);
    if let (Some(f), false, Some(store)) = (facts.as_ref(), cancelled, state.store()) {
        let guard = read(&session.data);
        if let Some(data) = guard.as_ref() {
            let _ = save_snapshot(store, f, data, scanner);
        }
    }
    if !cancelled {
        crate::live::start(app, state, volume_id);
    }
    Ok(())
}

/// Removes deleted entries (`(volume id, wire id)`, with their subtrees)
/// from the published indexes, so every view shows the space as freed
/// without waiting for live updates or a rescan.
pub fn forget_entries<R: Runtime>(app: &AppHandle<R>, state: &AppState, gone: &[(String, u32)]) {
    let mut touched = false;
    for (volume_id, wire) in gone {
        let Some(session) = state.existing_session(volume_id) else {
            continue;
        };
        let mut g = write(&session.data);
        let Some(data) = g.as_mut() else { continue };
        let Some(id) = data.resolve(*wire) else {
            continue;
        };
        let mut refs = Vec::new();
        data.index.for_each_in_subtree(id, |e| {
            if let Some(fr) = data.index.file_ref(e) {
                refs.push(fr);
            }
        });
        // Children first, so no removal leaves an orphan behind.
        refs.reverse();
        let _ = data.apply(refs.into_iter().map(strata_index::Update::Remove));
        touched = true;
    }
    if touched {
        emit_volumes(app, state);
    }
}

fn write<T>(m: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn shadow_bytes(facts: Option<&VolumeFacts>) -> ShadowBytes {
    let Some(f) = facts else {
        return ShadowBytes::default();
    };
    ShadowBytes {
        used: strata_win::shadow::shadow_storage()
            .for_volume(&f.guid_path)
            .map(|s| s.used_bytes),
    }
}

/// Writes the history snapshot of a complete whole-volume scan.
///
/// # Errors
///
/// Store errors.
pub fn save_snapshot(
    store: &Store,
    f: &VolumeFacts,
    data: &VolumeData,
    scanner: StoreScanner,
) -> strata_store::Result<()> {
    use strata_core::SizeMode;
    let ix = &data.index;
    let root = ix.aggregate(ix.root()).unwrap_or_default();
    let key = VolumeKey {
        serial: f.serial,
        guid_path: f.guid_path.clone(),
    };
    let totals = VolumeTotals {
        total_bytes: f.total_bytes,
        free_bytes: f.free_bytes,
        allocated_sum: root.allocated,
        logical_sum: root.logical,
        file_count: u64::from(root.files),
        dir_count: u64::from(root.dirs),
        scanner,
    };
    let mut w = store.begin_snapshot(&key, totals);
    let min = strata_store::SnapshotOptions::default().min_dir_bytes;
    let mut dirs = Vec::new();
    for i in 0..ix.slot_count() {
        let id = strata_index::EntryId(i as u32);
        if !ix.is_live(id) || !ix.is_dir(id) {
            continue;
        }
        let (a, l) = (
            ix.size(id, SizeMode::Allocated),
            ix.size(id, SizeMode::Logical),
        );
        if a.max(l) < min {
            continue;
        }
        let files = ix.aggregate(id).map_or(0, |g| u64::from(g.files));
        dirs.push(DirAggregate {
            path: ix.path_string(id),
            allocated: a,
            logical: l,
            files,
        });
    }
    w.add_dirs(dirs);
    w.commit().map(|_| ())
}
