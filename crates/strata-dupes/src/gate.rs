//! Which files may take part, decided twice: once from the index flags
//! (before anything is opened) and again from the attributes of an open
//! handle (before any byte is read).

use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use strata_core::win32::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_PINNED,
    FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_UNPINNED,
};
use strata_core::{CloudState, EntryFlags, ReparseKind};

use crate::candidate::Candidate;

/// Why a candidate was left out before any file was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Exclusion {
    /// A directory.
    Directory,
    /// Zero bytes: every empty file is "identical", which is not useful.
    Empty,
    /// Smaller than the configured minimum.
    BelowMinSize,
    /// A secondary hardlink: the same file as its primary path.
    HardlinkSecondary,
    /// The same (volume, file reference) offered twice.
    SameFile,
    /// A cloud placeholder (any cloud state other than none): reading it
    /// could download it.
    CloudPlaceholder,
    /// Offline or recall-on-access attributes.
    Offline,
    /// A reparse point other than WOF compression or deduplication.
    ReparsePoint,
    /// NTFS metadata or a virtual node.
    Metadata,
    /// A synthetic walker id: identity cannot be verified on a handle.
    UnverifiableId,
    /// Inside `$Recycle.Bin`: already deleted, and never deletable again.
    RecycleBin,
}

/// Why a file was dropped after it was opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SkipReason {
    /// The handle shows cloud or offline attributes.
    Placeholder {
        /// Win32 attributes seen on the handle.
        attributes: u32,
    },
    /// The handle shows a reparse point that is not plain content.
    ReparsePoint {
        /// Reparse tag.
        tag: u32,
    },
    /// The path now names a directory.
    Directory,
    /// The path now names a different file than the scan saw.
    IdMismatch {
        /// File reference from the scan.
        expected: u64,
        /// File index read from the handle.
        found: u64,
    },
    /// Size or last-write time changed between two observations.
    Changed,
    /// The file no longer exists.
    NotFound,
    /// Access denied.
    AccessDenied,
    /// Opened exclusively by another process.
    SharingViolation,
    /// Any other I/O error.
    Io {
        /// OS error code, when there is one.
        code: Option<i32>,
        /// Message for the UI.
        message: String,
    },
}

impl SkipReason {
    pub(crate) fn from_io(e: &std::io::Error) -> Self {
        // ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION
        match (e.kind(), e.raw_os_error()) {
            (std::io::ErrorKind::NotFound, _) | (_, Some(2 | 3)) => Self::NotFound,
            (std::io::ErrorKind::PermissionDenied, _) | (_, Some(5)) => Self::AccessDenied,
            (_, Some(32 | 33)) => Self::SharingViolation,
            (_, code) => Self::Io {
                code,
                message: e.to_string(),
            },
        }
    }
}

/// Settings the gates read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GateConfig {
    pub min_size: u64,
    pub allow_wof_and_dedup: bool,
}

/// Index-level gate. Runs before anything is opened.
pub(crate) fn check_candidate(c: &Candidate, cfg: GateConfig) -> Result<(), Exclusion> {
    let f = c.flags;
    if f.contains(EntryFlags::DIR) {
        return Err(Exclusion::Directory);
    }
    if f.contains(EntryFlags::NTFS_METADATA) || f.contains(EntryFlags::VIRTUAL) {
        return Err(Exclusion::Metadata);
    }
    if f.cloud() != CloudState::None {
        return Err(Exclusion::CloudPlaceholder);
    }
    if f.contains(EntryFlags::OFFLINE) {
        return Err(Exclusion::Offline);
    }
    if !reparse_kind_allowed(f.reparse(), cfg.allow_wof_and_dedup) {
        return Err(Exclusion::ReparsePoint);
    }
    if f.contains(EntryFlags::HARDLINK_SECONDARY) {
        return Err(Exclusion::HardlinkSecondary);
    }
    if c.file_ref.is_synthetic() {
        return Err(Exclusion::UnverifiableId);
    }
    if c.size == 0 {
        return Err(Exclusion::Empty);
    }
    if c.size < cfg.min_size {
        return Err(Exclusion::BelowMinSize);
    }
    if in_recycle_bin(&c.path) {
        return Err(Exclusion::RecycleBin);
    }
    Ok(())
}

/// Size gate on a measured size (after the handle replaced the hint).
pub(crate) fn check_size(size: u64, cfg: GateConfig) -> Result<(), Exclusion> {
    if size == 0 {
        Err(Exclusion::Empty)
    } else if size < cfg.min_size {
        Err(Exclusion::BelowMinSize)
    } else {
        Ok(())
    }
}

fn reparse_kind_allowed(kind: ReparseKind, allow_wof_and_dedup: bool) -> bool {
    match kind {
        ReparseKind::None => true,
        // Both are ordinary local files to a reader: the filter decompresses
        // or reassembles the data from this volume, never from the network.
        ReparseKind::Wof | ReparseKind::Dedup => allow_wof_and_dedup,
        _ => false,
    }
}

/// Every attribute bit that marks cloud or offline content. Pinned and
/// unpinned are cloud-only bits, so a hydrated placeholder is refused too:
/// the provider may dehydrate it at any moment.
pub const PLACEHOLDER_ATTRIBUTES: u32 = FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
    | FILE_ATTRIBUTE_RECALL_ON_OPEN
    | FILE_ATTRIBUTE_OFFLINE
    | FILE_ATTRIBUTE_PINNED
    | FILE_ATTRIBUTE_UNPINNED;

/// Handle-level gate on the attributes and reparse tag of an open handle.
///
/// Pure, so the placeholder logic is tested with injected values.
///
/// # Errors
///
/// The [`SkipReason`] that rules the file out.
///
/// # Example
///
/// ```
/// use strata_core::win32::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
/// assert!(strata_dupes::check_handle_attributes(0x20, 0, true).is_ok());
/// assert!(strata_dupes::check_handle_attributes(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, 0, true).is_err());
/// ```
pub fn check_handle_attributes(
    attributes: u32,
    reparse_tag: u32,
    allow_wof_and_dedup: bool,
) -> Result<(), SkipReason> {
    if attributes & PLACEHOLDER_ATTRIBUTES != 0 {
        return Err(SkipReason::Placeholder { attributes });
    }
    if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(SkipReason::Directory);
    }
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        let kind = ReparseKind::from_tag(reparse_tag);
        if kind == ReparseKind::Cloud {
            return Err(SkipReason::Placeholder { attributes });
        }
        if !reparse_kind_allowed(kind, allow_wof_and_dedup) {
            return Err(SkipReason::ReparsePoint { tag: reparse_tag });
        }
    }
    Ok(())
}

fn in_recycle_bin(path: &Path) -> bool {
    path.components().any(|c| match c {
        Component::Normal(n) => n.to_str().is_some_and(|s| {
            s.eq_ignore_ascii_case("$Recycle.Bin")
                || s.eq_ignore_ascii_case("RECYCLER")
                || s.eq_ignore_ascii_case("RECYCLED")
        }),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::VolumeKey;
    use strata_core::win32::*;
    use strata_core::{FileRef, FileTime};

    const CFG: GateConfig = GateConfig {
        min_size: 1 << 20,
        allow_wof_and_dedup: true,
    };

    fn cand(size: u64, flags: EntryFlags) -> Candidate {
        Candidate {
            volume: VolumeKey::new(1, "v"),
            file_ref: FileRef::from_parts(100, 1),
            path: r"D:\data\a.bin".into(),
            size,
            mtime: FileTime(1),
            flags,
        }
    }

    #[test]
    fn candidate_gate_table() {
        let mb = 1 << 20;
        let cases: &[(Candidate, Result<(), Exclusion>)] = &[
            (cand(mb, EntryFlags::EMPTY), Ok(())),
            (cand(0, EntryFlags::EMPTY), Err(Exclusion::Empty)),
            (
                cand(mb - 1, EntryFlags::EMPTY),
                Err(Exclusion::BelowMinSize),
            ),
            (cand(mb, EntryFlags::DIR), Err(Exclusion::Directory)),
            (
                cand(mb, EntryFlags::HARDLINK_SECONDARY),
                Err(Exclusion::HardlinkSecondary),
            ),
            (
                cand(mb, EntryFlags::NTFS_METADATA),
                Err(Exclusion::Metadata),
            ),
            (cand(mb, EntryFlags::VIRTUAL), Err(Exclusion::Metadata)),
            (cand(mb, EntryFlags::OFFLINE), Err(Exclusion::Offline)),
            (
                cand(mb, EntryFlags::EMPTY.with_cloud(CloudState::OnlineOnly)),
                Err(Exclusion::CloudPlaceholder),
            ),
            (
                cand(
                    mb,
                    EntryFlags::EMPTY.with_cloud(CloudState::LocallyAvailable),
                ),
                Err(Exclusion::CloudPlaceholder),
            ),
            (
                cand(mb, EntryFlags::EMPTY.with_cloud(CloudState::AlwaysKeep)),
                Err(Exclusion::CloudPlaceholder),
            ),
            (
                cand(mb, EntryFlags::EMPTY.with_reparse(ReparseKind::Symlink)),
                Err(Exclusion::ReparsePoint),
            ),
            (
                cand(mb, EntryFlags::EMPTY.with_reparse(ReparseKind::Cloud)),
                Err(Exclusion::ReparsePoint),
            ),
            (
                cand(mb, EntryFlags::EMPTY.with_reparse(ReparseKind::Wof)),
                Ok(()),
            ),
        ];
        for (c, want) in cases {
            assert_eq!(check_candidate(c, CFG), *want, "{:?}", c.flags);
        }
        let no_wof = GateConfig {
            allow_wof_and_dedup: false,
            ..CFG
        };
        assert_eq!(
            check_candidate(
                &cand(mb, EntryFlags::EMPTY.with_reparse(ReparseKind::Dedup)),
                no_wof
            ),
            Err(Exclusion::ReparsePoint)
        );
        let mut synthetic = cand(mb, EntryFlags::EMPTY);
        synthetic.file_ref = FileRef(FileRef::SYNTHETIC_BIT | 5);
        assert_eq!(
            check_candidate(&synthetic, CFG),
            Err(Exclusion::UnverifiableId)
        );
        let mut bin = cand(mb, EntryFlags::EMPTY);
        bin.path = r"D:\$RECYCLE.BIN\S-1-5-21-1-2-3-1001\$R1.bin".into();
        assert_eq!(check_candidate(&bin, CFG), Err(Exclusion::RecycleBin));
    }

    #[test]
    fn handle_gate_refuses_every_placeholder_signal() {
        for bit in [
            FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
            FILE_ATTRIBUTE_RECALL_ON_OPEN,
            FILE_ATTRIBUTE_OFFLINE,
            FILE_ATTRIBUTE_PINNED,
            FILE_ATTRIBUTE_UNPINNED,
        ] {
            assert!(matches!(
                check_handle_attributes(FILE_ATTRIBUTE_ARCHIVE | bit, 0, true),
                Err(SkipReason::Placeholder { .. })
            ));
        }
        // A hydrated placeholder: reparse tag only, no recall bits.
        for sub in 0..16u32 {
            assert!(matches!(
                check_handle_attributes(
                    FILE_ATTRIBUTE_REPARSE_POINT,
                    0x9000_001A | (sub << 12),
                    true
                ),
                Err(SkipReason::Placeholder { .. })
            ));
        }
        assert!(matches!(
            check_handle_attributes(FILE_ATTRIBUTE_REPARSE_POINT, IO_REPARSE_TAG_SYMLINK, true),
            Err(SkipReason::ReparsePoint { .. })
        ));
        assert!(
            check_handle_attributes(FILE_ATTRIBUTE_REPARSE_POINT, IO_REPARSE_TAG_WOF, true).is_ok()
        );
        assert!(
            check_handle_attributes(FILE_ATTRIBUTE_REPARSE_POINT, IO_REPARSE_TAG_WOF, false)
                .is_err()
        );
        assert_eq!(
            check_handle_attributes(FILE_ATTRIBUTE_DIRECTORY, 0, true),
            Err(SkipReason::Directory)
        );
        assert!(
            check_handle_attributes(FILE_ATTRIBUTE_ARCHIVE | FILE_ATTRIBUTE_READONLY, 0, true)
                .is_ok()
        );
    }

    #[test]
    fn io_errors_map_to_reasons() {
        let e = |c| std::io::Error::from_raw_os_error(c);
        assert_eq!(SkipReason::from_io(&e(2)), SkipReason::NotFound);
        assert_eq!(SkipReason::from_io(&e(5)), SkipReason::AccessDenied);
        assert_eq!(SkipReason::from_io(&e(32)), SkipReason::SharingViolation);
        assert!(matches!(
            SkipReason::from_io(&e(1117)),
            SkipReason::Io {
                code: Some(1117),
                ..
            }
        ));
    }
}
