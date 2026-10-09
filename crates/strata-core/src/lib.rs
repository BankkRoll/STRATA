//! Shared vocabulary for every Strata crate.
//!
//! This crate is the contract between the scanners (`strata-ntfs`,
//! `strata-walk`), the index (`strata-index`), the classifier, the cleaner, the
//! helper protocol and the UI backend. It has no Windows dependencies and no
//! I/O, so it builds and tests anywhere.
//!
//! Responsibilities:
//! - [`FileRef`]: stable identity of a file on a volume.
//! - [`WideName`]: lossless UTF-16 names (unpaired surrogates preserved).
//! - [`FileTime`] and the compact [`EpochSecs`] used by the index.
//! - [`ScanRecord`]: the one record type both scanners emit.
//! - [`EntryFlags`]: the packed per-entry flag word stored in the index.
//! - [`win32`]: Win32 attribute bits and reparse tags.
//! - [`known`]: resolved known folders as plain data (resolution lives in the
//!   Windows layer).
//! - [`Safety`], [`Category`], [`SizeMode`]: product-level enums.

#![forbid(unsafe_code)]

mod flags;
pub mod known;
mod name;
mod record;
mod tiers;
mod time;
pub mod win32;

pub use flags::{CloudState, EntryFlags, ReparseKind};
pub use name::WideName;
pub use record::{AdsInfo, NameLink, Reparse, ScanRecord, Sizes, Times};
pub use tiers::{Category, Safety, SizeMode};
pub use time::{EpochSecs, FileTime};

use serde::{Deserialize, Serialize};

/// Identity of a file on one volume.
///
/// On NTFS this is the 64-bit file reference: the low 48 bits are the MFT
/// record number and the high 16 bits are the record's sequence number. A
/// reference whose sequence does not match the live record is stale.
///
/// The fallback walker uses the 64-bit file index from
/// `GetFileInformationByHandle` where available, or a synthetic id with
/// [`FileRef::SYNTHETIC_BIT`] set when it never opened the file.
///
/// # Example
///
/// ```
/// use strata_core::FileRef;
/// let r = FileRef::from_parts(5, 5);
/// assert_eq!(r.record(), 5);
/// assert_eq!(r.sequence(), 5);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileRef(pub u64);

impl FileRef {
    /// Mask for the 48-bit record number.
    pub const RECORD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

    /// Marks ids invented by the fallback walker rather than read from the
    /// filesystem. Real NTFS references never have sequence 0x8000+ in
    /// practice, but walker ids must never be sent to the helper as file ids.
    pub const SYNTHETIC_BIT: u64 = 1 << 63;

    /// NTFS root directory record (`.`), always record 5.
    pub const NTFS_ROOT_RECORD: u64 = 5;

    /// Builds a reference from a record number and sequence number.
    ///
    /// Bits of `record` above 48 are discarded.
    #[must_use]
    pub const fn from_parts(record: u64, sequence: u16) -> Self {
        Self((record & Self::RECORD_MASK) | ((sequence as u64) << 48))
    }

    /// MFT record number (low 48 bits).
    #[must_use]
    pub const fn record(self) -> u64 {
        self.0 & Self::RECORD_MASK
    }

    /// Sequence number (high 16 bits).
    #[must_use]
    pub const fn sequence(self) -> u16 {
        (self.0 >> 48) as u16
    }

    /// Whether this id was synthesized by the walker (not a real file id).
    #[must_use]
    pub const fn is_synthetic(self) -> bool {
        self.0 & Self::SYNTHETIC_BIT != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_ref_round_trips_parts() {
        let r = FileRef::from_parts(0x1234_5678_9ABC, 0xBEEF);
        assert_eq!(r.record(), 0x1234_5678_9ABC);
        assert_eq!(r.sequence(), 0xBEEF);
    }

    #[test]
    fn file_ref_truncates_oversized_record() {
        let r = FileRef::from_parts(u64::MAX, 1);
        assert_eq!(r.record(), FileRef::RECORD_MASK);
        assert_eq!(r.sequence(), 1);
    }
}
