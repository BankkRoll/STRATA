//! Bounds-checked parsers for the variable-length buffers Windows returns:
//! directory-information records, stream-information records and reparse
//! data. They are pure byte-slice code (no `unsafe`) and never panic on
//! malformed input; a truncated or inconsistent buffer just ends the parse.

use strata_core::{FileTime, Times, win32};

/// One directory entry as reported by a listing call, before it becomes a
/// [`strata_core::ScanRecord`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawEntry {
    /// Name within the directory (raw UTF-16).
    pub name: Vec<u16>,
    /// `FILE_ATTRIBUTE_*` bits.
    pub attributes: u32,
    /// Reparse tag; 0 unless `FILE_ATTRIBUTE_REPARSE_POINT` is set.
    pub reparse_tag: u32,
    /// Timestamps (`changed` is 0 for `FindFirstFileExW`).
    pub times: Times,
    /// End of file of the unnamed stream.
    pub logical: u64,
    /// Allocation size, when the listing reports it.
    pub allocated: Option<u64>,
    /// Filesystem file id, when the listing reports a usable one.
    pub file_id: Option<u128>,
}

impl RawEntry {
    /// Whether the entry is a directory.
    pub fn is_dir(&self) -> bool {
        self.attributes & win32::FILE_ATTRIBUTE_DIRECTORY != 0
    }
}

/// Directory-information record layouts accepted by
/// `GetFileInformationByHandleEx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirInfoClass {
    /// `FILE_ID_EXTD_DIR_INFO`: 128-bit id and an explicit reparse tag.
    IdExtd,
    /// `FILE_ID_BOTH_DIR_INFO`: 64-bit id; reparse tag in `EaSize`.
    IdBoth,
    /// `FILE_FULL_DIR_INFO`: no id; reparse tag in `EaSize`.
    Full,
}

impl DirInfoClass {
    /// Byte offset of `FileName` in the record.
    const fn name_offset(self) -> usize {
        match self {
            Self::IdExtd => 88,
            Self::IdBoth => 104,
            Self::Full => 68,
        }
    }
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

fn u128_at(b: &[u8], off: usize) -> Option<u128> {
    Some(u128::from_le_bytes(b.get(off..off + 16)?.try_into().ok()?))
}

/// Reads `len_bytes` of UTF-16 at `off`. Odd lengths drop the final byte.
fn utf16_at(b: &[u8], off: usize, len_bytes: usize) -> Option<Vec<u16>> {
    let raw = b.get(off..off.checked_add(len_bytes)?)?;
    Some(
        raw.chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect(),
    )
}

/// Negative `LARGE_INTEGER` sizes and times are clamped to 0.
fn non_negative(v: u64) -> u64 {
    if (v as i64) < 0 { 0 } else { v }
}

fn is_dot_or_dotdot(name: &[u16]) -> bool {
    let dot = u16::from(b'.');
    name == [dot] || name == [dot, dot]
}

/// Ids that are absent or are the "no id" sentinel (`FILE_INVALID_FILE_ID`).
fn usable_id(id: u128) -> Option<u128> {
    (id != 0 && id != u128::MAX && id != u128::from(u64::MAX)).then_some(id)
}

/// Iterates `NextEntryOffset`-chained records in `buf`, calling `f` with the
/// slice starting at each record. Stops at the first offset that would loop,
/// leave the buffer, or misalign.
fn for_each_record(buf: &[u8], mut f: impl FnMut(&[u8], usize)) {
    let mut off = 0usize;
    while off < buf.len() {
        f(buf, off);
        let Some(next) = u32_at(buf, off) else { return };
        if next == 0 {
            return;
        }
        match off.checked_add(next as usize) {
            Some(n) if n > off => off = n,
            _ => return,
        }
    }
}

/// Parses one buffer of directory-information records, appending entries to
/// `out` and skipping `.` and `..`.
pub(crate) fn parse_dir_info(buf: &[u8], class: DirInfoClass, out: &mut Vec<RawEntry>) {
    for_each_record(buf, |b, off| {
        if let Some(e) = parse_one_dir_info(b, off, class) {
            out.push(e);
        }
    });
}

fn parse_one_dir_info(b: &[u8], off: usize, class: DirInfoClass) -> Option<RawEntry> {
    let name_len = u32_at(b, off + 60)? as usize;
    let name = utf16_at(b, off.checked_add(class.name_offset())?, name_len)?;
    if name.is_empty() || is_dot_or_dotdot(&name) {
        return None;
    }
    let attributes = u32_at(b, off + 56)?;
    let ea_size = u32_at(b, off + 64)?;
    let reparse_tag = if attributes & win32::FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        0
    } else if class == DirInfoClass::IdExtd {
        u32_at(b, off + 68)?
    } else {
        // NOTE: for the older classes NTFS reports the reparse tag in EaSize
        // when the reparse attribute is set (a file cannot have both).
        ea_size
    };
    let file_id = match class {
        DirInfoClass::IdExtd => usable_id(u128_at(b, off + 72)?),
        DirInfoClass::IdBoth => usable_id(u128::from(u64_at(b, off + 96)?)),
        DirInfoClass::Full => None,
    };
    let time = |o: usize| u64_at(b, off + o).map(|v| FileTime(non_negative(v)));
    Some(RawEntry {
        name,
        attributes,
        reparse_tag,
        times: Times {
            created: time(8)?,
            accessed: time(16)?,
            modified: time(24)?,
            changed: time(32)?,
        },
        logical: non_negative(u64_at(b, off + 40)?),
        allocated: Some(non_negative(u64_at(b, off + 48)?)),
        file_id,
    })
}

/// One named data stream from `FILE_STREAM_INFO`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StreamEntry {
    /// Stream name without the leading `:` and the `:$DATA` suffix.
    pub name: Vec<u16>,
    /// Logical size.
    pub logical: u64,
    /// Allocated size.
    pub allocated: u64,
}

/// Parses `FILE_STREAM_INFO` records, returning named streams only (the
/// unnamed `::$DATA` stream is the file's content and is skipped).
pub(crate) fn parse_streams(buf: &[u8]) -> Vec<StreamEntry> {
    let mut out = Vec::new();
    for_each_record(buf, |b, off| {
        let Some(name_len) = u32_at(b, off + 4) else {
            return;
        };
        let (Some(logical), Some(allocated), Some(raw)) = (
            u64_at(b, off + 8),
            u64_at(b, off + 16),
            utf16_at(b, off + 24, name_len as usize),
        ) else {
            return;
        };
        if let Some(name) = stream_display_name(&raw) {
            out.push(StreamEntry {
                name,
                logical: non_negative(logical),
                allocated: non_negative(allocated),
            });
        }
    });
    out
}

/// `:name:$DATA` → `name`; `::$DATA` → `None`.
fn stream_display_name(raw: &[u16]) -> Option<Vec<u16>> {
    let colon = u16::from(b':');
    let rest = raw.strip_prefix(&[colon]).unwrap_or(raw);
    let suffix: Vec<u16> = ":$DATA".encode_utf16().collect();
    let name = rest.strip_suffix(suffix.as_slice()).unwrap_or(rest);
    (!name.is_empty()).then(|| name.to_vec())
}

/// `WofCompressedData`, the named stream holding a WOF file's real bytes.
pub(crate) fn is_wof_stream(name: &[u16]) -> bool {
    name.iter().copied().eq("WofCompressedData".encode_utf16())
}

/// Extracts the display target from a `REPARSE_DATA_BUFFER`.
///
/// Returns `(tag, target)`. The target is the print name when present, else
/// the substitute name with its `\??\` prefix rewritten (`\??\C:\x` becomes
/// `C:\x`; `\??\Volume{..}` becomes `\\?\Volume{..}`). Tags other than
/// symlink and mount point have no target.
pub(crate) fn parse_reparse_target(buf: &[u8]) -> Option<(u32, Option<Vec<u16>>)> {
    let tag = u32_at(buf, 0)?;
    let path_base = match tag {
        win32::IO_REPARSE_TAG_MOUNT_POINT => 16,
        win32::IO_REPARSE_TAG_SYMLINK => 20,
        _ => return Some((tag, None)),
    };
    let field = |o: usize| u16_at(buf, o).map(usize::from);
    let (sub_off, sub_len, print_off, print_len) = (field(8)?, field(10)?, field(12)?, field(14)?);
    let print = utf16_at(buf, path_base + print_off, print_len).unwrap_or_default();
    if !print.is_empty() {
        return Some((tag, Some(print)));
    }
    let sub = utf16_at(buf, path_base + sub_off, sub_len).unwrap_or_default();
    if sub.is_empty() {
        return Some((tag, None));
    }
    let nt_prefix: Vec<u16> = r"\??\".encode_utf16().collect();
    let target = match sub.strip_prefix(nt_prefix.as_slice()) {
        Some(rest) if rest.starts_with(&"Volume{".encode_utf16().collect::<Vec<_>>()) => {
            let mut v: Vec<u16> = r"\\?\".encode_utf16().collect();
            v.extend_from_slice(rest);
            v
        }
        Some(rest) => rest.to_vec(),
        None => sub,
    };
    Some((tag, Some(target)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn put(b: &mut [u8], off: usize, v: &[u8]) {
        b[off..off + v.len()].copy_from_slice(v);
    }

    fn extd_record(name: &[u16], attrs: u32, tag: u32, id: u128, next: u32) -> Vec<u8> {
        let mut b = vec![0u8; 88 + name.len() * 2];
        put(&mut b, 0, &next.to_le_bytes());
        put(&mut b, 8, &100u64.to_le_bytes());
        put(&mut b, 16, &200u64.to_le_bytes());
        put(&mut b, 24, &300u64.to_le_bytes());
        put(&mut b, 32, &400u64.to_le_bytes());
        put(&mut b, 40, &5000u64.to_le_bytes());
        put(&mut b, 48, &8192u64.to_le_bytes());
        put(&mut b, 56, &attrs.to_le_bytes());
        put(&mut b, 60, &((name.len() * 2) as u32).to_le_bytes());
        put(&mut b, 68, &tag.to_le_bytes());
        put(&mut b, 72, &id.to_le_bytes());
        for (i, u) in name.iter().enumerate() {
            put(&mut b, 88 + i * 2, &u.to_le_bytes());
        }
        b
    }

    #[test]
    fn parses_chained_extd_records_and_skips_dots() {
        let mut buf = extd_record(&w("."), 0x10, 0, 1, 96);
        buf.resize(96, 0);
        let mut second = extd_record(&w("a.txt"), 0x20, 0xDEAD, 0x42, 104);
        second.resize(104, 0);
        buf.extend(second);
        buf.extend(extd_record(
            &w("link"),
            win32::FILE_ATTRIBUTE_REPARSE_POINT | 0x10,
            win32::IO_REPARSE_TAG_MOUNT_POINT,
            0x43,
            0,
        ));
        let mut out = Vec::new();
        parse_dir_info(&buf, DirInfoClass::IdExtd, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, w("a.txt"));
        assert_eq!(out[0].reparse_tag, 0, "tag ignored without the attribute");
        assert_eq!(out[0].logical, 5000);
        assert_eq!(out[0].allocated, Some(8192));
        assert_eq!(out[0].file_id, Some(0x42));
        assert_eq!(out[0].times.changed, FileTime(400));
        assert_eq!(out[1].reparse_tag, win32::IO_REPARSE_TAG_MOUNT_POINT);
        assert!(out[1].is_dir());
    }

    #[test]
    fn truncated_name_is_dropped() {
        let mut buf = extd_record(&w("abcdef"), 0x20, 0, 7, 0);
        buf.truncate(buf.len() - 2);
        let mut out = Vec::new();
        parse_dir_info(&buf, DirInfoClass::IdExtd, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn sentinel_ids_are_unusable() {
        assert_eq!(usable_id(0), None);
        assert_eq!(usable_id(u128::MAX), None);
        assert_eq!(usable_id(u128::from(u64::MAX)), None);
        assert_eq!(usable_id(5), Some(5));
    }

    #[test]
    fn stream_names() {
        let rec = |name: &str, size: u64, alloc: u64, next: u32| {
            let n = w(name);
            let mut b = vec![0u8; 24 + n.len() * 2];
            put(&mut b, 0, &next.to_le_bytes());
            put(&mut b, 4, &((n.len() * 2) as u32).to_le_bytes());
            put(&mut b, 8, &size.to_le_bytes());
            put(&mut b, 16, &alloc.to_le_bytes());
            for (i, u) in n.iter().enumerate() {
                put(&mut b, 24 + i * 2, &u.to_le_bytes());
            }
            b.resize(b.len().next_multiple_of(8), 0);
            b
        };
        let mut buf = rec("::$DATA", 10, 4096, 0);
        let first_len = buf.len() as u32;
        put(&mut buf, 0, &first_len.to_le_bytes());
        buf.extend(rec(":Zone.Identifier:$DATA", 26, 0, 0));
        let s = parse_streams(&buf);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, w("Zone.Identifier"));
        assert_eq!((s[0].logical, s[0].allocated), (26, 0));
        assert!(is_wof_stream(&w("WofCompressedData")));
        assert!(!is_wof_stream(&w("Zone.Identifier")));
    }

    fn reparse_buf(tag: u32, sub: &str, print: &str) -> Vec<u8> {
        let (sub, print) = (w(sub), w(print));
        let base = if tag == win32::IO_REPARSE_TAG_SYMLINK {
            20
        } else {
            16
        };
        let mut b = vec![0u8; base + (sub.len() + print.len()) * 2];
        put(&mut b, 0, &tag.to_le_bytes());
        put(&mut b, 8, &0u16.to_le_bytes());
        put(&mut b, 10, &((sub.len() * 2) as u16).to_le_bytes());
        put(&mut b, 12, &((sub.len() * 2) as u16).to_le_bytes());
        put(&mut b, 14, &((print.len() * 2) as u16).to_le_bytes());
        for (i, u) in sub.iter().chain(print.iter()).enumerate() {
            put(&mut b, base + i * 2, &u.to_le_bytes());
        }
        b
    }

    #[test]
    fn reparse_targets() {
        let j = reparse_buf(win32::IO_REPARSE_TAG_MOUNT_POINT, r"\??\C:\target", "");
        assert_eq!(
            parse_reparse_target(&j),
            Some((win32::IO_REPARSE_TAG_MOUNT_POINT, Some(w(r"C:\target"))))
        );
        let v = reparse_buf(win32::IO_REPARSE_TAG_MOUNT_POINT, r"\??\Volume{abc}\", "");
        assert_eq!(
            parse_reparse_target(&v).unwrap().1,
            Some(w(r"\\?\Volume{abc}\"))
        );
        let s = reparse_buf(win32::IO_REPARSE_TAG_SYMLINK, r"\??\C:\x", r"C:\x");
        assert_eq!(parse_reparse_target(&s).unwrap().1, Some(w(r"C:\x")));
        let other = 0x8000_0017u32.to_le_bytes();
        assert_eq!(parse_reparse_target(&other), Some((0x8000_0017, None)));
        assert_eq!(parse_reparse_target(&[1, 2]), None);
    }

    proptest! {
        #[test]
        fn dir_info_never_panics(buf in proptest::collection::vec(any::<u8>(), 0..600)) {
            let mut out = Vec::new();
            for class in [DirInfoClass::IdExtd, DirInfoClass::IdBoth, DirInfoClass::Full] {
                parse_dir_info(&buf, class, &mut out);
            }
        }

        #[test]
        fn streams_and_reparse_never_panic(buf in proptest::collection::vec(any::<u8>(), 0..600)) {
            let _ = parse_streams(&buf);
            let _ = parse_reparse_target(&buf);
        }

        #[test]
        fn reparse_with_valid_tags_never_panics(
            tag in prop_oneof![
                Just(win32::IO_REPARSE_TAG_SYMLINK),
                Just(win32::IO_REPARSE_TAG_MOUNT_POINT)
            ],
            rest in proptest::collection::vec(any::<u8>(), 0..300),
        ) {
            let mut buf = tag.to_le_bytes().to_vec();
            buf.extend(rest);
            let _ = parse_reparse_target(&buf);
        }
    }
}
