//! Input records: what the index knows about each file.

use std::path::PathBuf;
use std::sync::Arc;

use strata_core::{EntryFlags, FileRef, FileTime};

/// Identity of a volume, mirroring `strata_store::VolumeKey`.
///
/// The GUID path is reference-counted so a million candidates on one volume
/// share one string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VolumeKey {
    /// Volume serial number (64-bit on NTFS, 32-bit elsewhere).
    pub serial: u64,
    /// Volume GUID path, e.g. `\\?\Volume{...}\`.
    pub guid_path: Arc<str>,
}

impl VolumeKey {
    /// Builds a key.
    ///
    /// # Example
    ///
    /// ```
    /// let v = strata_dupes::VolumeKey::new(7, r"\\?\Volume{00000000-0000-0000-0000-000000000000}\");
    /// assert_eq!(v.serial, 7);
    /// ```
    #[must_use]
    pub fn new(serial: u64, guid_path: impl Into<Arc<str>>) -> Self {
        Self {
            serial,
            guid_path: guid_path.into(),
        }
    }
}

/// One file the index offers for duplicate detection.
///
/// The fields are hints from the scan; every one that matters is
/// re-read from a handle before a hash is trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Volume the file lives on.
    pub volume: VolumeKey,
    /// File reference from the scan (verified against the handle).
    pub file_ref: FileRef,
    /// Full Win32 path.
    pub path: PathBuf,
    /// Logical size of the unnamed data stream. When the source only knows
    /// the all-streams total (the index's `own_logical`), keep
    /// [`EntryFlags::HAS_ADS`] set: such candidates are re-measured from a
    /// handle before grouping, so alternate streams never split a group.
    pub size: u64,
    /// Last-write time from the scan (may be second precision).
    pub mtime: FileTime,
    /// Index flags (hardlink, cloud state, reparse kind, metadata, ...).
    pub flags: EntryFlags,
}
