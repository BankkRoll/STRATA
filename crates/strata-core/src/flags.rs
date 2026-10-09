use serde::{Deserialize, Serialize};

use crate::win32;

/// Kind of reparse point, derived from the reparse tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum ReparseKind {
    /// Not a reparse point.
    None = 0,
    /// Symbolic link. Never traversed.
    Symlink = 1,
    /// Junction or volume mount point. Never traversed.
    MountPoint = 2,
    /// WOF-compressed file; real bytes live in `WofCompressedData`.
    Wof = 3,
    /// Cloud-files placeholder (OneDrive etc.).
    Cloud = 4,
    /// Data deduplication; allocated size unreliable.
    Dedup = 5,
    /// App execution alias (~0 bytes).
    AppExecLink = 6,
    /// WSL symlink / socket / FIFO / device.
    Wsl = 7,
    /// Any other tag; raw tag kept in the scan record.
    Unknown = 8,
}

impl ReparseKind {
    /// Classifies a raw reparse tag.
    #[must_use]
    pub const fn from_tag(tag: u32) -> Self {
        match tag {
            win32::IO_REPARSE_TAG_SYMLINK => Self::Symlink,
            win32::IO_REPARSE_TAG_MOUNT_POINT => Self::MountPoint,
            win32::IO_REPARSE_TAG_WOF => Self::Wof,
            win32::IO_REPARSE_TAG_DEDUP => Self::Dedup,
            win32::IO_REPARSE_TAG_APPEXECLINK => Self::AppExecLink,
            win32::IO_REPARSE_TAG_LX_SYMLINK
            | win32::IO_REPARSE_TAG_AF_UNIX
            | win32::IO_REPARSE_TAG_LX_FIFO
            | win32::IO_REPARSE_TAG_LX_CHR
            | win32::IO_REPARSE_TAG_LX_BLK => Self::Wsl,
            t if win32::is_cloud_tag(t) => Self::Cloud,
            _ => Self::Unknown,
        }
    }

    /// Whether a directory with this reparse kind must not be descended into.
    #[must_use]
    pub const fn blocks_traversal(self) -> bool {
        !matches!(self, Self::None | Self::Wof | Self::Cloud | Self::Dedup)
    }

    const fn from_bits(bits: u32) -> Self {
        match bits {
            1 => Self::Symlink,
            2 => Self::MountPoint,
            3 => Self::Wof,
            4 => Self::Cloud,
            5 => Self::Dedup,
            6 => Self::AppExecLink,
            7 => Self::Wsl,
            8 => Self::Unknown,
            _ => Self::None,
        }
    }
}

/// Local availability of a cloud-files placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum CloudState {
    /// Not a cloud placeholder.
    None = 0,
    /// Online-only: content is not on disk; reading would download it.
    OnlineOnly = 1,
    /// Locally available (hydrated) but may be freed by the provider.
    LocallyAvailable = 2,
    /// Pinned: "Always keep on this device".
    AlwaysKeep = 3,
}

impl CloudState {
    /// Derives the state from a cloud placeholder's Win32 attributes.
    ///
    /// Pinned wins; then any recall/offline bit means online-only; otherwise
    /// the content is local.
    #[must_use]
    pub const fn from_attributes(attrs: u32) -> Self {
        if attrs & win32::FILE_ATTRIBUTE_PINNED != 0 {
            Self::AlwaysKeep
        } else if attrs
            & (win32::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
                | win32::FILE_ATTRIBUTE_RECALL_ON_OPEN
                | win32::FILE_ATTRIBUTE_OFFLINE)
            != 0
        {
            Self::OnlineOnly
        } else {
            Self::LocallyAvailable
        }
    }

    const fn from_bits(bits: u32) -> Self {
        match bits {
            1 => Self::OnlineOnly,
            2 => Self::LocallyAvailable,
            3 => Self::AlwaysKeep,
            _ => Self::None,
        }
    }
}

/// Packed per-entry flag word, stored as one `u32` per entry in the index.
///
/// Layout: bits 0..20 are boolean flags (constants below), bits 20..24 hold
/// the [`ReparseKind`], bits 24..26 hold the [`CloudState`]. Bits 26..32 are
/// reserved and must be zero.
///
/// # Example
///
/// ```
/// use strata_core::{EntryFlags, ReparseKind};
/// let f = EntryFlags::DIR.with_reparse(ReparseKind::MountPoint);
/// assert!(f.contains(EntryFlags::DIR));
/// assert_eq!(f.reparse(), ReparseKind::MountPoint);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryFlags(pub u32);

impl EntryFlags {
    /// Directory.
    pub const DIR: Self = Self(1 << 0);
    /// Win32 hidden attribute.
    pub const HIDDEN: Self = Self(1 << 1);
    /// Win32 system attribute.
    pub const SYSTEM: Self = Self(1 << 2);
    /// Win32 readonly attribute.
    pub const READONLY: Self = Self(1 << 3);
    /// NTFS (LZNT1) compressed.
    pub const COMPRESSED: Self = Self(1 << 4);
    /// Sparse file.
    pub const SPARSE: Self = Self(1 << 5);
    /// EFS encrypted.
    pub const ENCRYPTED: Self = Self(1 << 6);
    /// Has one or more alternate data streams.
    pub const HAS_ADS: Self = Self(1 << 7);
    /// Secondary hardlink: data counted at another path, contributes 0 bytes.
    pub const HARDLINK_SECONDARY: Self = Self(1 << 8);
    /// Parent missing or stale; attached under "Orphaned entries".
    pub const ORPHAN: Self = Self(1 << 9);
    /// Directory could not be listed (walker); contents unknown.
    pub const ACCESS_DENIED: Self = Self(1 << 10);
    /// Queued for deletion / deleted, awaiting confirmation from live updates.
    pub const DELETE_PENDING: Self = Self(1 << 11);
    /// NTFS metadata file (`$MFT`, `$LogFile`, ...).
    pub const NTFS_METADATA: Self = Self(1 << 12);
    /// Virtual node synthesized by Strata (not on disk).
    pub const VIRTUAL: Self = Self(1 << 13);
    /// Some timestamp is implausible (pre-1990 or in the future).
    pub const SUSPICIOUS_TIME: Self = Self(1 << 14);
    /// Win32 temporary attribute.
    pub const TEMPORARY: Self = Self(1 << 15);
    /// Win32 offline attribute.
    pub const OFFLINE: Self = Self(1 << 16);
    /// Subtree totals are incomplete (cancelled or partial scan).
    pub const PARTIAL: Self = Self(1 << 17);
    /// Entry was part of a parent-chain cycle that was broken.
    pub const CYCLE_BROKEN: Self = Self(1 << 18);
    /// Allocated size is an estimate (walker before the allocation pass).
    pub const ALLOC_ESTIMATED: Self = Self(1 << 19);

    const REPARSE_SHIFT: u32 = 20;
    const REPARSE_MASK: u32 = 0xF << Self::REPARSE_SHIFT;
    const CLOUD_SHIFT: u32 = 24;
    const CLOUD_MASK: u32 = 0x3 << Self::CLOUD_SHIFT;

    /// No flags.
    pub const EMPTY: Self = Self(0);

    /// Whether every bit of `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns `self` with `other`'s bits set.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Sets or clears `other`'s bits in place.
    pub fn set(&mut self, other: Self, on: bool) {
        if on {
            self.0 |= other.0;
        } else {
            self.0 &= !other.0;
        }
    }

    /// Reparse kind stored in the packed field.
    #[must_use]
    pub const fn reparse(self) -> ReparseKind {
        ReparseKind::from_bits((self.0 & Self::REPARSE_MASK) >> Self::REPARSE_SHIFT)
    }

    /// Returns `self` with the reparse field replaced.
    #[must_use]
    pub const fn with_reparse(self, kind: ReparseKind) -> Self {
        Self((self.0 & !Self::REPARSE_MASK) | ((kind as u32) << Self::REPARSE_SHIFT))
    }

    /// Cloud state stored in the packed field.
    #[must_use]
    pub const fn cloud(self) -> CloudState {
        CloudState::from_bits((self.0 & Self::CLOUD_MASK) >> Self::CLOUD_SHIFT)
    }

    /// Returns `self` with the cloud field replaced.
    #[must_use]
    pub const fn with_cloud(self, state: CloudState) -> Self {
        Self((self.0 & !Self::CLOUD_MASK) | ((state as u32) << Self::CLOUD_SHIFT))
    }

    /// Maps Win32 attribute bits onto the boolean flags they correspond to.
    ///
    /// Reparse and cloud fields are not derived here because they need the
    /// reparse tag, not just the attribute bit.
    #[must_use]
    pub const fn from_win32_attributes(attrs: u32) -> Self {
        let mut bits = 0;
        let map: [(u32, Self); 8] = [
            (crate::win32::FILE_ATTRIBUTE_DIRECTORY, Self::DIR),
            (crate::win32::FILE_ATTRIBUTE_HIDDEN, Self::HIDDEN),
            (crate::win32::FILE_ATTRIBUTE_SYSTEM, Self::SYSTEM),
            (crate::win32::FILE_ATTRIBUTE_READONLY, Self::READONLY),
            (crate::win32::FILE_ATTRIBUTE_COMPRESSED, Self::COMPRESSED),
            (crate::win32::FILE_ATTRIBUTE_SPARSE_FILE, Self::SPARSE),
            (crate::win32::FILE_ATTRIBUTE_ENCRYPTED, Self::ENCRYPTED),
            (crate::win32::FILE_ATTRIBUTE_TEMPORARY, Self::TEMPORARY),
        ];
        let mut i = 0;
        while i < map.len() {
            if attrs & map[i].0 != 0 {
                bits |= map[i].1.0;
            }
            i += 1;
        }
        if attrs & crate::win32::FILE_ATTRIBUTE_OFFLINE != 0 {
            bits |= Self::OFFLINE.0;
        }
        Self(bits)
    }
}

impl std::ops::BitOr for EntryFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl std::ops::BitOrAssign for EntryFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_fields_do_not_clobber_booleans() {
        let all_bools = ALL_BOOLS;
        let f = all_bools
            .with_reparse(ReparseKind::Unknown)
            .with_cloud(CloudState::AlwaysKeep);
        assert_eq!(f.reparse(), ReparseKind::Unknown);
        assert_eq!(f.cloud(), CloudState::AlwaysKeep);
        assert!(f.contains(all_bools));
        let cleared = f
            .with_reparse(ReparseKind::None)
            .with_cloud(CloudState::None);
        assert_eq!(cleared, all_bools);
    }

    const ALL_BOOLS: EntryFlags = EntryFlags((1 << 20) - 1);

    #[test]
    fn reparse_tag_classification() {
        use crate::win32::*;
        assert_eq!(
            ReparseKind::from_tag(IO_REPARSE_TAG_SYMLINK),
            ReparseKind::Symlink
        );
        assert_eq!(
            ReparseKind::from_tag(IO_REPARSE_TAG_MOUNT_POINT),
            ReparseKind::MountPoint
        );
        assert_eq!(ReparseKind::from_tag(IO_REPARSE_TAG_WOF), ReparseKind::Wof);
        assert_eq!(ReparseKind::from_tag(0x9000_601A), ReparseKind::Cloud);
        assert_eq!(
            ReparseKind::from_tag(IO_REPARSE_TAG_LX_SYMLINK),
            ReparseKind::Wsl
        );
        assert_eq!(ReparseKind::from_tag(0x1234_5678), ReparseKind::Unknown);
        assert!(ReparseKind::Symlink.blocks_traversal());
        assert!(ReparseKind::MountPoint.blocks_traversal());
        assert!(!ReparseKind::Cloud.blocks_traversal());
    }

    #[test]
    fn cloud_state_from_attributes() {
        use crate::win32::*;
        assert_eq!(
            CloudState::from_attributes(
                FILE_ATTRIBUTE_PINNED | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
            ),
            CloudState::AlwaysKeep
        );
        assert_eq!(
            CloudState::from_attributes(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS),
            CloudState::OnlineOnly
        );
        assert_eq!(CloudState::from_attributes(0), CloudState::LocallyAvailable);
    }

    #[test]
    fn win32_attribute_mapping() {
        use crate::win32::*;
        let f = EntryFlags::from_win32_attributes(
            FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_OFFLINE,
        );
        assert!(f.contains(EntryFlags::HIDDEN | EntryFlags::DIR | EntryFlags::OFFLINE));
        assert!(!f.contains(EntryFlags::SYSTEM));
    }
}
