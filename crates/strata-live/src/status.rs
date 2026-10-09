//! Typed outcomes of live tailing.

use std::fmt;

use crate::source::JournalInfo;

/// Where in which journal the index is up to date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalPosition {
    /// Journal id the position belongs to.
    pub journal_id: u64,
    /// Every change before this USN is reflected in the index.
    pub usn: i64,
}

/// Why the index must be rebuilt by a full scan. The app rescans
/// automatically and shows [`RescanReason`]'s `Display` text unobtrusively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RescanReason {
    /// The journal purged records the index had not applied yet.
    JournalWrapped {
        /// Position the index was at.
        saved_usn: i64,
        /// Oldest USN still in the journal (0 when unknown).
        first_usn: i64,
    },
    /// The journal was deleted and recreated.
    JournalIdChanged {
        /// Id the index was built against.
        expected: u64,
        /// Current id, when known.
        found: Option<u64>,
    },
    /// The saved position lies beyond the journal's end, so the journal
    /// was reset or the cache belongs to another volume state.
    UsnAhead {
        /// Position the index was at.
        saved_usn: i64,
        /// The journal's next USN.
        next_usn: i64,
    },
    /// The cache belongs to another volume.
    VolumeMismatch {
        /// Serial of the volume being opened.
        expected: u64,
        /// Serial stored in the cache.
        found: u64,
    },
    /// The cache file is missing, corrupt or from another format version.
    CacheUnusable(String),
    /// A journal buffer could not be decoded, so changes may be lost.
    MalformedJournal(String),
    /// The journal uses 128-bit file ids (ReFS) that the index cannot key.
    UnsupportedFileIds,
    /// The index refused an update (for example it is full).
    IndexRejected(String),
}

impl fmt::Display for RescanReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::JournalWrapped { .. } => {
                f.write_str("Rescanning: too many changes happened while Strata was not watching")
            }
            Self::JournalIdChanged { .. } | Self::UsnAhead { .. } => {
                f.write_str("Rescanning: the volume's change journal was reset")
            }
            Self::VolumeMismatch { .. } => {
                f.write_str("Rescanning: the saved index belongs to a different volume")
            }
            Self::CacheUnusable(_) => f.write_str("Rescanning: the saved index could not be read"),
            Self::MalformedJournal(_) => {
                f.write_str("Rescanning: the change journal returned unreadable data")
            }
            Self::UnsupportedFileIds => {
                f.write_str("Rescanning: this volume's change journal is not supported")
            }
            Self::IndexRejected(_) => f.write_str("Rescanning: the index rejected a change"),
        }
    }
}

/// Why tailing stopped with the index left as it was (shown as stale).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaleReason {
    /// The helper or pipe disconnected.
    Disconnected,
    /// The volume was dismounted, locked or removed.
    VolumeGone,
    /// Another source failure.
    Io(String),
}

/// Why [`crate::Tailer::run`] or [`crate::Tailer::step`] stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Halt {
    /// Shutdown was requested.
    Stopped,
    /// The journal is not active. The app shows "Live updates unavailable"
    /// and offers to enable it through the helper.
    JournalDisabled,
    /// Tailing stopped; the index is intact but stale. After a reconnect
    /// (or resume from sleep), [`crate::Tailer::resume`] catches up from the
    /// last applied position.
    Stale(StaleReason),
    /// The index can no longer be caught up; rescan.
    NeedsRescan(RescanReason),
}

/// Validates a saved position against the journal's current state.
///
/// # Errors
///
/// [`Halt::JournalDisabled`] when `info` is `None`, or
/// [`Halt::NeedsRescan`] for an id change, a wrap or a position past the end.
///
/// # Example
///
/// ```
/// use strata_live::{check_position, Halt, JournalInfo, JournalPosition, RescanReason};
/// let info = JournalInfo { journal_id: 7, first_usn: 4096, next_usn: 9000,
///     lowest_valid_usn: 0, max_usn: i64::MAX };
/// let pos = JournalPosition { journal_id: 7, usn: 100 };
/// assert!(matches!(
///     check_position(pos, Some(info)),
///     Err(Halt::NeedsRescan(RescanReason::JournalWrapped { .. }))
/// ));
/// ```
pub fn check_position(
    pos: JournalPosition,
    info: Option<JournalInfo>,
) -> Result<JournalInfo, Halt> {
    let Some(info) = info else {
        return Err(Halt::JournalDisabled);
    };
    if info.journal_id != pos.journal_id {
        return Err(Halt::NeedsRescan(RescanReason::JournalIdChanged {
            expected: pos.journal_id,
            found: Some(info.journal_id),
        }));
    }
    if pos.usn < info.oldest_readable() {
        return Err(Halt::NeedsRescan(RescanReason::JournalWrapped {
            saved_usn: pos.usn,
            first_usn: info.oldest_readable(),
        }));
    }
    if pos.usn > info.next_usn {
        return Err(Halt::NeedsRescan(RescanReason::UsnAhead {
            saved_usn: pos.usn,
            next_usn: info.next_usn,
        }));
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> JournalInfo {
        JournalInfo {
            journal_id: 7,
            first_usn: 4096,
            next_usn: 9000,
            lowest_valid_usn: 2048,
            max_usn: i64::MAX,
        }
    }

    #[test]
    fn positions() {
        let pos = |journal_id, usn| JournalPosition { journal_id, usn };
        assert_eq!(check_position(pos(7, 4096), Some(info())), Ok(info()));
        assert_eq!(check_position(pos(7, 9000), Some(info())), Ok(info()));
        assert_eq!(
            check_position(pos(7, 4096), None),
            Err(Halt::JournalDisabled)
        );
        assert_eq!(
            check_position(pos(8, 5000), Some(info())),
            Err(Halt::NeedsRescan(RescanReason::JournalIdChanged {
                expected: 8,
                found: Some(7)
            }))
        );
        assert_eq!(
            check_position(pos(7, 4095), Some(info())),
            Err(Halt::NeedsRescan(RescanReason::JournalWrapped {
                saved_usn: 4095,
                first_usn: 4096
            }))
        );
        assert_eq!(
            check_position(pos(7, 9001), Some(info())),
            Err(Halt::NeedsRescan(RescanReason::UsnAhead {
                saved_usn: 9001,
                next_usn: 9000
            }))
        );
    }

    #[test]
    fn notices_are_plain_text() {
        let r = RescanReason::JournalWrapped {
            saved_usn: 1,
            first_usn: 2,
        };
        assert!(r.to_string().starts_with("Rescanning"));
    }
}
