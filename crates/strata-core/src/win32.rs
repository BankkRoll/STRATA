//! Win32 file attribute bits and reparse tags.
//!
//! Values are from the Windows SDK (`winnt.h`). They are duplicated here so
//! pure crates (parsers, index, classifier) need no Windows dependency.

/// `FILE_ATTRIBUTE_READONLY`
pub const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
/// `FILE_ATTRIBUTE_HIDDEN`
pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
/// `FILE_ATTRIBUTE_SYSTEM`
pub const FILE_ATTRIBUTE_SYSTEM: u32 = 0x0000_0004;
/// `FILE_ATTRIBUTE_DIRECTORY`
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
/// `FILE_ATTRIBUTE_ARCHIVE`
pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;
/// `FILE_ATTRIBUTE_NORMAL`
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
/// `FILE_ATTRIBUTE_TEMPORARY`
pub const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x0000_0100;
/// `FILE_ATTRIBUTE_SPARSE_FILE`
pub const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x0000_0200;
/// `FILE_ATTRIBUTE_REPARSE_POINT`
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
/// `FILE_ATTRIBUTE_COMPRESSED`
pub const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x0000_0800;
/// `FILE_ATTRIBUTE_OFFLINE`
pub const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
/// `FILE_ATTRIBUTE_NOT_CONTENT_INDEXED`
pub const FILE_ATTRIBUTE_NOT_CONTENT_INDEXED: u32 = 0x0000_2000;
/// `FILE_ATTRIBUTE_ENCRYPTED`
pub const FILE_ATTRIBUTE_ENCRYPTED: u32 = 0x0000_4000;
/// `FILE_ATTRIBUTE_INTEGRITY_STREAM`
pub const FILE_ATTRIBUTE_INTEGRITY_STREAM: u32 = 0x0000_8000;
/// `FILE_ATTRIBUTE_RECALL_ON_OPEN` (cloud placeholder: opening recalls data)
pub const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
/// `FILE_ATTRIBUTE_PINNED` ("Always keep on this device")
pub const FILE_ATTRIBUTE_PINNED: u32 = 0x0008_0000;
/// `FILE_ATTRIBUTE_UNPINNED` ("Free up space": online-only)
pub const FILE_ATTRIBUTE_UNPINNED: u32 = 0x0010_0000;
/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` (cloud placeholder: reading recalls data)
pub const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// `IO_REPARSE_TAG_MOUNT_POINT` (junctions and volume mount points)
pub const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
/// `IO_REPARSE_TAG_SYMLINK`
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
/// `IO_REPARSE_TAG_DEDUP`
pub const IO_REPARSE_TAG_DEDUP: u32 = 0x8000_0013;
/// `IO_REPARSE_TAG_WOF` (Windows Overlay Filter: CompactOS / `compact /exe`)
pub const IO_REPARSE_TAG_WOF: u32 = 0x8000_0017;
/// `IO_REPARSE_TAG_APPEXECLINK` (app execution aliases)
pub const IO_REPARSE_TAG_APPEXECLINK: u32 = 0x8000_001B;
/// `IO_REPARSE_TAG_LX_SYMLINK` (WSL symlink)
pub const IO_REPARSE_TAG_LX_SYMLINK: u32 = 0xA000_001D;
/// `IO_REPARSE_TAG_AF_UNIX` (WSL Unix socket)
pub const IO_REPARSE_TAG_AF_UNIX: u32 = 0x8000_0023;
/// `IO_REPARSE_TAG_LX_FIFO` (WSL FIFO)
pub const IO_REPARSE_TAG_LX_FIFO: u32 = 0x8000_0024;
/// `IO_REPARSE_TAG_LX_CHR` (WSL character device)
pub const IO_REPARSE_TAG_LX_CHR: u32 = 0x8000_0025;
/// `IO_REPARSE_TAG_LX_BLK` (WSL block device)
pub const IO_REPARSE_TAG_LX_BLK: u32 = 0x8000_0026;
/// `IO_REPARSE_TAG_CLOUD` base value. The cloud family is `0x9000_X01A`:
/// the 4-bit nibble at bits 12..16 encodes the provider sub-tag, so mask with
/// [`IO_REPARSE_TAG_CLOUD_MASK`] before comparing.
pub const IO_REPARSE_TAG_CLOUD: u32 = 0x9000_001A;
/// Mask that clears the cloud provider nibble.
pub const IO_REPARSE_TAG_CLOUD_MASK: u32 = 0xFFFF_0FFF;

/// Whether `tag` belongs to the cloud-files family (`IO_REPARSE_TAG_CLOUD_*`).
#[must_use]
pub const fn is_cloud_tag(tag: u32) -> bool {
    tag & IO_REPARSE_TAG_CLOUD_MASK == IO_REPARSE_TAG_CLOUD
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_family_matches_every_sub_tag() {
        for n in 0..16u32 {
            assert!(is_cloud_tag(0x9000_001A | (n << 12)));
        }
        assert!(!is_cloud_tag(IO_REPARSE_TAG_SYMLINK));
        assert!(!is_cloud_tag(IO_REPARSE_TAG_WOF));
    }
}
