use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::ListingMethod;

/// Category of a non-fatal error met during a walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// Permission denied (directory listing or per-file open).
    AccessDenied,
    /// The entry disappeared between being listed and being opened.
    Vanished,
    /// Another process holds the file with an incompatible share mode
    /// (e.g. `pagefile.sys`).
    SharingViolation,
    /// A directory was replaced by a file between listing and descent.
    ReplacedByFile,
    /// A network request exceeded the timeout or was abandoned on cancel.
    Timeout,
    /// The network path became unreachable.
    Network,
    /// Anything else.
    Other,
}

impl ErrorKind {
    const ALL: [Self; 7] = [
        Self::AccessDenied,
        Self::Vanished,
        Self::SharingViolation,
        Self::ReplacedByFile,
        Self::Timeout,
        Self::Network,
        Self::Other,
    ];

    /// Maps an OS error to a category.
    #[must_use]
    pub fn classify(e: &io::Error) -> Self {
        match e.raw_os_error() {
            // ERROR_ACCESS_DENIED, ERROR_PRIVILEGE_NOT_HELD, ERROR_CANT_ACCESS_FILE
            Some(5 | 1314 | 1920) => Self::AccessDenied,
            // FILE_NOT_FOUND, PATH_NOT_FOUND, INVALID_NAME (parent renamed),
            // DELETE_PENDING, NOT_FOUND
            Some(2 | 3 | 123 | 303 | 1168) => Self::Vanished,
            // SHARING_VIOLATION, LOCK_VIOLATION
            Some(32 | 33) => Self::SharingViolation,
            // ERROR_DIRECTORY ("the directory name is invalid")
            Some(267) => Self::ReplacedByFile,
            // SEM_TIMEOUT, OPERATION_ABORTED (CancelSynchronousIo), TIMEOUT
            Some(121 | 995 | 1460) => Self::Timeout,
            // BAD_NETPATH, NETNAME_DELETED, UNEXP_NET_ERR, BAD_NET_NAME,
            // NO_NETWORK, NETWORK_UNREACHABLE, CONNECTION_ABORTED, NOT_CONNECTED
            Some(51 | 53 | 59 | 64 | 67 | 1222 | 1231 | 1236 | 2250) => Self::Network,
            _ => Self::Other,
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// Error counts by [`ErrorKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ErrorCounts {
    counts: [u64; 7],
}

impl ErrorCounts {
    /// Count for one kind.
    #[must_use]
    pub const fn get(&self, kind: ErrorKind) -> u64 {
        self.counts[kind.index()]
    }

    /// Sum over all kinds.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.counts.iter().sum()
    }

    /// Non-zero `(kind, count)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (ErrorKind, u64)> + '_ {
        ErrorKind::ALL
            .iter()
            .map(|&k| (k, self.get(k)))
            .filter(|&(_, n)| n > 0)
    }
}

/// Thread-safe accumulator behind [`ErrorCounts`].
#[derive(Debug, Default)]
pub(crate) struct AtomicErrorCounts([AtomicU64; 7]);

impl AtomicErrorCounts {
    pub(crate) fn add(&self, kind: ErrorKind) {
        self.0[kind.index()].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> ErrorCounts {
        ErrorCounts {
            counts: std::array::from_fn(|i| self.0[i].load(Ordering::Relaxed)),
        }
    }
}

/// Running totals passed to [`crate::WalkSink::progress`].
///
/// Counts are of records delivered to the sink so far; a merged hardlink
/// counts once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    /// Directory records delivered.
    pub dirs: u64,
    /// File records delivered.
    pub files: u64,
    /// Sum of logical sizes (unnamed + named streams).
    pub logical_bytes: u64,
    /// Sum of allocated sizes (unnamed + named streams + directory overhead).
    pub allocated_bytes: u64,
    /// Non-fatal errors so far.
    pub errors: u64,
    /// Time since the walk started.
    pub elapsed: Duration,
}

/// Facts about the volume holding the walk root, for reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VolumeStats {
    /// Volume mount path (e.g. `C:\` or `\\server\share\`).
    pub mount: String,
    /// Filesystem name (`NTFS`, `ReFS`, `exFAT`, ...).
    pub filesystem: String,
    /// Cluster size in bytes, used for allocation estimates.
    pub cluster_size: u64,
    /// Total bytes (`GetDiskFreeSpaceExW`).
    pub total_bytes: u64,
    /// Free bytes (`GetDiskFreeSpaceExW`).
    pub free_bytes: u64,
    /// Whether the volume is remote.
    pub is_network: bool,
}

impl VolumeStats {
    /// `total - free`.
    #[must_use]
    pub const fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.free_bytes)
    }
}

/// Summary of a finished (or cancelled) walk.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WalkStats {
    /// Final progress counters.
    pub totals: Progress,
    /// Listing method used. [`ListingMethod::DirectoryInfo`] may have fallen
    /// back to an older information class; see `dir_info_fallback`.
    pub listing: ListingMethod,
    /// Set when `FileIdExtdDirectoryInfo` was unsupported and an older class
    /// was used.
    pub dir_info_fallback: bool,
    /// Directories that could not be listed for lack of permission.
    pub access_denied_dirs: u64,
    /// Directories flagged `PARTIAL` (cancelled, timed out, or unreadable).
    pub partial_dirs: u64,
    /// Records whose allocation is still an estimate.
    pub estimated_allocations: u64,
    /// Extra hardlink names merged into an existing record.
    pub hardlinks_merged: u64,
    /// Non-fatal errors by kind.
    pub errors: ErrorCounts,
    /// Whether the walk was cancelled.
    pub cancelled: bool,
    /// Whether any part of the tree is missing (cancelled, partial or denied
    /// directories). The totals are then a lower bound.
    pub partial: bool,
    /// Volume facts, when they could be read.
    pub volume: Option<VolumeStats>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_of_common_codes() {
        let k = |c| ErrorKind::classify(&io::Error::from_raw_os_error(c));
        assert_eq!(k(5), ErrorKind::AccessDenied);
        assert_eq!(k(3), ErrorKind::Vanished);
        assert_eq!(k(32), ErrorKind::SharingViolation);
        assert_eq!(k(267), ErrorKind::ReplacedByFile);
        assert_eq!(k(995), ErrorKind::Timeout);
        assert_eq!(k(53), ErrorKind::Network);
        assert_eq!(k(87), ErrorKind::Other);
    }

    #[test]
    fn counts_round_trip() {
        let a = AtomicErrorCounts::default();
        a.add(ErrorKind::Timeout);
        a.add(ErrorKind::Timeout);
        a.add(ErrorKind::Other);
        let s = a.snapshot();
        assert_eq!(s.get(ErrorKind::Timeout), 2);
        assert_eq!(s.total(), 3);
        assert_eq!(s.iter().count(), 2);
    }
}
