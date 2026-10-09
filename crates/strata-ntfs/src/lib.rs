//! NTFS MFT scanner for Strata.
//!
//! Reads the Master File Table directly and emits one merged
//! [`strata_core::ScanRecord`] per file, with exact size accounting
//! (see [`strata_core::Sizes`]). Parsing is pure, safe Rust that never panics on malformed
//! input; the only I/O is positioned reads through [`ReadAt`].
//!
//! Responsibilities:
//! - [`io`]: [`ReadAt`] for memory, image files and raw volumes
//!   (`FILE_FLAG_NO_BUFFERING` or `FILE_FLAG_SEQUENTIAL_SCAN`), and
//!   [`QueuedReader`] for overlapped reads with several requests in flight.
//! - [`BootSector`]: geometry from sector 0.
//! - [`apply_fixups`]: update sequence array verification.
//! - [`parse_record`] / [`AttrIter`]: record headers and attributes.
//! - [`decode_runlist`]: mapping pairs.
//! - [`assemble`]: base + extension records → [`strata_core::ScanRecord`].
//! - [`NtfsVolume`]: `$MFT` bootstrap, [`NtfsVolume::read_record`],
//!   [`NtfsVolume::scan`] (the parallel pipeline), `$Bitmap` reconciliation.
//! - [`usn`]: `USN_RECORD_V2/V3/V4` parsing.
//! - `test_image` (feature `test-image`): synthetic NTFS images.
//!
//! # Example
//!
//! ```
//! use strata_ntfs::test_image::{Geometry, ImageBuilder};
//! use strata_ntfs::{NtfsVolume, ScanOptions};
//!
//! let image = ImageBuilder::new(Geometry::default()).with_system_files().finish();
//! let volume = NtfsVolume::open(image).unwrap();
//! let mut records = Vec::new();
//! let stats = volume.scan(&ScanOptions::default(), |batch| records.extend(batch)).unwrap();
//! assert_eq!(stats.records_emitted as usize, records.len());
//! assert!(records.iter().any(|r| r.id.record() == 5 && r.is_dir()));
//! ```

#![deny(unsafe_code)]

mod assemble;
mod attr;
mod boot;
mod complete;
mod error;
mod fixup;
pub mod io;
mod le;
mod overlapped;
mod record;
mod runlist;
mod scan;
pub mod usn;
mod volume;

#[cfg(feature = "test-image")]
pub mod test_image;

pub use assemble::{BADCLUS_RECORD, EXTEND_RECORD, FIRST_USER_RECORD, WOF_STREAM, assemble};
pub use attr::{
    AT_ATTRIBUTE_LIST, AT_BITMAP, AT_DATA, AT_EA, AT_EA_INFORMATION, AT_END, AT_FILE_NAME,
    AT_INDEX_ALLOCATION, AT_INDEX_ROOT, AT_LOGGED_UTILITY_STREAM, AT_OBJECT_ID, AT_REPARSE_POINT,
    AT_SECURITY_DESCRIPTOR, AT_STANDARD_INFORMATION, AT_VOLUME_INFORMATION, AT_VOLUME_NAME,
    ATTR_FLAG_COMPRESSED, ATTR_FLAG_ENCRYPTED, ATTR_FLAG_SPARSE, Attr, AttrForm, AttrIter,
    AttrListEntry, FileName, NS_DOS, NS_POSIX, NS_WIN32, NS_WIN32_AND_DOS, NonResident, StdInfo,
    parse_attr_list, parse_file_name, parse_reparse, parse_std_info,
};
pub use boot::{BootSector, MAX_CLUSTER_SIZE};
pub use error::{NtfsError, RecordError, Result, RunlistError};
pub use fixup::{FIXUP_STRIDE, FixupError, apply_fixups};
pub use io::{AlignedBuf, IoMode, QueuedReader, RawVolume, ReadAt};
pub use record::{
    DataPiece, IndexPiece, ParseOptions, ParsedRecord, RECORD_IN_USE, RECORD_IS_DIRECTORY,
    RecordHeader, RecordOutcome, ReparseLoc, ValueLoc, parse_fixed_record, parse_record,
};
pub use runlist::{MAX_RUNS, Run, allocated_clusters, decode_runlist, encode_runlist};
pub use scan::{DEFAULT_CHUNK_BYTES, DEFAULT_IO_DEPTH, ScanOptions, ScanStats};
pub use usn::{UsnRecord, parse_usn_buffer};
pub use volume::{BITMAP_RECORD, MFT_RECORD, MftLayout, NtfsVolume};
