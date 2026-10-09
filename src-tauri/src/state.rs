//! Shared app state and the threading model.
//!
//! # Threading model
//!
//! - **Tauri main thread:** window events and the few shell calls that must
//!   run there (`SHObjectProperties`, `SHOpenFolderAndSelectItems`).
//!   Commands never run heavy work on it: every command that touches an
//!   index is `async` and moves its work to Tauri's blocking pool
//!   (`spawn_blocking`).
//! - **Startup loader thread:** resolves known folders, compiles the rules
//!   ([`EngineCell`]), then reads the installed-apps catalog and loads cached
//!   indexes. Scans that finish before the engine is ready wait for it.
//! - **Volume watcher thread:** `strata_win::watcher::VolumeWatcher` events
//!   update the [`Registry`] and emit `volumes://changed`.
//! - **One scan thread per running scan:** drives the walker (whose rayon
//!   pool lists directories) or pumps the helper's `ScanVolume` stream into
//!   an [`crate::scan::Ingest`], publishes the preview and the final index,
//!   writes the history snapshot.
//! - **Helper client dispatcher and watchdog:** route pipe responses to
//!   waiting requests; the watchdog notices a lost helper.
//! - **Live threads:** one USN tailer or folder watcher per indexed volume
//!   ([`crate::live`]); it takes the write side of the session briefly per
//!   applied batch.
//! - **Duplicate scan thread** ([`crate::dupes`]): one at a time.
//! - **Activity thread** ([`crate::activity`]): drains the helper's ETW
//!   stream into the store while tracking runs.
//! - **Search threads:** one per query, cancelled by the next query.
//!
//! Each volume's index lives in a [`Session`] behind an `RwLock`. Readers
//! (layout, rows, info, search) take the read side for the length of one
//! request; the scan thread takes the write side briefly to apply a preview
//! batch or to swap in a finished index.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::classify::EngineCell;
use crate::helper::HelperManager;
use crate::layout_pipe::LayoutStream;
use crate::scan::DataSlot;
use crate::search::SearchStream;
use crate::volumes::Registry;

/// One volume's index and scan control.
#[derive(Debug, Default)]
pub struct Session {
    /// The published index (preview or final).
    pub data: Arc<DataSlot>,
    /// Set to cancel the running scan.
    pub cancel: Mutex<Option<Arc<ScanCancel>>>,
}

/// Cancellation of one scan, for both scanners.
#[derive(Debug, Default)]
pub struct ScanCancel {
    /// The walker's token.
    pub walker: strata_walk::CancelToken,
    /// Checked by the helper pump.
    pub helper: AtomicBool,
}

impl ScanCancel {
    /// Requests cancellation.
    pub fn cancel(&self) {
        self.walker.cancel();
        self.helper.store(true, Ordering::Release);
    }
}

/// Everything the commands share.
#[derive(Default)]
pub struct AppState {
    /// Rules and catalog (loaded at startup).
    pub engine: EngineCell,
    /// The store the shell features opened (`None` if unavailable); set once
    /// in setup from `features::store::handle`.
    pub store: OnceLock<Option<strata_store::Store>>,
    /// Known volumes.
    pub registry: Mutex<Registry>,
    /// Index sessions by volume id.
    pub sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// The elevated helper.
    pub helper: HelperManager,
    /// Open layout streams.
    pub layouts: Mutex<HashMap<u32, Arc<LayoutStream>>>,
    /// Open search streams.
    pub searches: Mutex<HashMap<u32, Arc<SearchStream>>>,
    /// Stream id source.
    pub next_stream: AtomicU32,
    /// Live updates per volume.
    pub live: crate::live::LiveRegistry,
    /// The duplicate finder.
    pub dupes: crate::dupes::DupeState,
    /// File-activity tracking.
    pub activity: crate::activity::ActivityState,
    /// Last-access times are unreliable on this machine (updates disabled or
    /// system-managed).
    pub access_unreliable: AtomicBool,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState").finish_non_exhaustive()
    }
}

/// Locks a mutex, recovering from poisoning (a panicked command must not
/// take the app down).
pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Read-locks, recovering from poisoning.
pub fn read<T>(m: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl AppState {
    /// The session of a volume, created on first use.
    pub fn session(&self, volume_id: &str) -> Arc<Session> {
        lock(&self.sessions)
            .entry(volume_id.to_owned())
            .or_default()
            .clone()
    }

    /// The session of a volume, if it was ever scanned.
    pub fn existing_session(&self, volume_id: &str) -> Option<Arc<Session>> {
        lock(&self.sessions).get(volume_id).cloned()
    }

    /// Whether a volume has an index.
    pub fn has_index(&self, volume_id: &str) -> bool {
        self.existing_session(volume_id)
            .is_some_and(|s| read(&s.data).is_some())
    }

    /// A fresh stream id.
    pub fn stream_id(&self) -> u32 {
        self.next_stream.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The store, if it opened.
    pub fn store(&self) -> Option<&strata_store::Store> {
        self.store.get().and_then(Option::as_ref)
    }
}
