//! Ports to the outside world.
//!
//! The crate never touches the OS. The elevated helper (or the app, over IPC)
//! implements these traits:
//! - [`JournalSource`]: `FSCTL_QUERY_USN_JOURNAL` and a blocking
//!   `FSCTL_READ_USN_JOURNAL`.
//! - [`RecordSource`]: fresh metadata for changed files (`read_record` on the
//!   raw MFT, or `OpenFileById` with read-attributes access).
//! - [`Clock`]: monotonic time, so tick logic is testable.

use std::time::{Duration, Instant};

use strata_core::{FileRef, ScanRecord};

/// State of an active USN journal (`USN_JOURNAL_DATA`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalInfo {
    /// Journal id; it changes when the journal is deleted and recreated.
    pub journal_id: u64,
    /// Oldest USN still readable.
    pub first_usn: i64,
    /// USN the next change will get.
    pub next_usn: i64,
    /// Records below this USN were purged.
    pub lowest_valid_usn: i64,
    /// Largest USN the journal can reach.
    pub max_usn: i64,
}

impl JournalInfo {
    /// The lowest USN a reader may still resume from.
    #[must_use]
    pub fn oldest_readable(&self) -> i64 {
        self.first_usn.max(self.lowest_valid_usn)
    }
}

/// Why a source call failed. Implementations map OS errors onto these.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    /// The journal is not active (`ERROR_JOURNAL_NOT_ACTIVE`).
    #[error("the USN journal is not active")]
    JournalInactive,
    /// The journal is being deleted or was recreated under a new id
    /// (`ERROR_JOURNAL_DELETE_IN_PROGRESS`, or an id mismatch).
    #[error("the USN journal was deleted or recreated")]
    JournalReset,
    /// The requested USN was purged (`ERROR_JOURNAL_ENTRY_DELETED`).
    #[error("the requested USN was purged from the journal")]
    UsnPurged,
    /// The volume was dismounted, locked or removed.
    #[error("the volume is no longer available")]
    VolumeGone,
    /// The helper or pipe went away.
    #[error("the source disconnected")]
    Disconnected,
    /// The call was cancelled because the owner is shutting down.
    #[error("the call was cancelled")]
    Cancelled,
    /// Any other failure.
    #[error("source I/O error: {0}")]
    Io(String),
}

/// Reads one volume's USN change journal.
pub trait JournalSource {
    /// Queries the journal. `Ok(None)` means the journal is not active.
    ///
    /// # Errors
    ///
    /// [`SourceError`] when the query itself fails.
    fn query(&mut self) -> Result<Option<JournalInfo>, SourceError>;

    /// Reads records starting at `from_usn`, exactly as `FSCTL_READ_USN_JOURNAL`
    /// returns them: an 8-byte next USN followed by packed
    /// `USN_RECORD_V2/V3/V4` records.
    ///
    /// Blocks until at least one record is available or `wait` elapses
    /// (`None`: no limit, until data arrives or the call is cancelled;
    /// `Some(Duration::ZERO)`: return immediately). Returning early with no
    /// records is allowed. `journal_id` is the id the caller expects; a
    /// different live id must fail with [`SourceError::JournalReset`].
    ///
    /// # Errors
    ///
    /// Any [`SourceError`].
    fn read(
        &mut self,
        journal_id: u64,
        from_usn: i64,
        wait: Option<Duration>,
    ) -> Result<Vec<u8>, SourceError>;
}

/// Fresh metadata for a batch of file references.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fetched {
    /// Merged records of files that exist. A record whose sequence differs
    /// from the requested reference (the record was reused) may be returned;
    /// the index replaces the stale predecessor.
    pub records: Vec<ScanRecord>,
    /// References that no longer exist or are stale.
    pub missing: Vec<FileRef>,
}

/// Re-reads file records after journal changes.
pub trait RecordSource {
    /// Returns fresh records for `refs`. A reference in neither
    /// [`Fetched::records`] nor [`Fetched::missing`] is treated as
    /// unavailable (for example access denied) and left unchanged.
    ///
    /// # Errors
    ///
    /// Any [`SourceError`]; the whole batch is considered unread.
    fn fetch(&mut self, refs: &[FileRef]) -> Result<Fetched, SourceError>;
}

/// Monotonic time source.
pub trait Clock {
    /// The current instant.
    fn now(&self) -> Instant;
}

/// [`Clock`] backed by [`Instant::now`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}
