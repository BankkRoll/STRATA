//! Classification of `USN_REASON_*` bits.
//!
//! NTFS accumulates reason bits per file from the first change after the
//! file is opened until its last handle closes. It writes a record each time
//! a *new* bit is added and one more with [`USN_REASON_CLOSE`] at the close.
//! Changes made while a bit is already set produce no record, so the
//! close record is the only reliable signal that a file reached its final
//! state. Every record that carries a relevant bit (including close
//! records) therefore schedules a refresh; coalescing by file reference
//! keeps that to one fetch per file per tick.

use strata_ntfs::usn::{
    USN_REASON_CLOSE, USN_REASON_DESIRED_STORAGE_CLASS_CHANGE, USN_REASON_FILE_CREATE,
    USN_REASON_FILE_DELETE, USN_REASON_HARD_LINK_CHANGE, USN_REASON_INTEGRITY_CHANGE,
    USN_REASON_OBJECT_ID_CHANGE, USN_REASON_RENAME_NEW_NAME, USN_REASON_RENAME_OLD_NAME,
    USN_REASON_SECURITY_CHANGE, USN_REASON_TRANSACTED_CHANGE,
};

/// Bits that never change anything the index stores (security descriptor,
/// object id, transaction marker, ReFS integrity, storage class) plus the
/// close marker itself.
pub const IRRELEVANT: u32 = USN_REASON_SECURITY_CHANGE
    | USN_REASON_OBJECT_ID_CHANGE
    | USN_REASON_TRANSACTED_CHANGE
    | USN_REASON_INTEGRITY_CHANGE
    | USN_REASON_DESIRED_STORAGE_CLASS_CHANGE
    | USN_REASON_CLOSE;

/// Bits that add, remove or rename a name in a directory, so the directory's
/// own record (its index allocation) may change too.
pub const NAMESPACE: u32 = USN_REASON_FILE_CREATE
    | USN_REASON_FILE_DELETE
    | USN_REASON_RENAME_OLD_NAME
    | USN_REASON_RENAME_NEW_NAME
    | USN_REASON_HARD_LINK_CHANGE;

/// Whether a record with these bits requires re-reading the file.
///
/// # Example
///
/// ```
/// use strata_live::reason::needs_refresh;
/// use strata_ntfs::usn::{USN_REASON_CLOSE, USN_REASON_DATA_EXTEND, USN_REASON_SECURITY_CHANGE};
/// assert!(needs_refresh(USN_REASON_DATA_EXTEND | USN_REASON_CLOSE));
/// assert!(!needs_refresh(USN_REASON_SECURITY_CHANGE | USN_REASON_CLOSE));
/// ```
#[must_use]
pub const fn needs_refresh(reason: u32) -> bool {
    reason & !IRRELEVANT != 0
}

/// Whether the record deletes the file (its last name is gone).
#[must_use]
pub const fn is_delete(reason: u32) -> bool {
    reason & USN_REASON_FILE_DELETE != 0
}

/// Whether the record changes a directory's name set.
#[must_use]
pub const fn is_namespace(reason: u32) -> bool {
    reason & NAMESPACE != 0
}

/// Whether this is the first half of a rename (old name, no new name yet).
#[must_use]
pub const fn opens_rename(reason: u32) -> bool {
    reason & USN_REASON_RENAME_OLD_NAME != 0 && reason & USN_REASON_RENAME_NEW_NAME == 0
}

/// Whether this record carries the new name of a rename.
#[must_use]
pub const fn closes_rename(reason: u32) -> bool {
    reason & USN_REASON_RENAME_NEW_NAME != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_ntfs::usn::*;

    #[test]
    fn every_state_changing_reason_refreshes() {
        for bit in [
            USN_REASON_DATA_OVERWRITE,
            USN_REASON_DATA_EXTEND,
            USN_REASON_DATA_TRUNCATION,
            USN_REASON_NAMED_DATA_OVERWRITE,
            USN_REASON_NAMED_DATA_EXTEND,
            USN_REASON_NAMED_DATA_TRUNCATION,
            USN_REASON_FILE_CREATE,
            USN_REASON_FILE_DELETE,
            USN_REASON_EA_CHANGE,
            USN_REASON_RENAME_OLD_NAME,
            USN_REASON_RENAME_NEW_NAME,
            USN_REASON_INDEXABLE_CHANGE,
            USN_REASON_BASIC_INFO_CHANGE,
            USN_REASON_HARD_LINK_CHANGE,
            USN_REASON_COMPRESSION_CHANGE,
            USN_REASON_ENCRYPTION_CHANGE,
            USN_REASON_REPARSE_POINT_CHANGE,
            USN_REASON_STREAM_CHANGE,
        ] {
            assert!(needs_refresh(bit), "{bit:#x}");
            assert!(needs_refresh(bit | USN_REASON_CLOSE), "{bit:#x}");
        }
        assert!(!needs_refresh(USN_REASON_CLOSE));
        assert!(!needs_refresh(0));
    }

    #[test]
    fn rename_halves() {
        assert!(opens_rename(USN_REASON_RENAME_OLD_NAME));
        assert!(!opens_rename(
            USN_REASON_RENAME_OLD_NAME | USN_REASON_RENAME_NEW_NAME
        ));
        assert!(closes_rename(USN_REASON_RENAME_NEW_NAME | USN_REASON_CLOSE));
        assert!(is_namespace(USN_REASON_HARD_LINK_CHANGE));
        assert!(!is_namespace(USN_REASON_DATA_EXTEND));
    }
}
