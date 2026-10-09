//! SQLite persistence for Strata.
//!
//! Responsibilities:
//! - Volume snapshots of directory aggregates, diffs, usage and per-directory
//!   series, and retention thinning ([`snapshot`](Store::begin_snapshot),
//!   [`diff`](Store::diff), [`apply_retention`](Store::apply_retention)).
//! - Typed, versioned settings with export/import ([`Settings`]).
//! - The write-ahead undo/audit log for deletes ([`Store::begin_action`]).
//! - ETW hourly activity rollups and last-writer rows.
//! - The duplicate finder's hash cache.
//! - License storage.
//!
//! # Files
//!
//! A store is a directory with two SQLite databases:
//!
//! | File         | Contents                                       | `synchronous` |
//! |--------------|------------------------------------------------|---------------|
//! | `history.db` | snapshots, activity, hash cache (all derived)  | `NORMAL`      |
//! | `state.db`   | settings, undo/audit log, license              | `FULL`        |
//!
//! Splitting them means [`Store::reset_history`] can move a corrupt history
//! aside without losing settings, the undo log (which Recycle Bin restores
//! depend on) or the license, and a long snapshot commit never delays the
//! write-ahead record of a delete.
//!
//! # Failure model
//!
//! Nothing here panics. [`Store::open`] only fails if the directory cannot be
//! created; a damaged or too-new database is reported by [`Store::health`]
//! and by [`StoreError::Corrupt`] / [`StoreError::TooNew`] from each call that
//! touches it, while the other database keeps working.
//!
//! # Threading
//!
//! [`Store`] is a cheap `Clone + Send + Sync` handle (an `Arc`). Each database
//! has one writer connection, serialized by a mutex, and a pool of reader
//! connections; with WAL, readers never block on the writer and see a
//! consistent snapshot for the duration of each call. Every multi-row write
//! (a snapshot, an activity flush, a hash batch) is a single transaction.
//! Calls block, so the app should make them from a background thread rather
//! than the UI/IPC thread.
//!
//! # Example
//!
//! ```
//! use strata_store::{DbHealth, Store};
//! let dir = tempfile::tempdir().unwrap();
//! let store = Store::open(dir.path()).unwrap();
//! let health = store.health();
//! if health.history != DbHealth::Ok {
//!     // Offer "Reset history" in the UI; this never touches settings.
//!     store.reset_history().unwrap();
//! }
//! let settings = store.load_settings().unwrap();
//! assert_eq!(settings.history.retention_days, 90);
//! ```

#![forbid(unsafe_code)]

mod activity;
mod clock;
mod codec;
mod db;
mod diff;
mod error;
mod hashes;
mod license;
mod path;
mod retention;
mod schema;
mod settings;
mod snapshot;
mod undo;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use activity::{ActivityPruneReport, ActivitySample, LastWrite, WriterTotal};
pub use clock::{CivilUtc, Clock, ManualClock, SystemClock, Timestamp};
pub use db::DbHealth;
pub use diff::{DiffOptions, DirChange, SinceLastScan, SnapshotDiff};
pub use error::{DbKind, Result, StoreError};
pub use hashes::{CachedHash, HashKey};
pub use license::LicenseRecord;
pub use path::{normalize_path, parent_hash, path_hash};
pub use retention::{RetentionPolicy, RetentionReport};
pub use settings::{
    ActivitySettings, AppearanceSettings, CleanupMethod, CleanupSettings, ColorMode, HelperMode,
    HelperSettings, HistorySettings, LiveSettings, PrivacySettings, RulesSettings, ScanSettings,
    Settings, SettingsIssue, SizeUnits, StartupSettings, Theme, TraySettings, TreemapStyle,
    UpdateChannel, UpdateSettings,
};
pub use snapshot::{
    DirAggregate, DirPoint, DirSizes, ScannerKind, SnapshotId, SnapshotInfo, SnapshotOptions,
    SnapshotWriter, UsagePoint, VolumeKey, VolumeSummary, VolumeTotals,
};
pub use undo::{
    ActionId, ActionKind, ActionRecord, ActionStatus, ActionSummary, DeleteMethod, ItemId,
    ItemOutcome, ItemRecord, ItemResult, PlannedItem, RestoreInfo,
};

use db::{Db, HISTORY_SPEC, STATE_SPEC};

/// Bit-casts a `u64` for an SQLite `INTEGER` column (lossless round trip with
/// [`i2u`]).
pub(crate) const fn u2i(v: u64) -> i64 {
    v as i64
}

/// Inverse of [`u2i`].
pub(crate) const fn i2u(v: i64) -> u64 {
    v as u64
}

/// Health of both database files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreHealth {
    /// `history.db`.
    pub history: DbHealth,
    /// `state.db`.
    pub state: DbHealth,
}

/// Where a reset moved the old database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetReport {
    /// The renamed file (`<name>.corrupt-<UTC timestamp>`), or `None` if
    /// there was no file to move.
    pub moved_to: Option<PathBuf>,
}

struct Inner {
    dir: PathBuf,
    history: Db,
    state: Db,
    clock: Arc<dyn Clock>,
}

/// Handle to the store. Cheap to clone; share it across threads.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("dir", &self.inner.dir)
            .field("clock", &self.inner.clock)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Opens (creating if needed) the store in `dir` using the system clock.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] if `dir` cannot be created. Database problems do
    /// not fail the open; see [`health`](Self::health).
    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_with_clock(dir, Arc::new(SystemClock))
    }

    /// Opens the store with an injected clock (tests, diagnostics).
    ///
    /// # Errors
    ///
    /// As for [`open`](Self::open).
    pub fn open_with_clock(dir: &Path, clock: Arc<dyn Clock>) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|source| StoreError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let history = Db::open(&HISTORY_SPEC, dir.join(DbKind::History.file_name()));
        let state = Db::open(&STATE_SPEC, dir.join(DbKind::State.file_name()));
        Ok(Self {
            inner: Arc::new(Inner {
                dir: dir.to_path_buf(),
                history,
                state,
                clock,
            }),
        })
    }

    /// Directory holding the database files.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// Current health of both databases, including corruption detected by
    /// earlier calls.
    #[must_use]
    pub fn health(&self) -> StoreHealth {
        StoreHealth {
            history: self.inner.history.health(),
            state: self.inner.state.health(),
        }
    }

    /// Moves `history.db` aside as `history.db.corrupt-<timestamp>` and
    /// starts an empty one. Settings, the undo log and the license are
    /// untouched. Safe to call while other threads use the store: they wait
    /// for the reset, then see the fresh database.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] if the file cannot be renamed (for example another
    /// process has it open), or the open error of the fresh database.
    pub fn reset_history(&self) -> Result<ResetReport> {
        let moved_to = self.inner.history.reset(self.now())?;
        Ok(ResetReport { moved_to })
    }

    /// Like [`reset_history`](Self::reset_history) for `state.db`. This loses
    /// settings, the undo log and the license, so the UI should only offer it
    /// when `state.db` itself is reported corrupt.
    ///
    /// # Errors
    ///
    /// As for [`reset_history`](Self::reset_history).
    pub fn reset_state(&self) -> Result<ResetReport> {
        let moved_to = self.inner.state.reset(self.now())?;
        Ok(ResetReport { moved_to })
    }

    pub(crate) fn now(&self) -> Timestamp {
        self.inner.clock.now()
    }

    pub(crate) fn history(&self) -> &Db {
        &self.inner.history
    }

    pub(crate) fn state(&self) -> &Db {
        &self.inner.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_is_send_sync_clone() {
        fn assert_traits<T: Send + Sync + Clone + 'static>() {}
        assert_traits::<Store>();
    }

    #[test]
    fn bit_casts_round_trip() {
        for v in [0, 1, u64::MAX, 1 << 63, 0x1234_5678_9ABC_DEF0] {
            assert_eq!(i2u(u2i(v)), v);
        }
    }

    #[test]
    fn open_creates_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("nested").join("store")).unwrap();
        assert_eq!(
            store.health(),
            StoreHealth {
                history: DbHealth::Ok,
                state: DbHealth::Ok
            }
        );
        assert!(store.dir().join("history.db").exists());
        assert!(store.dir().join("state.db").exists());
        assert_eq!(store.inner.history.kind(), DbKind::History);
    }
}
