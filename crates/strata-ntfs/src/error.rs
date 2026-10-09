//! Error types.
//!
//! Volume-level failures (bad boot sector, unreadable `$MFT`, I/O) are
//! [`NtfsError`]s. Per-record problems during a scan are not errors: they are
//! counted in [`crate::ScanStats`] and the record is skipped.

use std::io;

/// Why a runlist (mapping pairs array) was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RunlistError {
    /// The array ended without a terminating zero header byte.
    #[error("runlist is truncated")]
    Truncated,
    /// A length or offset field is wider than 8 bytes, or the length field is empty.
    #[error("runlist field size {0} is invalid")]
    FieldSize(u8),
    /// A run has length zero or a negative length.
    #[error("runlist contains a zero or negative run length")]
    BadLength,
    /// A run starts before LCN 0 or ends past the last cluster of the volume.
    #[error("run at LCN {lcn} with {len} clusters is outside the volume")]
    OutOfVolume {
        /// First cluster of the run (may be negative after a bad delta).
        lcn: i64,
        /// Run length in clusters.
        len: u64,
    },
    /// VCN or LCN arithmetic overflowed.
    #[error("runlist arithmetic overflow")]
    Overflow,
    /// More runs than any plausible attribute holds.
    #[error("runlist has more than {0} runs")]
    TooManyRuns(usize),
}

/// Why a record could not be parsed. Counted as "malformed" during scans.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    /// Header fields (offsets, used size) are inconsistent with the buffer.
    #[error("record header is invalid: {0}")]
    Header(&'static str),
    /// An attribute header or value lies outside the record.
    #[error("attribute at offset {offset:#x} is invalid: {reason}")]
    Attribute {
        /// Byte offset of the attribute within the record.
        offset: usize,
        /// What was wrong.
        reason: &'static str,
    },
    /// A runlist inside the record was invalid.
    #[error("runlist: {0}")]
    Runlist(#[from] RunlistError),
}

/// Volume-level failure.
#[derive(Debug, thiserror::Error)]
pub enum NtfsError {
    /// Underlying read failed.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    /// The boot sector is not a valid NTFS boot sector.
    #[error("invalid NTFS boot sector: {0}")]
    Boot(&'static str),
    /// `$MFT` (record 0) could not be bootstrapped.
    #[error("cannot locate the MFT: {0}")]
    Mft(String),
    /// A record that had to be parsed (not merely skipped) was invalid.
    #[error("record {record}: {source}")]
    Record {
        /// MFT record number.
        record: u64,
        /// What was wrong.
        source: RecordError,
    },
    /// A record number lies past the end of the MFT.
    #[error("record {0} is beyond the end of the MFT")]
    OutOfRange(u64),
    /// An option passed by the caller is invalid.
    #[error("invalid option: {0}")]
    InvalidOption(String),
}

/// Convenience alias.
pub type Result<T, E = NtfsError> = std::result::Result<T, E>;
