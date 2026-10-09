//! Index cache file and launch catch-up.
//!
//! # Policy
//!
//! - The cache stores the index together with the journal id and the last
//!   *applied* USN: the position at which every journal record before it had
//!   been fetched and applied. Records that were read but not yet applied are
//!   never covered by the saved position, so a crash replays them.
//! - Replay is idempotent. Every refresh re-reads the file's current state,
//!   and a delete names an exact reference whose sequence number is never
//!   reused, so applying a record twice cannot corrupt the index.
//! - The tailer saves when changes were applied, nothing is pending, and
//!   [`CachePolicy::interval`] passed since the last save; on a clean stop;
//!   and on a stale stop (dismount, disconnect), so a later launch catches
//!   up from there. It never saves after deciding a rescan is needed.
//! - Writes are atomic: the bytes go to `<path>.tmp`, are flushed to disk,
//!   and the temporary file is renamed over the cache. A crash leaves
//!   either the old or the new cache, never a torn one; a torn temporary
//!   file is overwritten by the next save.
//! - The index is serialized while the index lock is held and written after
//!   it is released, so file I/O never blocks readers.
//!
//! On launch, [`CacheFile::load`] reads the file, checks the header's volume
//! serial before decoding the rest, and the caller passes the index's
//! position to [`crate::Tailer::start`], which validates journal id and wrap
//! ([`crate::check_position`]) and replays to the head before reporting
//! [`crate::LiveStatus::Live`].

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use strata_index::{CacheError, Index, cache};

use crate::status::{JournalPosition, RescanReason};

/// When the tailer saves the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachePolicy {
    /// Minimum time between periodic saves while changes keep arriving.
    pub interval: Duration,
    /// Save when tailing stops cleanly or goes stale.
    pub save_on_stop: bool,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(300),
            save_on_stop: true,
        }
    }
}

/// Why a cache could not be used.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// There is no cache file.
    #[error("no cache file")]
    Missing,
    /// The file is corrupt, truncated or from another format version.
    #[error("cache unusable: {0}")]
    Unusable(#[from] CacheError),
    /// The cache was written for another volume.
    #[error("cache belongs to volume {found:#x}, expected {expected:#x}")]
    VolumeMismatch {
        /// Serial of the volume being opened.
        expected: u64,
        /// Serial stored in the cache.
        found: u64,
    },
}

impl LoadError {
    /// The rescan notice for this failure.
    #[must_use]
    pub fn rescan_reason(&self) -> RescanReason {
        match self {
            Self::VolumeMismatch { expected, found } => RescanReason::VolumeMismatch {
                expected: *expected,
                found: *found,
            },
            other => RescanReason::CacheUnusable(other.to_string()),
        }
    }
}

/// One volume's cache file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheFile {
    path: PathBuf,
}

impl CacheFile {
    /// A cache stored at `path` (the app picks a per-volume path in its data
    /// directory).
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The cache path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn tmp_path(&self) -> PathBuf {
        let mut s = self.path.as_os_str().to_owned();
        s.push(".tmp");
        PathBuf::from(s)
    }

    /// Atomically replaces the cache with `bytes` (from [`Index::to_bytes`]).
    ///
    /// # Errors
    ///
    /// Any I/O error; the previous cache is then left untouched.
    pub fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        let tmp = self.tmp_path();
        let result = (|| {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, &self.path)
        })();
        if result.is_err() {
            // Only the temporary file this call created is removed.
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Serializes `index` with its position set to `pos`, then writes it.
    ///
    /// # Errors
    ///
    /// Any I/O error.
    pub fn save(&self, index: &mut Index, pos: JournalPosition) -> std::io::Result<()> {
        index.set_usn_position(pos.journal_id, pos.usn);
        self.write(&index.to_bytes())
    }

    /// Loads the cache of the volume with serial `volume_serial`.
    ///
    /// # Errors
    ///
    /// [`LoadError`]; [`LoadError::rescan_reason`] gives the notice.
    pub fn load(&self, volume_serial: u64) -> Result<Index, LoadError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(LoadError::Missing),
            Err(e) => return Err(LoadError::Unusable(CacheError::Io(e))),
        };
        let header = cache::read_header(&bytes)?;
        if header.volume_serial != volume_serial {
            return Err(LoadError::VolumeMismatch {
                expected: volume_serial,
                found: header.volume_serial,
            });
        }
        Ok(Index::from_bytes(&bytes)?)
    }
}

/// The journal position stored in an index (from a cache or set after a
/// scan with [`Index::set_usn_position`]).
#[must_use]
pub fn index_position(index: &Index) -> JournalPosition {
    let v = index.volume();
    JournalPosition {
        journal_id: v.usn_journal_id,
        usn: v.last_usn,
    }
}
