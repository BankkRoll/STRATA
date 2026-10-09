use serde::{Deserialize, Serialize};

use crate::{EntryFlags, FileRef, FileTime, WideName};

/// One `(parent, name)` link of a file. A file with N hardlinks has N links.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameLink {
    /// Parent directory reference (including its expected sequence number).
    pub parent: FileRef,
    /// Name within the parent. Never a DOS 8.3 alias.
    pub name: WideName,
}

/// Timestamps from `$STANDARD_INFORMATION` (or find data on the walker).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Times {
    /// Creation time.
    pub created: FileTime,
    /// Last content modification.
    pub modified: FileTime,
    /// Last access. Unreliable: Windows may disable or coarsen updates.
    pub accessed: FileTime,
    /// Last MFT record change (NTFS only; 0 when unknown).
    pub changed: FileTime,
}

/// Size accounting for one file record. See SPEC §7.
///
/// The record's contribution to "allocated" totals is
/// `allocated + ads_allocated + dir_overhead + attr_overhead`; to "logical"
/// totals it is `logical + ads_logical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Sizes {
    /// Logical size of the unnamed data stream (Explorer's "Size").
    pub logical: u64,
    /// Bytes on disk for the unnamed stream, honouring compression, sparse
    /// runs, WOF (bytes of `WofCompressedData`) and resident data (0).
    pub allocated: u64,
    /// Sum of logical sizes of named streams. Excludes `WofCompressedData`,
    /// whose bytes are already reported in `allocated`.
    pub ads_logical: u64,
    /// Sum of allocated sizes of named streams (same exclusion).
    pub ads_allocated: u64,
    /// Directory B-tree (`$INDEX_ALLOCATION`) bytes; 0 for files.
    pub dir_overhead: u64,
    /// On-disk bytes of non-resident attributes that are neither content nor
    /// directory indexes: `$ATTRIBUTE_LIST`, `$BITMAP`, `$EA`,
    /// `$LOGGED_UTILITY_STREAM`, non-resident `$REPARSE_POINT`. The walker
    /// cannot see these and reports 0.
    #[serde(default)]
    pub attr_overhead: u64,
}

impl Sizes {
    /// Total on-disk cost of this record.
    #[must_use]
    pub const fn total_allocated(&self) -> u64 {
        self.allocated
            .saturating_add(self.ads_allocated)
            .saturating_add(self.dir_overhead)
            .saturating_add(self.attr_overhead)
    }

    /// Total logical size of this record (all streams).
    #[must_use]
    pub const fn total_logical(&self) -> u64 {
        self.logical.saturating_add(self.ads_logical)
    }
}

/// Reparse point details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reparse {
    /// Raw reparse tag.
    pub tag: u32,
    /// Display target for symlinks/junctions/mount points (print name when
    /// present, else substitute name). `None` for tags without a target.
    pub target: Option<WideName>,
}

/// One alternate data stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdsInfo {
    /// Stream name (without the leading `:`).
    pub name: WideName,
    /// Logical size.
    pub logical: u64,
    /// Bytes on disk.
    pub allocated: u64,
}

/// A fully merged file record, as emitted by any scanner.
///
/// Both the MFT scanner and the fallback walker produce these; the index
/// consumes them without caring which scanner ran. Scanner-specific gaps are
/// expressed through flags (e.g. [`EntryFlags::ALLOC_ESTIMATED`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanRecord {
    /// This file's identity.
    pub id: FileRef,
    /// All non-DOS names. Empty only for records that have no `$FILE_NAME`
    /// (corrupt); the index attaches those under "Orphaned entries".
    /// The root directory links to itself.
    pub links: Vec<NameLink>,
    /// Raw Win32 attribute bits (`FILE_ATTRIBUTE_*`).
    pub attributes: u32,
    /// Flags derived by the scanner (directory, reparse kind, cloud state,
    /// metadata, ADS present, ...).
    pub flags: EntryFlags,
    /// Authoritative timestamps.
    pub times: Times,
    /// `$FILE_NAME` creation time of the first link, for forensics display.
    pub fn_created: Option<FileTime>,
    /// Size accounting.
    pub sizes: Sizes,
    /// Reparse point, if any.
    pub reparse: Option<Reparse>,
    /// Alternate data streams (excluding `WofCompressedData`).
    pub ads: Vec<AdsInfo>,
}

impl ScanRecord {
    /// Whether this record is a directory.
    #[must_use]
    pub const fn is_dir(&self) -> bool {
        self.flags.contains(EntryFlags::DIR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_include_streams_and_overhead() {
        let s = Sizes {
            logical: 10,
            allocated: 4096,
            ads_logical: 5,
            ads_allocated: 0,
            dir_overhead: 8192,
            attr_overhead: 1024,
        };
        assert_eq!(s.total_allocated(), 4096 + 8192 + 1024);
        assert_eq!(s.total_logical(), 15);
    }

    #[test]
    fn totals_saturate() {
        let s = Sizes {
            logical: u64::MAX,
            allocated: u64::MAX,
            ads_logical: 1,
            ads_allocated: 1,
            dir_overhead: 1,
            attr_overhead: 1,
        };
        assert_eq!(s.total_allocated(), u64::MAX);
        assert_eq!(s.total_logical(), u64::MAX);
    }
}
