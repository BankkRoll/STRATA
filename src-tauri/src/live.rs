//! Live updates: keeping a scanned volume's index current.
//!
//! - **Journal mode** (NTFS volumes scanned with the elevated helper): a
//!   [`strata_live::Tailer`] reads the USN journal through the helper
//!   ([`HelperJournal`], [`HelperRecords`]) and applies change sets to the
//!   index under the session's write lock. The tailer saves the index cache
//!   (with the journal id and applied USN) every few minutes and when it
//!   stops; on the next launch the cache is loaded and the tailer replays the
//!   journal from that position ("catch-up") as soon as a helper connects.
//!   A wrapped or recreated journal halts with a rescan reason that is
//!   shown as an unobtrusive notice, and triggers a rescan when settings
//!   allow it.
//! - **Watch mode** (NTFS volumes the walker scanned): a recursive folder
//!   watch on the volume root ([`crate::dirwatch`]) feeds a
//!   [`strata_live::RescanPlanner`]; each settled batch re-walks the
//!   smallest set of folders and folds the result in with
//!   [`strata_live::reconcile_subtree`].
//!
//! Changed entries get the "changed recently" color-key bit, and every
//! applied change bumps the volume's `changedMs` in `volumes://changed`, so
//! views re-request their layout.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;
use strata_helper::client::{ClientError, HelperClient, UsnRead};
use strata_index::Index;
use strata_ipc::protocol::ErrorCode;
use strata_live::{
    CacheFile, Fetched, Halt, IndexAccess, JournalInfo, JournalSource, LiveEvent, LiveStatus,
    RecordSource, RescanPlanner, SourceError, SubtreeWatcher, SystemClock, Tailer, TailerConfig,
    index_position, reconcile_subtree, resolve_relative,
};
use strata_win::volume::FileSystemKind;
use tauri::{AppHandle, Emitter, Manager, Runtime};

use crate::scan::DataSlot;
use crate::state::{AppState, lock, read};

/// Event carrying a [`LiveNotice`] (journal lost, live updates stopped).
pub const LIVE_NOTICE: &str = "live://notice";

/// Longest single journal read; a read with no deadline is repeated in
/// slices this long so a stop request is seen promptly.
const READ_SLICE: Duration = Duration::from_secs(2);

/// Folder-watch notifications settle this long before folders are re-walked.
const WATCH_SETTLE: Duration = Duration::from_millis(1500);

/// How a volume is kept current.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveMode {
    /// USN journal through the helper.
    Journal,
    /// Folder watch plus re-walks.
    Watch,
}

#[derive(Debug)]
struct Running {
    mode: LiveMode,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Live updaters by volume id.
#[derive(Debug, Default)]
pub struct LiveRegistry(Mutex<HashMap<String, Running>>);

impl LiveRegistry {
    /// The mode keeping `volume_id` current, if any.
    #[must_use]
    pub fn mode(&self, volume_id: &str) -> Option<LiveMode> {
        lock(&self.0)
            .get(volume_id)
            .filter(|r| r.thread.as_ref().is_some_and(|t| !t.is_finished()))
            .map(|r| r.mode)
    }
}

/// A notice for the UI (`live://notice`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveNotice {
    /// Volume.
    pub volume_id: String,
    /// What happened, in plain words.
    pub message: String,
    /// A rescan was started because of it.
    pub rescanning: bool,
}

/// Where the index cache of a volume with this serial lives.
#[must_use]
pub fn cache_path<R: Runtime>(app: &AppHandle<R>, serial: u64) -> Option<PathBuf> {
    let dir = app.path().app_local_data_dir().ok()?.join("index");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!("{serial:016x}.idx")))
}

/// Deletes every saved index ("Clear caches").
pub fn clear_caches<R: Runtime>(app: &AppHandle<R>) {
    if let Ok(dir) = app.path().app_local_data_dir() {
        let dir = dir.join("index");
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if e.path().extension().is_some_and(|x| x == "idx") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Helper sources
// -----------------------------------------------------------------------------

fn source_error(e: &ClientError) -> SourceError {
    match e {
        ClientError::Disconnected => SourceError::Disconnected,
        _ => match e.code() {
            Some(ErrorCode::JournalNotActive) => SourceError::JournalInactive,
            Some(ErrorCode::JournalChanged) => SourceError::JournalReset,
            Some(ErrorCode::JournalWrapped) => SourceError::UsnPurged,
            Some(ErrorCode::UnknownVolume) => SourceError::VolumeGone,
            Some(ErrorCode::Cancelled) => SourceError::Cancelled,
            _ => SourceError::Io(e.to_string()),
        },
    }
}

/// [`JournalSource`] over the helper's `QueryUsnJournal` / `ReadUsn`.
#[derive(Debug)]
pub struct HelperJournal {
    client: Arc<HelperClient>,
    volume: String,
    stop: Arc<AtomicBool>,
}

impl JournalSource for HelperJournal {
    fn query(&mut self) -> Result<Option<JournalInfo>, SourceError> {
        self.client
            .query_usn_journal(self.volume.clone())
            .map(|o| {
                o.map(|i| JournalInfo {
                    journal_id: i.journal_id,
                    first_usn: i.first_usn,
                    next_usn: i.next_usn,
                    lowest_valid_usn: i.lowest_valid_usn,
                    max_usn: i.max_usn,
                })
            })
            .map_err(|e| source_error(&e))
    }

    fn read(
        &mut self,
        journal_id: u64,
        from_usn: i64,
        wait: Option<Duration>,
    ) -> Result<Vec<u8>, SourceError> {
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Err(SourceError::Cancelled);
            }
            let slice = wait.map_or(READ_SLICE, |w| w.min(READ_SLICE));
            let chunk = self
                .client
                .read_usn(&UsnRead {
                    volume: self.volume.clone(),
                    journal_id,
                    from: from_usn,
                    max_bytes: 1 << 16,
                    bytes_to_wait_for: u32::from(!slice.is_zero()),
                    wait: slice,
                })
                .map_err(|e| source_error(&e))?;
            // NOTE: "no deadline" means block until records arrive; the
            // helper bounds each wait, so empty slices are simply repeated.
            if wait.is_some() || !chunk.raw.is_empty() {
                return Ok(chunk.to_fsctl_buffer());
            }
        }
    }
}

/// [`RecordSource`] over the helper's `ReadRecords`.
#[derive(Debug)]
pub struct HelperRecords {
    client: Arc<HelperClient>,
    volume: String,
}

impl RecordSource for HelperRecords {
    fn fetch(&mut self, refs: &[strata_core::FileRef]) -> Result<Fetched, SourceError> {
        self.client
            .read_records(self.volume.clone(), refs.to_vec())
            .map(|r| Fetched {
                records: r.records,
                missing: r.missing,
            })
            .map_err(|e| source_error(&e))
    }
}

/// [`IndexAccess`] over a published volume.
struct SlotAccess(Arc<DataSlot>);

impl IndexAccess for SlotAccess {
    fn with_index(&mut self, f: &mut dyn FnMut(&mut Index)) -> bool {
        let mut g = self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match g.as_mut() {
            Some(d) if !d.preview => {
                f(&mut d.index);
                true
            }
            _ => false,
        }
    }
}

// -----------------------------------------------------------------------------
// Start and stop
// -----------------------------------------------------------------------------

struct Target {
    root: PathBuf,
    serial: u64,
    ntfs: bool,
}

fn target(state: &AppState, volume_id: &str) -> Option<Target> {
    let reg = lock(&state.registry);
    let e = reg.volumes.get(volume_id)?;
    if !e.present {
        return None;
    }
    Some(Target {
        root: e.info.root_path()?,
        serial: u64::from(e.info.serial.unwrap_or(0)),
        ntfs: e.info.filesystem == FileSystemKind::Ntfs,
    })
}

fn live_enabled(state: &AppState) -> bool {
    state
        .store()
        .and_then(|s| s.load_settings().ok())
        .is_none_or(|s| s.live.usn_enabled)
}

/// Stops live updates of a volume (before a rescan replaces its index, or
/// when it is removed). The updater saves the cache before it exits.
pub fn stop(state: &AppState, volume_id: &str) {
    let r = lock(&state.live.0).remove(volume_id);
    if let Some(mut r) = r {
        r.stop.store(true, Ordering::Release);
        if let Some(t) = r.thread.take() {
            let _ = t.join();
        }
    }
}

/// Stops every updater (app exit); each saves its cache.
pub fn shutdown(state: &AppState) {
    let ids: Vec<String> = lock(&state.live.0).keys().cloned().collect();
    for id in ids {
        stop(state, &id);
    }
}

/// Starts keeping `volume_id` current: from the journal when the index has
/// a journal position and a helper is connected, else by watching folders
/// (NTFS volumes only: other filesystems have no stable file ids to match a
/// re-walk against the index).
pub fn start<R: Runtime>(app: &AppHandle<R>, state: &Arc<AppState>, volume_id: &str) {
    if !live_enabled(state) || state.live.mode(volume_id).is_some() {
        return;
    }
    let Some(t) = target(state, volume_id) else {
        return;
    };
    let Some(session) = state.existing_session(volume_id) else {
        return;
    };
    let pos = {
        let g = read(&session.data);
        match g.as_ref() {
            Some(d) if !d.preview => index_position(&d.index),
            _ => return,
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let client = state.helper.client().filter(|_| pos.journal_id != 0);
    let (mode, thread) = match client {
        Some(client) => {
            let (app, st, vid, stop2) = (
                app.clone(),
                state.clone(),
                volume_id.to_owned(),
                stop.clone(),
            );
            let cache = cache_path(&app, t.serial).map(CacheFile::new);
            (
                LiveMode::Journal,
                std::thread::Builder::new()
                    .name("strata-live-usn".into())
                    .spawn(move || {
                        run_journal(&app, &st, &vid, client, session.data.clone(), cache, stop2)
                    }),
            )
        }
        None if t.ntfs => {
            let (app, st, vid, stop2) = (
                app.clone(),
                state.clone(),
                volume_id.to_owned(),
                stop.clone(),
            );
            let cache = cache_path(&app, t.serial).map(CacheFile::new);
            (
                LiveMode::Watch,
                std::thread::Builder::new()
                    .name("strata-live-watch".into())
                    .spawn(move || {
                        run_watch(
                            &app,
                            &st,
                            &vid,
                            &t.root,
                            session.data.clone(),
                            cache,
                            &stop2,
                        )
                    }),
            )
        }
        None => return,
    };
    if let Ok(thread) = thread {
        lock(&state.live.0).insert(
            volume_id.to_owned(),
            Running {
                mode,
                stop,
                thread: Some(thread),
            },
        );
        crate::jobs::emit_volumes(app, state);
    }
}

/// A helper connected: switch every journal-capable volume to journal mode
/// (catching up from its saved position).
pub fn on_helper_connected<R: Runtime>(app: &AppHandle<R>, state: &Arc<AppState>) {
    let ids: Vec<String> = lock(&state.sessions).keys().cloned().collect();
    for id in ids {
        let has_pos = state.existing_session(&id).is_some_and(|s| {
            read(&s.data)
                .as_ref()
                .is_some_and(|d| !d.preview && d.index.volume().usn_journal_id != 0)
        });
        if has_pos && state.live.mode(&id) != Some(LiveMode::Journal) {
            stop(state, &id);
            start(app, state, &id);
        }
    }
}

fn notice<R: Runtime>(app: &AppHandle<R>, state: &Arc<AppState>, volume_id: &str, message: String) {
    let rescan = state
        .store()
        .and_then(|s| s.load_settings().ok())
        .is_some_and(|s| s.live.auto_rescan_on_journal_loss);
    if let Some(e) = lock(&state.registry).volumes.get_mut(volume_id) {
        e.status.notice = Some(message.clone());
    }
    let _ = app.emit(
        LIVE_NOTICE,
        LiveNotice {
            volume_id: volume_id.to_owned(),
            message,
            rescanning: rescan,
        },
    );
    if rescan {
        let _ = crate::jobs::start(app, state, volume_id, crate::jobs::ScanMode::Auto);
    } else {
        crate::jobs::emit_volumes(app, state);
    }
}

fn applied<R: Runtime>(
    app: &AppHandle<R>,
    state: &Arc<AppState>,
    volume_id: &str,
    slot: &DataSlot,
    changes: Option<&strata_index::ChangeSet>,
) {
    {
        let mut g = slot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(d) = g.as_mut() else { return };
        d.after_change(changes);
        if let Some(c) = changes {
            crate::dupes::on_live_changes(state, volume_id, &d.index, c);
        }
    }
    crate::jobs::emit_volumes(app, state);
}

#[allow(clippy::too_many_arguments)]
fn run_journal<R: Runtime>(
    app: &AppHandle<R>,
    state: &Arc<AppState>,
    volume_id: &str,
    client: Arc<HelperClient>,
    slot: Arc<DataSlot>,
    cache: Option<CacheFile>,
    stop: Arc<AtomicBool>,
) {
    let mut journal = HelperJournal {
        client: client.clone(),
        volume: volume_id.to_owned(),
        stop: stop.clone(),
    };
    let mut records = HelperRecords {
        client,
        volume: volume_id.to_owned(),
    };
    let pos = match read(&slot).as_ref() {
        Some(d) => index_position(&d.index),
        None => return,
    };
    let tick = state
        .store()
        .and_then(|s| s.load_settings().ok())
        .map_or(250, |s| s.live.update_tick_ms);
    let cfg = TailerConfig {
        tick: Duration::from_millis(u64::from(tick)),
        ..TailerConfig::default()
    };
    let mut tailer = match Tailer::start(cfg, &mut journal, pos) {
        Ok(t) => t,
        Err(h) => return halted(app, state, volume_id, &h),
    };
    if let Some(c) = cache {
        tailer = tailer.with_cache(c);
    }
    let mut access = SlotAccess(slot.clone());
    let halt = tailer.run(
        &mut journal,
        &mut records,
        &mut access,
        &SystemClock,
        &stop,
        &mut |ev| match ev {
            LiveEvent::Tick(r) => applied(app, state, volume_id, &slot, Some(&r.changes)),
            LiveEvent::Status(LiveStatus::CatchingUp { pending, .. }) => {
                set_catch_up(app, state, volume_id, Some(pending));
            }
            LiveEvent::Status(LiveStatus::Live) => set_catch_up(app, state, volume_id, None),
            LiveEvent::Saved(_) | LiveEvent::SaveFailed(_) => {}
        },
    );
    halted(app, state, volume_id, &halt);
}

fn set_catch_up<R: Runtime>(
    app: &AppHandle<R>,
    state: &Arc<AppState>,
    volume_id: &str,
    pending: Option<usize>,
) {
    if let Some(e) = lock(&state.registry).volumes.get_mut(volume_id) {
        e.status.catching_up = pending;
    }
    crate::jobs::emit_volumes(app, state);
}

fn halted<R: Runtime>(app: &AppHandle<R>, state: &Arc<AppState>, volume_id: &str, h: &Halt) {
    match h {
        Halt::Stopped => {}
        Halt::JournalDisabled => notice(
            app,
            state,
            volume_id,
            "Live updates are unavailable: the volume's change journal is turned off.".into(),
        ),
        Halt::NeedsRescan(reason) => notice(
            app,
            state,
            volume_id,
            format!("Live updates stopped: {reason}. A new scan brings the view up to date."),
        ),
        Halt::Stale(_) => {
            // NOTE: the helper went away or the volume was dismounted; the
            // last index stays browsable and tailing resumes from its
            // position when a helper connects again.
        }
    }
    if let Some(e) = lock(&state.registry).volumes.get_mut(volume_id) {
        e.status.catching_up = None;
    }
    crate::jobs::emit_volumes(app, state);
}

#[allow(clippy::too_many_arguments)]
fn run_watch<R: Runtime>(
    app: &AppHandle<R>,
    state: &Arc<AppState>,
    volume_id: &str,
    root: &std::path::Path,
    slot: Arc<DataSlot>,
    cache: Option<CacheFile>,
    stop: &AtomicBool,
) {
    let Ok(mut watcher) = crate::dirwatch::DirWatcher::new(root) else {
        return;
    };
    let mut planner = RescanPlanner::default();
    let mut last_event: Option<Instant> = None;
    while !stop.load(Ordering::Acquire) {
        match watcher.next(Some(Duration::from_millis(500))) {
            Ok(batch) => {
                let empty = matches!(&batch, strata_live::WatchBatch::Changes(c) if c.is_empty());
                if !empty {
                    planner.push(batch);
                    last_event = Some(Instant::now());
                }
            }
            Err(_) => break,
        }
        if last_event.is_some_and(|t| t.elapsed() >= WATCH_SETTLE) && !planner.is_empty() {
            last_event = None;
            for target in planner.take() {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                rewalk(app, state, volume_id, root, &slot, &target);
            }
        }
    }
    if let Some(c) = cache {
        let mut g = slot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(d) = g.as_mut().filter(|d| !d.preview) {
            let pos = index_position(&d.index);
            let _ = c.save(&mut d.index, pos);
        }
    }
}

/// Re-walks one folder after change notifications and folds the result in.
fn rewalk<R: Runtime>(
    app: &AppHandle<R>,
    state: &Arc<AppState>,
    volume_id: &str,
    root: &std::path::Path,
    slot: &DataSlot,
    target: &strata_live::RescanTarget,
) {
    // The nearest folder that is still in the index; a folder that vanished
    // means its parent is walked instead.
    let (scope, path, deep) = {
        let g = read(slot);
        let Some(d) = g.as_ref() else { return };
        let mut comps = target.path.clone();
        let mut deep = target.deep;
        loop {
            if let Some(id) =
                resolve_relative(&d.index, d.index.root(), &comps).filter(|&id| d.index.is_dir(id))
            {
                let mut p = root.to_path_buf();
                for c in &comps {
                    p.push(c.to_string_lossy());
                }
                break (id, p, deep);
            }
            if comps.pop().is_none() {
                return;
            }
            deep = false;
        }
    };
    let scope_ref = read(slot).as_ref().and_then(|d| d.index.file_ref(scope));
    let opts = strata_walk::WalkOptions {
        threads: 2,
        max_depth: (!deep).then_some(1),
        ..strata_walk::WalkOptions::default()
    };
    let Ok(walker) = strata_walk::Walker::new(&path, opts) else {
        return;
    };
    let mut recs: Vec<strata_core::ScanRecord> = Vec::new();
    if walker
        .run(&mut recs, &strata_walk::CancelToken::new())
        .is_err()
    {
        return;
    }
    // The walk's root links to itself; the index already places the folder.
    recs.retain(|r| Some(r.id) != scope_ref && !r.links.iter().any(|l| l.parent == r.id));
    let changes = {
        let mut g = slot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(d) = g.as_mut() else { return };
        reconcile_subtree(&mut d.index, scope, recs, deep).ok()
    };
    if changes
        .as_ref()
        .is_some_and(|c| !c.created.is_empty() || !c.updated.is_empty() || !c.removed.is_empty())
    {
        applied(app, state, volume_id, slot, changes.as_ref());
    }
}

// -----------------------------------------------------------------------------
// Cache on launch
// -----------------------------------------------------------------------------

/// Loads the saved index of every present volume that has one, publishes it
/// (shown as a snapshot until live updates catch up), and starts catching up
/// through the helper when one connects without a prompt (service mode).
/// Runs on the startup thread after the classifier loaded.
pub fn restore_cached<R: Runtime>(app: &AppHandle<R>, state: &Arc<AppState>) {
    // The volume watcher fills the registry on its own thread.
    for _ in 0..50 {
        if !lock(&state.registry).volumes.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let Ok(engine) = state.engine.wait() else {
        return;
    };
    let volumes: Vec<(String, u64, String)> = lock(&state.registry)
        .volumes
        .iter()
        .filter(|(_, e)| e.present && e.info.filesystem == FileSystemKind::Ntfs)
        .filter_map(|(id, e)| {
            let root = e.info.root_path()?.to_string_lossy().into_owned();
            Some((id.clone(), u64::from(e.info.serial?), root))
        })
        .collect();
    let mut any_journal = false;
    for (id, serial, root) in volumes {
        if state.has_index(&id) {
            continue;
        }
        let Some(path) = cache_path(app, serial) else {
            continue;
        };
        let file = CacheFile::new(&path);
        let Ok(mut index) = file.load(serial) else {
            continue;
        };
        let saved_ms = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0));
        let root_display = crate::shell::strip_verbatim(&root);
        let journal = index.volume().usn_journal_id != 0;
        any_journal |= journal;
        let slots = crate::classify::ext_slots(&index);
        let classified =
            crate::classify::classify_index(&engine, &mut index, &root_display, &slots);
        let mut data = crate::model::VolumeData::new(
            0,
            index,
            classified,
            root_display,
            if journal {
                crate::model::ScannerUsed::Mft
            } else {
                crate::model::ScannerUsed::Walker
            },
        );
        data.scanned_at_ms = saved_ms;
        data.partial = data
            .index
            .aggregate(data.index.root())
            .is_some_and(|a| a.partial);
        let session = state.session(&id);
        *session
            .data
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(data);
    }
    crate::jobs::emit_volumes(app, state);
    let service = state
        .store()
        .and_then(|s| s.load_settings().ok())
        .is_some_and(|s| s.helper.mode == strata_store::HelperMode::Service);
    if any_journal && service && state.helper.available() {
        let _ = crate::helper::connect_quietly(app, state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_errors_map_to_source_errors() {
        let remote = |code| {
            ClientError::Remote(strata_helper::client::RemoteError {
                code,
                message: String::new(),
                audit: Vec::new(),
            })
        };
        assert_eq!(
            source_error(&remote(ErrorCode::JournalNotActive)),
            SourceError::JournalInactive
        );
        assert_eq!(
            source_error(&remote(ErrorCode::JournalChanged)),
            SourceError::JournalReset
        );
        assert_eq!(
            source_error(&remote(ErrorCode::JournalWrapped)),
            SourceError::UsnPurged
        );
        assert_eq!(
            source_error(&remote(ErrorCode::UnknownVolume)),
            SourceError::VolumeGone
        );
        assert_eq!(
            source_error(&ClientError::Disconnected),
            SourceError::Disconnected
        );
        assert!(matches!(
            source_error(&remote(ErrorCode::Io)),
            SourceError::Io(_)
        ));
    }
}
