//! MFT record header parsing and per-record attribute extraction.
//!
//! [`parse_record`] turns one raw record into a [`RecordOutcome`]: a
//! classification for the scan statistics plus, for in-use records, a
//! [`ParsedRecord`] holding everything the scanner needs from that record
//! alone. Records that spill into extension records are completed later by
//! [`crate::assemble`].

use strata_core::{FileRef, FileTime, NameLink, Reparse, WideName};

use crate::attr::{
    AT_ATTRIBUTE_LIST, AT_BITMAP, AT_DATA, AT_FILE_NAME, AT_INDEX_ALLOCATION, AT_REPARSE_POINT,
    AT_STANDARD_INFORMATION, Attr, AttrForm, AttrIter, NS_DOS, StdInfo, parse_file_name,
    parse_reparse, parse_std_info,
};
use crate::error::RecordError;
use crate::fixup::{FixupError, apply_fixups};
use crate::le::{u16_at, u32_at, u64_at};
use crate::runlist::{Run, decode_runlist};

/// Record header flag: record is in use.
pub const RECORD_IN_USE: u16 = 0x0001;
/// Record header flag: record is a directory (has a `$I30` index).
pub const RECORD_IS_DIRECTORY: u16 = 0x0002;

/// Decoded fixed part of an MFT record header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordHeader {
    /// Offset of the update sequence array.
    pub usa_offset: u16,
    /// Number of update sequence entries (strides + 1).
    pub usa_count: u16,
    /// `$LogFile` sequence number.
    pub lsn: u64,
    /// Sequence number, bumped each time the record is reused.
    pub sequence: u16,
    /// Hard link count as maintained by NTFS.
    pub link_count: u16,
    /// Offset of the first attribute.
    pub first_attribute: u16,
    /// Header flags (`RECORD_*`).
    pub flags: u16,
    /// Bytes in use, including the end marker.
    pub used_size: u32,
    /// Bytes allocated for the record.
    pub allocated_size: u32,
    /// Base record reference; zero for base records.
    pub base: FileRef,
    /// Next attribute instance id.
    pub next_attribute_id: u16,
}

impl RecordHeader {
    /// Decodes the header without validating it against the buffer.
    ///
    /// # Errors
    ///
    /// The buffer is shorter than the 0x2A-byte header.
    pub fn parse(b: &[u8]) -> Result<Self, RecordError> {
        let short = RecordError::Header("record shorter than its header");
        Ok(Self {
            usa_offset: u16_at(b, 0x04).ok_or(short.clone())?,
            usa_count: u16_at(b, 0x06).ok_or(short.clone())?,
            lsn: u64_at(b, 0x08).ok_or(short.clone())?,
            sequence: u16_at(b, 0x10).ok_or(short.clone())?,
            link_count: u16_at(b, 0x12).ok_or(short.clone())?,
            first_attribute: u16_at(b, 0x14).ok_or(short.clone())?,
            flags: u16_at(b, 0x16).ok_or(short.clone())?,
            used_size: u32_at(b, 0x18).ok_or(short.clone())?,
            allocated_size: u32_at(b, 0x1C).ok_or(short.clone())?,
            base: FileRef(u64_at(b, 0x20).ok_or(short.clone())?),
            next_attribute_id: u16_at(b, 0x28).ok_or(short)?,
        })
    }

    /// Whether the in-use flag is set.
    #[must_use]
    pub fn in_use(&self) -> bool {
        self.flags & RECORD_IN_USE != 0
    }
}

/// An attribute value that may need to be read from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueLoc {
    /// Value copied out of the record.
    Resident(Vec<u8>),
    /// Value stored in clusters.
    NonResident {
        /// Decoded runs of the instance starting at VCN 0.
        runs: Vec<Run>,
        /// Real size of the value in bytes.
        size: u64,
    },
}

/// One `$DATA` attribute instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPiece {
    /// Stream name; empty for the unnamed (content) stream.
    pub name: WideName,
    /// First VCN of this instance (0 for resident data).
    pub start_vcn: u64,
    /// Whether the value is resident.
    pub resident: bool,
    /// Attribute flags.
    pub flags: u16,
    /// Logical size. Meaningful only when `start_vcn == 0`.
    pub logical: u64,
    /// Initialized size (equals `logical` for resident data). Meaningful only
    /// when `start_vcn == 0`.
    pub initialized: u64,
    /// Bytes on disk: 0 for resident data, total-allocated for
    /// compressed or sparse, else allocated. Meaningful only when `start_vcn == 0`.
    pub allocated: u64,
    /// Decoded runs, when the caller asked for them ([`ParseOptions::decode_runs`]).
    pub runs: Option<Vec<Run>>,
}

/// One `$INDEX_ALLOCATION` attribute instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexPiece {
    /// Index name (`$I30` for directories).
    pub name: WideName,
    /// First VCN of this instance.
    pub start_vcn: u64,
    /// Bytes on disk. Meaningful only when `start_vcn == 0`.
    pub allocated: u64,
}

/// A `$REPARSE_POINT` value, decoded when resident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReparseLoc {
    /// Decoded from the resident value.
    Resident(Reparse),
    /// Must be read from disk before it can be decoded.
    NonResident(ValueLoc),
}

/// Everything the scanner extracts from one in-use record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRecord {
    /// MFT record number (the record's index in `$MFT`).
    pub record: u64,
    /// Sequence number from the header.
    pub sequence: u16,
    /// Link count from the header.
    pub link_count: u16,
    /// Directory flag from the header.
    pub is_dir: bool,
    /// Base record for extension records; `None` for base records.
    pub base: Option<FileRef>,
    /// `$STANDARD_INFORMATION`, if present in this record.
    pub std_info: Option<StdInfo>,
    /// One link per non-DOS `$FILE_NAME`, in attribute order.
    pub links: Vec<NameLink>,
    /// `$FILE_NAME` creation time of the first link in this record.
    pub fn_created: Option<FileTime>,
    /// `$DATA` instances, in attribute order.
    pub data: Vec<DataPiece>,
    /// `$INDEX_ALLOCATION` instances.
    pub index_allocations: Vec<IndexPiece>,
    /// `$REPARSE_POINT`, if present.
    pub reparse: Option<ReparseLoc>,
    /// `$ATTRIBUTE_LIST`, if present.
    pub attr_list: Option<ValueLoc>,
    /// Unnamed `$BITMAP`, captured only with [`ParseOptions::capture_bitmap`].
    pub bitmap: Option<ValueLoc>,
    /// On-disk bytes of other non-resident attributes (`$ATTRIBUTE_LIST`,
    /// `$BITMAP`, `$EA`, `$LOGGED_UTILITY_STREAM`, ...) from their VCN-0
    /// instances. Becomes [`strata_core::Sizes::attr_overhead`] on assembly.
    pub other_allocated: u64,
}

impl ParsedRecord {
    /// This record's file reference.
    #[must_use]
    pub fn file_ref(&self) -> FileRef {
        FileRef::from_parts(self.record, self.sequence)
    }

    /// Whether completing this record needs other records or disk reads.
    #[must_use]
    pub fn needs_completion(&self) -> bool {
        self.attr_list.is_some() || matches!(self.reparse, Some(ReparseLoc::NonResident(_)))
    }
}

/// Knobs for [`parse_record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseOptions {
    /// Clusters in the volume; bounds every decoded LCN.
    pub total_clusters: u64,
    /// Decode `$DATA` runlists into [`DataPiece::runs`].
    pub decode_runs: bool,
    /// Capture the unnamed `$BITMAP` value into [`ParsedRecord::bitmap`].
    pub capture_bitmap: bool,
}

impl ParseOptions {
    /// Options for a full scan: no runlists except where a value must be read.
    #[must_use]
    pub fn scan(total_clusters: u64) -> Self {
        Self {
            total_clusters,
            decode_runs: false,
            capture_bitmap: false,
        }
    }

    /// These options adjusted for record `n`: reserved metadata records also
    /// decode their runlists, which the `$BadClus:$Bad` size rule needs.
    #[must_use]
    pub fn for_record(self, n: u64) -> Self {
        Self {
            decode_runs: self.decode_runs || n < crate::assemble::FIRST_USER_RECORD,
            ..self
        }
    }
}

/// Classification of one raw record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
    /// Not in use, or never initialized (all-zero signature).
    Free,
    /// `BAAD` signature: NTFS marked the record corrupt.
    Baad,
    /// Neither `FILE`, `BAAD` nor zero: garbage.
    BadSignature,
    /// Fixup mismatch: torn write.
    Torn,
    /// `FILE` record whose header or attributes are inconsistent.
    Malformed(RecordError),
    /// A valid in-use record.
    InUse(Box<ParsedRecord>),
}

/// Applies fixups to `buf` (exactly one record) and parses it.
///
/// `record` is the record's index in `$MFT`; the header's own record-number
/// field is ignored because NTFS 1.x records do not have one.
///
/// # Example
///
/// ```
/// use strata_ntfs::{parse_record, ParseOptions, RecordOutcome};
/// let mut zeros = vec![0u8; 1024];
/// assert_eq!(parse_record(&mut zeros, 7, &ParseOptions::scan(100)), RecordOutcome::Free);
/// ```
pub fn parse_record(buf: &mut [u8], record: u64, opts: &ParseOptions) -> RecordOutcome {
    match buf.get(0..4) {
        Some(b"FILE") => {}
        Some(b"BAAD") => return RecordOutcome::Baad,
        Some([0, 0, 0, 0]) => return RecordOutcome::Free,
        _ => return RecordOutcome::BadSignature,
    }
    // NOTE: the flags word is never under a fixup tail, so free records can
    // be skipped before paying for fixup verification.
    match u16_at(buf, 0x16) {
        Some(f) if f & RECORD_IN_USE != 0 => {}
        Some(_) => return RecordOutcome::Free,
        None => return RecordOutcome::Malformed(RecordError::Header("record too short")),
    }
    match apply_fixups(buf) {
        Ok(()) => {}
        Err(FixupError::Mismatch { .. }) => return RecordOutcome::Torn,
        Err(FixupError::BadArray) => {
            return RecordOutcome::Malformed(RecordError::Header("update sequence array"));
        }
    }
    match parse_fixed_record(buf, record, opts) {
        Ok(p) => RecordOutcome::InUse(Box::new(p)),
        Err(e) => RecordOutcome::Malformed(e),
    }
}

/// Parses a record whose fixups were already applied.
///
/// # Errors
///
/// Any [`RecordError`]: inconsistent header, attribute outside the record,
/// undecodable attribute value, or an invalid runlist that had to be decoded.
pub fn parse_fixed_record(
    buf: &[u8],
    record: u64,
    opts: &ParseOptions,
) -> Result<ParsedRecord, RecordError> {
    let h = RecordHeader::parse(buf)?;
    let used = h.used_size as usize;
    let first = usize::from(h.first_attribute);
    let usa_end = usize::from(h.usa_offset) + usize::from(h.usa_count) * 2;
    if used > buf.len() {
        return Err(RecordError::Header("used size exceeds record size"));
    }
    if first < usa_end || first < 0x18 || first.saturating_add(4) > used {
        return Err(RecordError::Header("first attribute offset is invalid"));
    }
    let mut p = ParsedRecord {
        record,
        sequence: h.sequence,
        link_count: h.link_count,
        is_dir: h.flags & RECORD_IS_DIRECTORY != 0,
        base: (h.base.0 != 0).then_some(h.base),
        std_info: None,
        links: Vec::new(),
        fn_created: None,
        data: Vec::new(),
        index_allocations: Vec::new(),
        reparse: None,
        attr_list: None,
        bitmap: None,
        other_allocated: 0,
    };
    for attr in AttrIter::new(buf, first, used) {
        let attr = attr?;
        let bad = |reason| RecordError::Attribute {
            offset: attr.offset,
            reason,
        };
        match (attr.type_code, attr.form) {
            (AT_STANDARD_INFORMATION, AttrForm::Resident { value }) => {
                if p.std_info.is_none() {
                    p.std_info = Some(parse_std_info(value).map_err(bad)?);
                }
            }
            (AT_FILE_NAME, AttrForm::Resident { value }) => {
                let n = parse_file_name(value).map_err(bad)?;
                if n.namespace != NS_DOS {
                    p.fn_created = p.fn_created.or(Some(n.times.created));
                    p.links.push(NameLink {
                        parent: n.parent,
                        name: n.name,
                    });
                }
            }
            (AT_DATA, _) => p.data.push(data_piece(&attr, opts)?),
            (AT_INDEX_ALLOCATION, AttrForm::NonResident(nr)) => {
                p.index_allocations.push(IndexPiece {
                    name: WideName::from_units(attr.name_units()),
                    start_vcn: nr.start_vcn,
                    allocated: nr.on_disk(),
                })
            }
            (AT_REPARSE_POINT, AttrForm::Resident { value }) => {
                p.reparse = Some(ReparseLoc::Resident(parse_reparse(value).map_err(bad)?));
            }
            (AT_REPARSE_POINT, AttrForm::NonResident(nr)) => {
                p.other_allocated = p.other_allocated.saturating_add(vcn0_on_disk(&nr));
                p.reparse = Some(ReparseLoc::NonResident(value_loc(&attr, opts)?));
            }
            (AT_ATTRIBUTE_LIST, form) => {
                if let AttrForm::NonResident(nr) = form {
                    p.other_allocated = p.other_allocated.saturating_add(vcn0_on_disk(&nr));
                }
                p.attr_list = Some(value_loc(&attr, opts)?);
            }
            (AT_BITMAP, form) => {
                if let AttrForm::NonResident(nr) = form {
                    p.other_allocated = p.other_allocated.saturating_add(vcn0_on_disk(&nr));
                }
                // NOTE: a large $MFT:$BITMAP is split across records once the
                // MFT grows; only the piece starting at VCN 0 is captured here,
                // and the MFT loader detects the split from the attribute list.
                let continuation =
                    matches!(attr.form, AttrForm::NonResident(nr) if nr.start_vcn != 0);
                if opts.capture_bitmap && attr.is_unnamed() && !continuation {
                    p.bitmap = Some(value_loc(&attr, opts)?);
                }
            }
            (AT_STANDARD_INFORMATION | AT_FILE_NAME, AttrForm::NonResident(_)) => {
                return Err(bad("attribute must be resident"));
            }
            (_, AttrForm::NonResident(nr)) => {
                p.other_allocated = p.other_allocated.saturating_add(vcn0_on_disk(&nr));
            }
            _ => {}
        }
    }
    Ok(p)
}

fn vcn0_on_disk(nr: &crate::attr::NonResident<'_>) -> u64 {
    if nr.start_vcn == 0 { nr.on_disk() } else { 0 }
}

fn data_piece(attr: &Attr<'_>, opts: &ParseOptions) -> Result<DataPiece, RecordError> {
    let name = WideName::from_units(attr.name_units());
    Ok(match attr.form {
        AttrForm::Resident { value } => DataPiece {
            name,
            start_vcn: 0,
            resident: true,
            flags: attr.flags,
            logical: value.len() as u64,
            initialized: value.len() as u64,
            allocated: 0,
            runs: None,
        },
        AttrForm::NonResident(nr) => DataPiece {
            name,
            start_vcn: nr.start_vcn,
            resident: false,
            flags: attr.flags,
            logical: nr.real,
            initialized: nr.initialized,
            allocated: nr.on_disk(),
            runs: if opts.decode_runs {
                Some(decode_runlist(
                    nr.runlist,
                    nr.start_vcn,
                    opts.total_clusters,
                )?)
            } else {
                None
            },
        },
    })
}

fn value_loc(attr: &Attr<'_>, opts: &ParseOptions) -> Result<ValueLoc, RecordError> {
    Ok(match attr.form {
        AttrForm::Resident { value } => ValueLoc::Resident(value.to_vec()),
        AttrForm::NonResident(nr) => {
            if nr.start_vcn != 0 {
                return Err(RecordError::Attribute {
                    offset: attr.offset,
                    reason: "value attribute does not start at VCN 0",
                });
            }
            ValueLoc::NonResident {
                runs: decode_runlist(nr.runlist, 0, opts.total_clusters)?,
                size: nr.real,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_are_classified() {
        let opts = ParseOptions::scan(10);
        let mut b = vec![0u8; 1024];
        b[0..4].copy_from_slice(b"BAAD");
        assert_eq!(parse_record(&mut b, 0, &opts), RecordOutcome::Baad);
        b[0..4].copy_from_slice(b"XXXX");
        assert_eq!(parse_record(&mut b, 0, &opts), RecordOutcome::BadSignature);
        b[0..4].copy_from_slice(b"FILE");
        assert_eq!(parse_record(&mut b, 0, &opts), RecordOutcome::Free);
        b[0x16] = 1;
        assert!(matches!(
            parse_record(&mut b, 0, &opts),
            RecordOutcome::Malformed(RecordError::Header(_))
        ));
        assert!(matches!(
            parse_record(&mut [b'F', b'I', b'L', b'E'], 0, &opts),
            RecordOutcome::Malformed(_)
        ));
        assert_eq!(parse_record(&mut [], 0, &opts), RecordOutcome::BadSignature);
    }
}
