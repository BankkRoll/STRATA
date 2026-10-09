//! USN change journal record parsing (SPEC §10.1).
//!
//! Pure decoding of the buffer returned by `FSCTL_READ_USN_JOURNAL` (and
//! `FSCTL_ENUM_USN_DATA`): an 8-byte next USN followed by packed
//! `USN_RECORD_V2`, `USN_RECORD_V3` and `USN_RECORD_V4` records. Live tailing
//! is built on top of this in the helper (M5).

use strata_core::{FileRef, FileTime, WideName};

use crate::le::{i64_at, u16_at, u32_at, u64_at, utf16_at};

/// `USN_REASON_DATA_OVERWRITE`
pub const USN_REASON_DATA_OVERWRITE: u32 = 0x0000_0001;
/// `USN_REASON_DATA_EXTEND`
pub const USN_REASON_DATA_EXTEND: u32 = 0x0000_0002;
/// `USN_REASON_DATA_TRUNCATION`
pub const USN_REASON_DATA_TRUNCATION: u32 = 0x0000_0004;
/// `USN_REASON_NAMED_DATA_OVERWRITE`
pub const USN_REASON_NAMED_DATA_OVERWRITE: u32 = 0x0000_0010;
/// `USN_REASON_NAMED_DATA_EXTEND`
pub const USN_REASON_NAMED_DATA_EXTEND: u32 = 0x0000_0020;
/// `USN_REASON_NAMED_DATA_TRUNCATION`
pub const USN_REASON_NAMED_DATA_TRUNCATION: u32 = 0x0000_0040;
/// `USN_REASON_FILE_CREATE`
pub const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
/// `USN_REASON_FILE_DELETE`
pub const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
/// `USN_REASON_EA_CHANGE`
pub const USN_REASON_EA_CHANGE: u32 = 0x0000_0400;
/// `USN_REASON_SECURITY_CHANGE`
pub const USN_REASON_SECURITY_CHANGE: u32 = 0x0000_0800;
/// `USN_REASON_RENAME_OLD_NAME`
pub const USN_REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
/// `USN_REASON_RENAME_NEW_NAME`
pub const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
/// `USN_REASON_INDEXABLE_CHANGE`
pub const USN_REASON_INDEXABLE_CHANGE: u32 = 0x0000_4000;
/// `USN_REASON_BASIC_INFO_CHANGE`
pub const USN_REASON_BASIC_INFO_CHANGE: u32 = 0x0000_8000;
/// `USN_REASON_HARD_LINK_CHANGE`
pub const USN_REASON_HARD_LINK_CHANGE: u32 = 0x0001_0000;
/// `USN_REASON_COMPRESSION_CHANGE`
pub const USN_REASON_COMPRESSION_CHANGE: u32 = 0x0002_0000;
/// `USN_REASON_ENCRYPTION_CHANGE`
pub const USN_REASON_ENCRYPTION_CHANGE: u32 = 0x0004_0000;
/// `USN_REASON_OBJECT_ID_CHANGE`
pub const USN_REASON_OBJECT_ID_CHANGE: u32 = 0x0008_0000;
/// `USN_REASON_REPARSE_POINT_CHANGE`
pub const USN_REASON_REPARSE_POINT_CHANGE: u32 = 0x0010_0000;
/// `USN_REASON_STREAM_CHANGE`
pub const USN_REASON_STREAM_CHANGE: u32 = 0x0020_0000;
/// `USN_REASON_TRANSACTED_CHANGE`
pub const USN_REASON_TRANSACTED_CHANGE: u32 = 0x0040_0000;
/// `USN_REASON_INTEGRITY_CHANGE`
pub const USN_REASON_INTEGRITY_CHANGE: u32 = 0x0080_0000;
/// `USN_REASON_DESIRED_STORAGE_CLASS_CHANGE`
pub const USN_REASON_DESIRED_STORAGE_CLASS_CHANGE: u32 = 0x0100_0000;
/// `USN_REASON_CLOSE`
pub const USN_REASON_CLOSE: u32 = 0x8000_0000;

/// A 128-bit file id. NTFS ids fit in the low 64 bits; ReFS uses all 128.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId128(pub u128);

impl FileId128 {
    /// The NTFS 64-bit file reference, if the high 64 bits are zero.
    #[must_use]
    pub fn as_file_ref(self) -> Option<FileRef> {
        u64::try_from(self.0).ok().map(FileRef)
    }
}

/// A `USN_RECORD_V2` or `USN_RECORD_V3` change record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsnChange {
    /// Major version (2 or 3).
    pub major_version: u16,
    /// Changed file.
    pub file: FileId128,
    /// Its parent directory.
    pub parent: FileId128,
    /// This record's USN.
    pub usn: i64,
    /// When the change was recorded.
    pub timestamp: FileTime,
    /// `USN_REASON_*` bits accumulated since the file was opened.
    pub reason: u32,
    /// `USN_SOURCE_*` bits.
    pub source_info: u32,
    /// Security descriptor id.
    pub security_id: u32,
    /// Win32 file attributes.
    pub attributes: u32,
    /// File name (not a path), raw UTF-16.
    pub name: WideName,
}

/// One extent of a `USN_RECORD_V4` range record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsnExtent {
    /// Byte offset of the changed range.
    pub offset: i64,
    /// Length of the changed range.
    pub length: i64,
}

/// A `USN_RECORD_V4` range-tracking record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsnRange {
    /// Changed file.
    pub file: FileId128,
    /// Its parent directory.
    pub parent: FileId128,
    /// This record's USN.
    pub usn: i64,
    /// `USN_REASON_*` bits.
    pub reason: u32,
    /// `USN_SOURCE_*` bits.
    pub source_info: u32,
    /// Extents still to come in later V4 records for the same change.
    pub remaining_extents: u32,
    /// Changed ranges in this record.
    pub extents: Vec<UsnExtent>,
}

/// One decoded journal record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsnRecord {
    /// V2 or V3 change record.
    Change(UsnChange),
    /// V4 range record.
    Range(UsnRange),
}

/// Why a journal buffer could not be decoded further.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UsnError {
    /// The buffer is shorter than the 8-byte next-USN prefix.
    #[error("USN buffer is shorter than its 8-byte header")]
    ShortBuffer,
    /// A record's length is smaller than its fixed header or runs past the buffer.
    #[error("USN record at offset {0} has an invalid length")]
    BadLength(usize),
    /// Unsupported major version.
    #[error("USN record at offset {offset} has unsupported version {version}")]
    Version {
        /// Byte offset of the record within the buffer.
        offset: usize,
        /// The major version found.
        version: u16,
    },
    /// The name or extents lie outside the record.
    #[error("USN record at offset {0} has a name or extents outside the record")]
    BadField(usize),
}

/// Splits a `FSCTL_READ_USN_JOURNAL` output buffer into the next USN and an
/// iterator over its records.
///
/// # Errors
///
/// The buffer is shorter than 8 bytes.
///
/// # Example
///
/// ```
/// use strata_ntfs::parse_usn_buffer;
/// let buf = 42i64.to_le_bytes();
/// let (next, records) = parse_usn_buffer(&buf).unwrap();
/// assert_eq!(next, 42);
/// assert_eq!(records.count(), 0);
/// ```
pub fn parse_usn_buffer(buf: &[u8]) -> Result<(i64, UsnIter<'_>), UsnError> {
    let next = i64_at(buf, 0).ok_or(UsnError::ShortBuffer)?;
    Ok((next, UsnIter { buf, pos: 8 }))
}

/// Iterator over packed USN records. Yields one error and stops on the
/// first malformed record.
#[derive(Debug, Clone)]
pub struct UsnIter<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Iterator for UsnIter<'_> {
    type Item = Result<UsnRecord, UsnError>;

    fn next(&mut self) -> Option<Self::Item> {
        // NOTE: fewer than 4 trailing bytes cannot hold a record length; the
        // kernel never produces that, so treat it as the end of the buffer.
        if self.buf.len().saturating_sub(self.pos) < 4 {
            return None;
        }
        let at = self.pos;
        match parse_usn_record(&self.buf[at..]) {
            Ok((rec, len)) => {
                self.pos = at + len;
                Some(Ok(rec))
            }
            Err(e) => {
                self.pos = self.buf.len();
                Some(Err(offset_error(e, at)))
            }
        }
    }
}

fn offset_error(e: UsnError, at: usize) -> UsnError {
    match e {
        UsnError::BadLength(o) => UsnError::BadLength(o + at),
        UsnError::BadField(o) => UsnError::BadField(o + at),
        UsnError::Version { offset, version } => UsnError::Version {
            offset: offset + at,
            version,
        },
        UsnError::ShortBuffer => UsnError::ShortBuffer,
    }
}

const V2_HEADER: usize = 60;
const V3_HEADER: usize = 76;
const V4_HEADER: usize = 64;
const V4_EXTENT: usize = 16;

/// Decodes one record at the start of `b`, returning it and its length.
///
/// # Errors
///
/// Any [`UsnError`]; offsets in the error are relative to `b`.
pub fn parse_usn_record(b: &[u8]) -> Result<(UsnRecord, usize), UsnError> {
    let len = u32_at(b, 0).ok_or(UsnError::BadLength(0))? as usize;
    let major = u16_at(b, 4).ok_or(UsnError::BadLength(0))?;
    let header = match major {
        2 => V2_HEADER,
        3 => V3_HEADER,
        4 => V4_HEADER,
        version => return Err(UsnError::Version { offset: 0, version }),
    };
    if len < header || len > b.len() {
        return Err(UsnError::BadLength(0));
    }
    let r = &b[..len];
    let short = UsnError::BadLength(0);
    let id128 = |o: usize| -> Option<FileId128> {
        let lo = u64_at(r, o)?;
        let hi = u64_at(r, o + 8)?;
        Some(FileId128((u128::from(hi) << 64) | u128::from(lo)))
    };
    let rec = match major {
        2 | 3 => {
            let (file, parent, base) = if major == 2 {
                let f = FileId128(u128::from(u64_at(r, 8).ok_or(short)?));
                let p = FileId128(u128::from(u64_at(r, 16).ok_or(short)?));
                (f, p, 24)
            } else {
                (id128(8).ok_or(short)?, id128(24).ok_or(short)?, 40)
            };
            let name_len = usize::from(u16_at(r, base + 32).ok_or(short)?);
            let name_off = usize::from(u16_at(r, base + 34).ok_or(short)?);
            if name_len % 2 != 0 {
                return Err(UsnError::BadField(0));
            }
            let name = utf16_at(r, name_off, name_len / 2).ok_or(UsnError::BadField(0))?;
            UsnRecord::Change(UsnChange {
                major_version: major,
                file,
                parent,
                usn: i64_at(r, base).ok_or(short)?,
                timestamp: FileTime(u64_at(r, base + 8).ok_or(short)?),
                reason: u32_at(r, base + 16).ok_or(short)?,
                source_info: u32_at(r, base + 20).ok_or(short)?,
                security_id: u32_at(r, base + 24).ok_or(short)?,
                attributes: u32_at(r, base + 28).ok_or(short)?,
                name: WideName::from_units(name),
            })
        }
        _ => {
            let count = usize::from(u16_at(r, 60).ok_or(short)?);
            let size = usize::from(u16_at(r, 62).ok_or(short)?);
            if count > 0 && size < V4_EXTENT {
                return Err(UsnError::BadField(0));
            }
            let mut extents = Vec::with_capacity(count.min(len / V4_EXTENT));
            for i in 0..count {
                let o = V4_HEADER + i * size;
                extents.push(UsnExtent {
                    offset: i64_at(r, o).ok_or(UsnError::BadField(0))?,
                    length: i64_at(r, o + 8).ok_or(UsnError::BadField(0))?,
                });
            }
            UsnRecord::Range(UsnRange {
                file: id128(8).ok_or(short)?,
                parent: id128(24).ok_or(short)?,
                usn: i64_at(r, 40).ok_or(short)?,
                reason: u32_at(r, 48).ok_or(short)?,
                source_info: u32_at(r, 52).ok_or(short)?,
                remaining_extents: u32_at(r, 56).ok_or(short)?,
                extents,
            })
        }
    };
    Ok((rec, len))
}

/// Encoders for building journal buffers in tests and fuzz seeds.
#[cfg(any(test, feature = "test-image"))]
pub mod encode {
    use super::{UsnChange, UsnRange, V2_HEADER, V3_HEADER, V4_EXTENT, V4_HEADER};

    fn pad8(v: &mut Vec<u8>) {
        while !v.len().is_multiple_of(8) {
            v.push(0);
        }
    }

    /// Encodes a V2 (if `c.major_version == 2`) or V3 record.
    #[must_use]
    pub fn change(c: &UsnChange) -> Vec<u8> {
        let v3 = c.major_version == 3;
        let header = if v3 { V3_HEADER } else { V2_HEADER };
        let mut v = vec![0u8; header];
        v[4..6].copy_from_slice(&c.major_version.to_le_bytes());
        let base = if v3 {
            v[8..24].copy_from_slice(&c.file.0.to_le_bytes());
            v[24..40].copy_from_slice(&c.parent.0.to_le_bytes());
            40
        } else {
            v[8..16].copy_from_slice(&(c.file.0 as u64).to_le_bytes());
            v[16..24].copy_from_slice(&(c.parent.0 as u64).to_le_bytes());
            24
        };
        v[base..base + 8].copy_from_slice(&c.usn.to_le_bytes());
        v[base + 8..base + 16].copy_from_slice(&c.timestamp.0.to_le_bytes());
        v[base + 16..base + 20].copy_from_slice(&c.reason.to_le_bytes());
        v[base + 20..base + 24].copy_from_slice(&c.source_info.to_le_bytes());
        v[base + 24..base + 28].copy_from_slice(&c.security_id.to_le_bytes());
        v[base + 28..base + 32].copy_from_slice(&c.attributes.to_le_bytes());
        let name: Vec<u8> = c
            .name
            .units()
            .iter()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        v[base + 32..base + 34].copy_from_slice(&(name.len() as u16).to_le_bytes());
        v[base + 34..base + 36].copy_from_slice(&(header as u16).to_le_bytes());
        v.extend_from_slice(&name);
        pad8(&mut v);
        let len = v.len() as u32;
        v[0..4].copy_from_slice(&len.to_le_bytes());
        v
    }

    /// Encodes a V4 range record.
    #[must_use]
    pub fn range(r: &UsnRange) -> Vec<u8> {
        let mut v = vec![0u8; V4_HEADER];
        v[4..6].copy_from_slice(&4u16.to_le_bytes());
        v[8..24].copy_from_slice(&r.file.0.to_le_bytes());
        v[24..40].copy_from_slice(&r.parent.0.to_le_bytes());
        v[40..48].copy_from_slice(&r.usn.to_le_bytes());
        v[48..52].copy_from_slice(&r.reason.to_le_bytes());
        v[52..56].copy_from_slice(&r.source_info.to_le_bytes());
        v[56..60].copy_from_slice(&r.remaining_extents.to_le_bytes());
        v[60..62].copy_from_slice(&(r.extents.len() as u16).to_le_bytes());
        v[62..64].copy_from_slice(&(V4_EXTENT as u16).to_le_bytes());
        for e in &r.extents {
            v.extend_from_slice(&e.offset.to_le_bytes());
            v.extend_from_slice(&e.length.to_le_bytes());
        }
        let len = v.len() as u32;
        v[0..4].copy_from_slice(&len.to_le_bytes());
        v
    }

    /// Prefixes records with a next-USN header, as `FSCTL_READ_USN_JOURNAL` returns.
    #[must_use]
    pub fn buffer(next_usn: i64, records: &[Vec<u8>]) -> Vec<u8> {
        let mut v = next_usn.to_le_bytes().to_vec();
        for r in records {
            v.extend_from_slice(r);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(major: u16, file: u128) -> UsnChange {
        UsnChange {
            major_version: major,
            file: FileId128(file),
            parent: FileId128(5 | (5 << 48)),
            usn: 4096,
            timestamp: FileTime(133_000_000_000_000_000),
            reason: USN_REASON_FILE_CREATE | USN_REASON_CLOSE,
            source_info: 0,
            security_id: 7,
            attributes: 0x20,
            name: WideName::from_units(vec![u16::from(b'a'), 0xD800]),
        }
    }

    #[test]
    fn round_trips_all_versions() {
        let v2 = sample(2, 0x0001_0000_0000_0040);
        let v3 = sample(3, u128::MAX - 1);
        let v4 = UsnRange {
            file: FileId128(77),
            parent: FileId128(5),
            usn: 8192,
            reason: USN_REASON_DATA_OVERWRITE,
            source_info: 0,
            remaining_extents: 3,
            extents: vec![
                UsnExtent {
                    offset: 0,
                    length: 4096,
                },
                UsnExtent {
                    offset: 65536,
                    length: 512,
                },
            ],
        };
        let buf = encode::buffer(
            9000,
            &[encode::change(&v2), encode::change(&v3), encode::range(&v4)],
        );
        let (next, it) = parse_usn_buffer(&buf).unwrap();
        assert_eq!(next, 9000);
        let recs: Vec<_> = it.collect::<Result<_, _>>().unwrap();
        assert_eq!(
            recs,
            vec![
                UsnRecord::Change(v2.clone()),
                UsnRecord::Change(v3),
                UsnRecord::Range(v4)
            ]
        );
        assert_eq!(v2.file.as_file_ref(), Some(FileRef(0x0001_0000_0000_0040)));
        assert_eq!(FileId128(u128::MAX).as_file_ref(), None);
    }

    #[test]
    fn rejects_malformed_records() {
        assert_eq!(parse_usn_buffer(&[0; 7]).err(), Some(UsnError::ShortBuffer));
        let mut rec = encode::change(&sample(2, 1));
        // Zero length must not loop forever.
        let mut zero = rec.clone();
        zero[0..4].copy_from_slice(&0u32.to_le_bytes());
        let buf = encode::buffer(0, &[zero]);
        let out: Vec<_> = parse_usn_buffer(&buf).unwrap().1.collect();
        assert_eq!(out, vec![Err(UsnError::BadLength(8))]);
        // Name past the record.
        rec[56..58].copy_from_slice(&200u16.to_le_bytes());
        assert_eq!(parse_usn_record(&rec).err(), Some(UsnError::BadField(0)));
        // Unknown version.
        rec[4..6].copy_from_slice(&9u16.to_le_bytes());
        assert!(matches!(
            parse_usn_record(&rec),
            Err(UsnError::Version { version: 9, .. })
        ));
        // V4 with an undersized extent stride.
        let mut r4 = encode::range(&UsnRange {
            file: FileId128(1),
            parent: FileId128(5),
            usn: 0,
            reason: 0,
            source_info: 0,
            remaining_extents: 0,
            extents: vec![UsnExtent {
                offset: 0,
                length: 1,
            }],
        });
        r4[62..64].copy_from_slice(&8u16.to_le_bytes());
        assert_eq!(parse_usn_record(&r4).err(), Some(UsnError::BadField(0)));
        r4[60..62].copy_from_slice(&1000u16.to_le_bytes());
        r4[62..64].copy_from_slice(&16u16.to_le_bytes());
        assert_eq!(parse_usn_record(&r4).err(), Some(UsnError::BadField(0)));
    }
}
