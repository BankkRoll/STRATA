//! Attribute headers and attribute value decoders.
//!
//! Responsibilities:
//! - [`AttrIter`]: a bounds-checked walk over the attributes of one fixed-up
//!   record, yielding resident and non-resident headers.
//! - Decoders for `$STANDARD_INFORMATION`, `$FILE_NAME`, `$ATTRIBUTE_LIST`
//!   entries and `$REPARSE_POINT` buffers.
//!
//! Nothing here allocates except to copy names out of the record.

use strata_core::{FileRef, FileTime, Reparse, Times, WideName, win32};

use crate::error::RecordError;
use crate::le::{u8_at, u16_at, u32_at, u64_at, utf16_at};

/// `$STANDARD_INFORMATION`
pub const AT_STANDARD_INFORMATION: u32 = 0x10;
/// `$ATTRIBUTE_LIST`
pub const AT_ATTRIBUTE_LIST: u32 = 0x20;
/// `$FILE_NAME`
pub const AT_FILE_NAME: u32 = 0x30;
/// `$OBJECT_ID`
pub const AT_OBJECT_ID: u32 = 0x40;
/// `$SECURITY_DESCRIPTOR`
pub const AT_SECURITY_DESCRIPTOR: u32 = 0x50;
/// `$VOLUME_NAME`
pub const AT_VOLUME_NAME: u32 = 0x60;
/// `$VOLUME_INFORMATION`
pub const AT_VOLUME_INFORMATION: u32 = 0x70;
/// `$DATA`
pub const AT_DATA: u32 = 0x80;
/// `$INDEX_ROOT`
pub const AT_INDEX_ROOT: u32 = 0x90;
/// `$INDEX_ALLOCATION`
pub const AT_INDEX_ALLOCATION: u32 = 0xA0;
/// `$BITMAP`
pub const AT_BITMAP: u32 = 0xB0;
/// `$REPARSE_POINT`
pub const AT_REPARSE_POINT: u32 = 0xC0;
/// `$EA_INFORMATION`
pub const AT_EA_INFORMATION: u32 = 0xD0;
/// `$EA`
pub const AT_EA: u32 = 0xE0;
/// `$LOGGED_UTILITY_STREAM`
pub const AT_LOGGED_UTILITY_STREAM: u32 = 0x100;
/// End-of-attributes marker.
pub const AT_END: u32 = 0xFFFF_FFFF;

/// Attribute flag: compressed (LZNT1). Non-resident headers then carry the
/// total-allocated field.
pub const ATTR_FLAG_COMPRESSED: u16 = 0x0001;
/// Attribute flag: encrypted (EFS).
pub const ATTR_FLAG_ENCRYPTED: u16 = 0x4000;
/// Attribute flag: sparse. Non-resident headers then carry the
/// total-allocated field.
pub const ATTR_FLAG_SPARSE: u16 = 0x8000;

/// `$FILE_NAME` namespace: POSIX (case-sensitive, any characters but NUL and `/`).
pub const NS_POSIX: u8 = 0;
/// `$FILE_NAME` namespace: Win32 long name.
pub const NS_WIN32: u8 = 1;
/// `$FILE_NAME` namespace: DOS 8.3 alias only. Skipped by the scanner.
pub const NS_DOS: u8 = 2;
/// `$FILE_NAME` namespace: name valid in both Win32 and DOS.
pub const NS_WIN32_AND_DOS: u8 = 3;

/// Non-resident attribute header fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NonResident<'a> {
    /// First VCN covered by this instance.
    pub start_vcn: u64,
    /// Last VCN covered by this instance (inclusive).
    pub last_vcn: u64,
    /// Raw mapping pairs (runlist) bytes, up to the end of the attribute.
    pub runlist: &'a [u8],
    /// Compression unit as a power of two in clusters (0 = uncompressed).
    pub compression_unit: u16,
    /// Allocated size in bytes. Valid only when `start_vcn == 0`.
    pub allocated: u64,
    /// Real (logical) size in bytes. Valid only when `start_vcn == 0`.
    pub real: u64,
    /// Initialized size in bytes. Valid only when `start_vcn == 0`.
    pub initialized: u64,
    /// Total allocated clusters in bytes ("compressed size"), present when
    /// the attribute is compressed or sparse.
    pub total_allocated: Option<u64>,
}

impl NonResident<'_> {
    /// Bytes on disk according to the header: total-allocated for
    /// compressed or sparse attributes, else the allocated size.
    #[must_use]
    pub fn on_disk(&self) -> u64 {
        self.total_allocated.unwrap_or(self.allocated)
    }
}

/// Resident value or non-resident header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrForm<'a> {
    /// Value stored inside the record.
    Resident {
        /// The attribute value.
        value: &'a [u8],
    },
    /// Value stored in clusters described by a runlist.
    NonResident(NonResident<'a>),
}

/// One attribute header within a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attr<'a> {
    /// Attribute type code (`AT_*`).
    pub type_code: u32,
    /// Byte offset of the attribute within the record.
    pub offset: usize,
    /// Attribute flags (`ATTR_FLAG_*`).
    pub flags: u16,
    /// Attribute instance id.
    pub id: u16,
    /// Raw UTF-16LE name bytes (empty for unnamed attributes).
    pub name: &'a [u8],
    /// Value location.
    pub form: AttrForm<'a>,
}

impl Attr<'_> {
    /// Whether the attribute has no name.
    #[must_use]
    pub fn is_unnamed(&self) -> bool {
        self.name.is_empty()
    }

    /// The name as UTF-16 code units.
    #[must_use]
    pub fn name_units(&self) -> Vec<u16> {
        self.name
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    /// Whether the name equals an ASCII string, without allocating.
    #[must_use]
    pub fn name_is(&self, ascii: &str) -> bool {
        name_bytes_eq(self.name, ascii)
    }
}

/// Compares raw UTF-16LE bytes with an ASCII string.
pub(crate) fn name_bytes_eq(raw: &[u8], ascii: &str) -> bool {
    raw.len() == ascii.len() * 2
        && raw
            .chunks_exact(2)
            .zip(ascii.bytes())
            .all(|(c, a)| c[0] == a && c[1] == 0)
}

/// Iterator over the attributes of a fixed-up record.
///
/// Stops at the end marker, at `end`, or after the first error (which it
/// yields once).
#[derive(Debug, Clone)]
pub struct AttrIter<'a> {
    rec: &'a [u8],
    pos: usize,
    end: usize,
    done: bool,
}

impl<'a> AttrIter<'a> {
    /// Walks attributes in `rec[first..end]`. `end` is clamped to the buffer.
    #[must_use]
    pub fn new(rec: &'a [u8], first: usize, end: usize) -> Self {
        Self {
            rec,
            pos: first,
            end: end.min(rec.len()),
            done: false,
        }
    }

    fn parse_one(&mut self) -> Result<Option<Attr<'a>>, RecordError> {
        let off = self.pos;
        let err = |reason| RecordError::Attribute {
            offset: off,
            reason,
        };
        let type_code = match u32_at(self.rec, off) {
            Some(t) if off + 4 <= self.end => t,
            _ => return Err(err("no end marker before the used size")),
        };
        if type_code == AT_END {
            return Ok(None);
        }
        let len = u32_at(self.rec, off + 4).ok_or(err("truncated header"))? as usize;
        if len < 0x18 || !len.is_multiple_of(8) {
            return Err(err("length is too small or not 8-byte aligned"));
        }
        let attr_end = off.checked_add(len).ok_or(err("length overflows"))?;
        if attr_end > self.end {
            return Err(err("extends past the used size"));
        }
        let a = &self.rec[off..attr_end];
        let non_resident = a[8];
        let name_len = usize::from(a[9]) * 2;
        let name_off = usize::from(u16_at(a, 10).ok_or(err("truncated header"))?);
        let flags = u16_at(a, 12).ok_or(err("truncated header"))?;
        let id = u16_at(a, 14).ok_or(err("truncated header"))?;
        let name = if name_len == 0 {
            &[][..]
        } else {
            a.get(name_off..name_off + name_len)
                .ok_or(err("name outside attribute"))?
        };
        let form = match non_resident {
            0 => {
                let vlen = u32_at(a, 0x10).ok_or(err("truncated header"))? as usize;
                let voff = usize::from(u16_at(a, 0x14).ok_or(err("truncated header"))?);
                let value = a
                    .get(voff..voff.checked_add(vlen).ok_or(err("value overflows"))?)
                    .ok_or(err("resident value outside attribute"))?;
                AttrForm::Resident { value }
            }
            1 => {
                if len < 0x40 {
                    return Err(err("non-resident header shorter than 0x40"));
                }
                let start_vcn = u64_at(a, 0x10).ok_or(err("truncated header"))?;
                let last_vcn = u64_at(a, 0x18).ok_or(err("truncated header"))?;
                let runs_off = usize::from(u16_at(a, 0x20).ok_or(err("truncated header"))?);
                let compression_unit = u16_at(a, 0x22).ok_or(err("truncated header"))?;
                let allocated = u64_at(a, 0x28).ok_or(err("truncated header"))?;
                let real = u64_at(a, 0x30).ok_or(err("truncated header"))?;
                let initialized = u64_at(a, 0x38).ok_or(err("truncated header"))?;
                let has_total = flags & (ATTR_FLAG_COMPRESSED | ATTR_FLAG_SPARSE) != 0;
                let total_allocated = if has_total && runs_off >= 0x48 {
                    u64_at(a, 0x40)
                } else {
                    None
                };
                if runs_off < 0x40 || runs_off > len {
                    return Err(err("runlist offset outside attribute"));
                }
                if start_vcn > last_vcn.wrapping_add(1) {
                    return Err(err("start VCN after last VCN"));
                }
                AttrForm::NonResident(NonResident {
                    start_vcn,
                    last_vcn,
                    runlist: &a[runs_off..],
                    compression_unit,
                    allocated,
                    real,
                    initialized,
                    total_allocated,
                })
            }
            _ => return Err(err("non-resident flag is neither 0 nor 1")),
        };
        self.pos = attr_end;
        Ok(Some(Attr {
            type_code,
            offset: off,
            flags,
            id,
            name,
            form,
        }))
    }
}

impl<'a> Iterator for AttrIter<'a> {
    type Item = Result<Attr<'a>, RecordError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.parse_one() {
            Ok(Some(a)) => Some(Ok(a)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

/// Decoded `$STANDARD_INFORMATION`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StdInfo {
    /// The four timestamps.
    pub times: Times,
    /// Win32 file attributes (`FILE_ATTRIBUTE_*`).
    pub attributes: u32,
}

/// Decodes a `$STANDARD_INFORMATION` value (NTFS 1.2 or 3.x layout).
///
/// # Errors
///
/// The value is shorter than the 0x24 bytes that hold times and attributes.
pub fn parse_std_info(v: &[u8]) -> Result<StdInfo, &'static str> {
    let t = |o| {
        u64_at(v, o)
            .map(FileTime)
            .ok_or("$STANDARD_INFORMATION too short")
    };
    Ok(StdInfo {
        times: Times {
            created: t(0x00)?,
            modified: t(0x08)?,
            changed: t(0x10)?,
            accessed: t(0x18)?,
        },
        attributes: u32_at(v, 0x20).ok_or("$STANDARD_INFORMATION too short")?,
    })
}

/// Decoded `$FILE_NAME`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileName {
    /// Parent directory reference.
    pub parent: FileRef,
    /// Name, raw UTF-16.
    pub name: WideName,
    /// Namespace (`NS_*`).
    pub namespace: u8,
    /// `$FILE_NAME` timestamps (often stale; kept for forensics).
    pub times: Times,
    /// Allocated size as of the last directory-entry update (often stale).
    pub allocated: u64,
    /// Real size as of the last directory-entry update (often stale).
    pub logical: u64,
    /// File attribute flags copied into the directory entry.
    pub flags: u32,
}

/// Decodes a `$FILE_NAME` value.
///
/// # Errors
///
/// The value is too short for its header or its declared name length.
pub fn parse_file_name(v: &[u8]) -> Result<FileName, &'static str> {
    const SHORT: &str = "$FILE_NAME too short";
    let t = |o| u64_at(v, o).map(FileTime).ok_or(SHORT);
    let name_len = usize::from(u8_at(v, 0x40).ok_or(SHORT)?);
    Ok(FileName {
        parent: FileRef(u64_at(v, 0).ok_or(SHORT)?),
        times: Times {
            created: t(0x08)?,
            modified: t(0x10)?,
            changed: t(0x18)?,
            accessed: t(0x20)?,
        },
        allocated: u64_at(v, 0x28).ok_or(SHORT)?,
        logical: u64_at(v, 0x30).ok_or(SHORT)?,
        flags: u32_at(v, 0x38).ok_or(SHORT)?,
        namespace: u8_at(v, 0x41).ok_or(SHORT)?,
        name: WideName::from_units(utf16_at(v, 0x42, name_len).ok_or("$FILE_NAME name truncated")?),
    })
}

/// One `$ATTRIBUTE_LIST` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrListEntry {
    /// Attribute type code.
    pub type_code: u32,
    /// Attribute name, raw UTF-16.
    pub name: Vec<u16>,
    /// First VCN of the attribute instance (0 for resident attributes).
    pub start_vcn: u64,
    /// Record holding the attribute instance.
    pub holder: FileRef,
    /// Attribute instance id within the holding record.
    pub id: u16,
}

/// Minimum size of an attribute list entry.
const ATTR_LIST_ENTRY_MIN: usize = 0x1A;

/// Decodes an `$ATTRIBUTE_LIST` value.
///
/// Trailing bytes too short to hold an entry are ignored.
///
/// # Errors
///
/// An entry's length is below the minimum or runs past the value, or its name
/// lies outside the entry.
pub fn parse_attr_list(v: &[u8]) -> Result<Vec<AttrListEntry>, &'static str> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while v.len().saturating_sub(pos) >= ATTR_LIST_ENTRY_MIN {
        let e = &v[pos..];
        let type_code = u32_at(e, 0).ok_or("truncated entry")?;
        if type_code == 0 || type_code == AT_END {
            break;
        }
        let len = usize::from(u16_at(e, 4).ok_or("truncated entry")?);
        if len < ATTR_LIST_ENTRY_MIN || len > e.len() {
            return Err("attribute list entry length is invalid");
        }
        let e = &e[..len];
        let name_len = usize::from(e[6]);
        let name_off = usize::from(e[7]);
        let name = if name_len == 0 {
            Vec::new()
        } else {
            utf16_at(e, name_off, name_len).ok_or("attribute list name outside entry")?
        };
        out.push(AttrListEntry {
            type_code,
            name,
            start_vcn: u64_at(e, 0x08).ok_or("truncated entry")?,
            holder: FileRef(u64_at(e, 0x10).ok_or("truncated entry")?),
            id: u16_at(e, 0x18).ok_or("truncated entry")?,
        });
        pos += len;
    }
    Ok(out)
}

/// Decodes a reparse buffer: the tag, and for symlinks and mount points the
/// display target (print name, else substitute name).
///
/// A target that cannot be decoded is reported as `None`; only a buffer too
/// short for the tag is an error.
///
/// # Errors
///
/// The buffer is shorter than its 8-byte header.
///
/// # Example
///
/// ```
/// use strata_ntfs::parse_reparse;
/// let r = parse_reparse(&0x8000_0017u32.to_le_bytes().iter().chain(&[0u8; 4]).copied().collect::<Vec<_>>()).unwrap();
/// assert_eq!(r.tag, 0x8000_0017);
/// assert_eq!(r.target, None);
/// ```
pub fn parse_reparse(v: &[u8]) -> Result<Reparse, &'static str> {
    let tag = u32_at(v, 0).ok_or("reparse buffer too short")?;
    if v.len() < 8 {
        return Err("reparse buffer too short");
    }
    let path_base = match tag {
        win32::IO_REPARSE_TAG_MOUNT_POINT => Some(16),
        // Symlinks carry a 4-byte Flags field before the path buffer.
        win32::IO_REPARSE_TAG_SYMLINK => Some(20),
        _ => None,
    };
    let target = path_base.and_then(|base| {
        let name = |off_at, len_at| -> Option<Vec<u16>> {
            let off = usize::from(u16_at(v, off_at)?);
            let len = usize::from(u16_at(v, len_at)?);
            if len % 2 != 0 {
                return None;
            }
            utf16_at(v, base + off, len / 2)
        };
        let print = name(12, 14).filter(|n| !n.is_empty());
        print
            .or_else(|| name(8, 10).filter(|n| !n.is_empty()))
            .map(WideName::from_units)
    });
    Ok(Reparse { tag, target })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_comparison() {
        let raw: Vec<u8> = "$I30".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert!(name_bytes_eq(&raw, "$I30"));
        assert!(!name_bytes_eq(&raw, "$I3"));
        assert!(!name_bytes_eq(&raw, "$i30"));
        assert!(name_bytes_eq(&[], ""));
    }

    #[test]
    fn attr_iter_rejects_overlong_and_missing_end() {
        let mut rec = vec![0u8; 64];
        rec[0..4].copy_from_slice(&AT_DATA.to_le_bytes());
        rec[4..8].copy_from_slice(&1000u32.to_le_bytes());
        let items: Vec<_> = AttrIter::new(&rec, 0, 64).collect();
        assert_eq!(items.len(), 1);
        assert!(items[0].is_err());

        let rec = [0x80u8, 0, 0, 0];
        let items: Vec<_> = AttrIter::new(&rec, 0, 4).collect();
        assert!(items[0].is_err());

        let rec = AT_END.to_le_bytes();
        assert_eq!(AttrIter::new(&rec, 0, 4).count(), 0);
    }

    #[test]
    fn std_info_and_file_name_bounds() {
        assert!(parse_std_info(&[0u8; 0x23]).is_err());
        let si = parse_std_info(&[1u8; 0x30]).unwrap();
        assert_eq!(si.attributes, 0x0101_0101);
        let mut fname = vec![0u8; 0x42];
        fname[0x40] = 2;
        assert!(parse_file_name(&fname).is_err());
        fname.extend_from_slice(&[b'a', 0, b'b', 0]);
        fname[0x41] = NS_WIN32;
        assert_eq!(
            parse_file_name(&fname).unwrap().name.to_string_lossy(),
            "ab"
        );
    }

    #[test]
    fn attr_list_rejects_bad_lengths() {
        let mut e = vec![0u8; 0x20];
        e[0..4].copy_from_slice(&AT_DATA.to_le_bytes());
        e[4..6].copy_from_slice(&0x10u16.to_le_bytes());
        assert!(parse_attr_list(&e).is_err());
        e[4..6].copy_from_slice(&0x40u16.to_le_bytes());
        assert!(parse_attr_list(&e).is_err());
        e[4..6].copy_from_slice(&0x20u16.to_le_bytes());
        e[6] = 4;
        e[7] = 0x1A;
        assert!(parse_attr_list(&e).is_err());
        e[6] = 3;
        assert_eq!(parse_attr_list(&e).unwrap()[0].name.len(), 3);
    }

    #[test]
    fn reparse_targets() {
        let target: Vec<u8> = r"C:\t".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut v = Vec::new();
        v.extend_from_slice(&win32::IO_REPARSE_TAG_SYMLINK.to_le_bytes());
        v.extend_from_slice(&[0; 4]);
        v.extend_from_slice(&0u16.to_le_bytes()); // subst off
        v.extend_from_slice(&(target.len() as u16).to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes()); // print off (empty)
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // flags
        v.extend_from_slice(&target);
        let r = parse_reparse(&v).unwrap();
        assert_eq!(r.target.unwrap().to_string_lossy(), r"C:\t");
        // Truncated path buffer: tag kept, target dropped.
        let r = parse_reparse(&v[..22]).unwrap();
        assert_eq!(r.target, None);
        assert!(parse_reparse(&v[..6]).is_err());
    }
}
