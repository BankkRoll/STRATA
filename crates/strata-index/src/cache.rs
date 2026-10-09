//! Per-volume index cache file (SPEC §9.1, §10.3).
//!
//! On launch the app loads the cache and catches up from the USN journal
//! instead of rescanning. Any problem (bad magic, other version, checksum
//! mismatch, structural inconsistency) makes [`Index::load`] fail with a
//! [`CacheError`], and the caller falls back to a rescan.
//!
//! # Format (version 1, little-endian)
//!
//! ```text
//! offset size  field
//!      0    8  magic "STRATIDX"
//!      8    4  format version
//!     12    4  flags: bit 0 lite, bit 1 split hardlinks
//!     16    8  volume serial
//!     24    8  USN journal id
//!     32    8  last applied USN (i64)
//!     40   16  build version (crate version, NUL padded)
//!     56    8  slot count
//!     64    8  live entry count
//!     72    4  base_len         76  4  base_dirs
//!     80    4  root             84  4  orphans node
//!     88    4  metadata node    92  4  section count
//!     96    8  "now" (FILETIME) used for suspicious-time flags
//!    104   16  reserved (zero)
//!    120    8  xxh3-64 of bytes 0..120 and the section table
//!    128 32*n  section table: { id u32, reserved u32, offset u64, length u64, xxh3-64 u64 }
//!      …       sections, each starting on an 8-byte boundary
//! ```
//!
//! Sections are the raw index columns (`u16`/`u32`/`u64` arrays), the name
//! buffer and its sampled offsets, the directory rows and CSR child array,
//! and the small side tables encoded as flat integer arrays. Columns are
//! stored exactly as they live in memory on little-endian targets, so a
//! future loader can map them instead of copying; this loader copies (about
//! 100 ms for a few million entries) and validates every index and offset
//! before use, so a corrupt file can never cause a panic or an
//! out-of-bounds access later.

use std::io::Write;
use std::path::Path;

use hashbrown::HashMap;
use strata_core::{EntryFlags, FileTime};
use xxhash_rust::xxh3::xxh3_64;

use crate::ext::{EXT_OVERFLOW, ExtTable};
use crate::index::{Columns, DEAD, Index, IndexOptions, NONE, Rows, VolumeInfo};
use crate::names::{self, NameStore, SAMPLE};
use crate::query::PathCache;

/// File magic.
pub const MAGIC: [u8; 8] = *b"STRATIDX";
/// Current format version.
pub const FORMAT_VERSION: u32 = 1;

const HEADER_LEN: usize = 128;
const TABLE_ENTRY: usize = 32;

/// Why a cache file could not be used.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// Reading or writing failed.
    #[error("cache I/O: {0}")]
    Io(#[from] std::io::Error),
    /// Not a Strata index cache.
    #[error("not an index cache (bad magic)")]
    BadMagic,
    /// Written by an incompatible format version.
    #[error("unsupported cache format version {0}")]
    UnsupportedVersion(u32),
    /// The file is shorter than its header or section table claims.
    #[error("cache file is truncated")]
    Truncated,
    /// A checksum did not match.
    #[error("cache section {0} failed its checksum")]
    Checksum(&'static str),
    /// Checksums passed but the content is inconsistent.
    #[error("cache content is invalid: {0}")]
    Invalid(&'static str),
}

/// Header fields, readable without loading the whole index (to decide
/// whether the cache matches the volume and journal before loading it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheHeader {
    /// Format version.
    pub version: u32,
    /// Lite index.
    pub lite: bool,
    /// Split-hardlink accounting.
    pub split_hardlinks: bool,
    /// Volume serial, journal id, last USN.
    pub volume_serial: u64,
    /// USN journal id.
    pub usn_journal_id: u64,
    /// Last applied USN.
    pub last_usn: i64,
    /// Crate version that wrote the file.
    pub build_version: String,
    /// Slot count.
    pub slots: u64,
    /// Live entries.
    pub live: u64,
}

// -----------------------------------------------------------------------------
// Section ids
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum Sec {
    Parent = 1,
    Flags,
    Owner,
    Logical,
    Allocated,
    Big,
    Mtime,
    Ctime,
    Atime,
    Mftchange,
    FileRef,
    Category,
    ExtId,
    NameBuf,
    NameSamples,
    NameMoved,
    NameMovedBits,
    NameMeta,
    Children,
    RowStart,
    RowLen,
    RowFiles,
    RowDirs,
    RowNewest,
    RowOldest,
    RowLargestA,
    RowLargestL,
    RowSubL,
    RowSubA,
    RowOwnPartial,
    Extra,
    DeltaRows,
    FreeRows,
    DeltaKeys,
    LinkCounts,
    Free,
    Detached,
    Exts,
    Prefix,
}

const ALL_SECTIONS: [Sec; 39] = [
    Sec::Parent,
    Sec::Flags,
    Sec::Owner,
    Sec::Logical,
    Sec::Allocated,
    Sec::Big,
    Sec::Mtime,
    Sec::Ctime,
    Sec::Atime,
    Sec::Mftchange,
    Sec::FileRef,
    Sec::Category,
    Sec::ExtId,
    Sec::NameBuf,
    Sec::NameSamples,
    Sec::NameMoved,
    Sec::NameMovedBits,
    Sec::NameMeta,
    Sec::Children,
    Sec::RowStart,
    Sec::RowLen,
    Sec::RowFiles,
    Sec::RowDirs,
    Sec::RowNewest,
    Sec::RowOldest,
    Sec::RowLargestA,
    Sec::RowLargestL,
    Sec::RowSubL,
    Sec::RowSubA,
    Sec::RowOwnPartial,
    Sec::Extra,
    Sec::DeltaRows,
    Sec::FreeRows,
    Sec::DeltaKeys,
    Sec::LinkCounts,
    Sec::Free,
    Sec::Detached,
    Sec::Exts,
    Sec::Prefix,
];

impl Sec {
    fn name(self) -> &'static str {
        match self {
            Self::Parent => "parent",
            Self::Flags => "flags",
            Self::Owner => "owner_app",
            Self::Logical => "logical",
            Self::Allocated => "allocated",
            Self::Big => "big_sizes",
            Self::Mtime => "mtime",
            Self::Ctime => "ctime",
            Self::Atime => "atime",
            Self::Mftchange => "mftchange",
            Self::FileRef => "file_ref",
            Self::Category => "category",
            Self::ExtId => "ext_id",
            Self::NameBuf => "names",
            Self::NameSamples => "name_samples",
            Self::NameMoved => "name_overrides",
            Self::NameMovedBits => "name_override_bits",
            Self::NameMeta => "name_meta",
            Self::Children => "children",
            Self::RowStart => "row_start",
            Self::RowLen => "row_len",
            Self::RowFiles => "row_files",
            Self::RowDirs => "row_dirs",
            Self::RowNewest => "row_newest",
            Self::RowOldest => "row_oldest",
            Self::RowLargestA => "row_largest_allocated",
            Self::RowLargestL => "row_largest_logical",
            Self::RowSubL => "row_logical",
            Self::RowSubA => "row_allocated",
            Self::RowOwnPartial => "row_own_partial",
            Self::Extra => "extra_children",
            Self::DeltaRows => "delta_rows",
            Self::FreeRows => "free_rows",
            Self::DeltaKeys => "delta_keys",
            Self::LinkCounts => "link_counts",
            Self::Free => "free_slots",
            Self::Detached => "detached",
            Self::Exts => "extensions",
            Self::Prefix => "volume_prefix",
        }
    }
}

// -----------------------------------------------------------------------------
// Little-endian array helpers
// -----------------------------------------------------------------------------

fn put_u16s(out: &mut Vec<u8>, v: &[u16]) {
    out.reserve(v.len() * 2);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
}

fn put_u32s(out: &mut Vec<u8>, v: &[u32]) {
    out.reserve(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
}

fn put_u64s(out: &mut Vec<u8>, v: &[u64]) {
    out.reserve(v.len() * 8);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
}

fn get_u16s(b: &[u8]) -> Result<Vec<u16>, CacheError> {
    if !b.len().is_multiple_of(2) {
        return Err(CacheError::Invalid("misaligned u16 section"));
    }
    Ok(b.chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect())
}

fn get_u32s(b: &[u8]) -> Result<Vec<u32>, CacheError> {
    if !b.len().is_multiple_of(4) {
        return Err(CacheError::Invalid("misaligned u32 section"));
    }
    Ok(b.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

fn get_u64s(b: &[u8]) -> Result<Vec<u64>, CacheError> {
    if !b.len().is_multiple_of(8) {
        return Err(CacheError::Invalid("misaligned u64 section"));
    }
    Ok(b.chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().expect("chunk of 8")))
        .collect())
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().expect("4 bytes"))
}

fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().expect("8 bytes"))
}

// -----------------------------------------------------------------------------
// Save
// -----------------------------------------------------------------------------

impl Index {
    /// Serializes the index to the cache format.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut sections: Vec<(Sec, Vec<u8>)> = Vec::with_capacity(ALL_SECTIONS.len());
        for s in ALL_SECTIONS {
            let mut b = Vec::new();
            self.encode_section(s, &mut b);
            sections.push((s, b));
        }
        let table_len = sections.len() * TABLE_ENTRY;
        let mut offset = (HEADER_LEN + table_len).next_multiple_of(8);
        let mut table = Vec::with_capacity(table_len);
        for (s, b) in &sections {
            table.extend_from_slice(&(*s as u32).to_le_bytes());
            table.extend_from_slice(&0u32.to_le_bytes());
            table.extend_from_slice(&(offset as u64).to_le_bytes());
            table.extend_from_slice(&(b.len() as u64).to_le_bytes());
            table.extend_from_slice(&xxh3_64(b).to_le_bytes());
            offset = (offset + b.len()).next_multiple_of(8);
        }

        let mut out = Vec::with_capacity(offset);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        let flags = u32::from(self.opts.lite) | (u32::from(self.opts.split_hardlinks) << 1);
        out.extend_from_slice(&flags.to_le_bytes());
        let v = &self.opts.volume;
        out.extend_from_slice(&v.serial.to_le_bytes());
        out.extend_from_slice(&v.usn_journal_id.to_le_bytes());
        out.extend_from_slice(&v.last_usn.to_le_bytes());
        let mut ver = [0u8; 16];
        let pkg = env!("CARGO_PKG_VERSION").as_bytes();
        ver[..pkg.len().min(16)].copy_from_slice(&pkg[..pkg.len().min(16)]);
        out.extend_from_slice(&ver);
        out.extend_from_slice(&(self.col.len() as u64).to_le_bytes());
        out.extend_from_slice(&u64::from(self.live).to_le_bytes());
        for x in [
            self.base_len,
            self.base_dirs,
            self.root,
            self.orphans,
            self.metadata,
            sections.len() as u32,
        ] {
            out.extend_from_slice(&x.to_le_bytes());
        }
        out.extend_from_slice(&self.opts.now.0.to_le_bytes());
        out.extend_from_slice(&[0u8; 16]);
        debug_assert_eq!(out.len(), 120);
        let mut h = out.clone();
        h.extend_from_slice(&table);
        out.extend_from_slice(&xxh3_64(&h).to_le_bytes());
        out.extend_from_slice(&table);
        for (_, b) in &sections {
            out.resize(out.len().next_multiple_of(8), 0);
            out.extend_from_slice(b);
        }
        out
    }

    /// Writes the cache to `path` atomically (temporary file, then rename).
    ///
    /// # Errors
    ///
    /// [`CacheError::Io`] if writing or renaming fails.
    pub fn save(&self, path: &Path) -> Result<(), CacheError> {
        let tmp = path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&self.to_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Loads a cache written by [`Index::save`].
    ///
    /// # Errors
    ///
    /// Any [`CacheError`]; the caller should rescan.
    pub fn load(path: &Path) -> Result<Self, CacheError> {
        Self::from_bytes(&std::fs::read(path)?)
    }

    fn encode_section(&self, s: Sec, b: &mut Vec<u8>) {
        let c = &self.col;
        match s {
            Sec::Parent => put_u32s(b, &c.parent),
            Sec::Flags => put_u32s(b, &c.flags),
            Sec::Owner => put_u32s(b, &c.owner_app),
            Sec::Logical => put_u32s(b, &c.logical),
            Sec::Allocated => put_u32s(b, &c.allocated),
            Sec::Big => {
                let mut v: Vec<(&u32, &[u64; 2])> = c.big.iter().collect();
                v.sort_unstable();
                for (id, [l, a]) in v {
                    put_u64s(b, &[u64::from(*id), *l, *a]);
                }
            }
            Sec::Mtime => put_u32s(b, &c.mtime),
            Sec::Ctime => put_u32s(b, &c.ctime),
            Sec::Atime => put_u32s(b, &c.atime),
            Sec::Mftchange => put_u32s(b, &c.mftchange),
            Sec::FileRef => put_u64s(b, &c.file_ref),
            Sec::Category => put_u16s(b, &c.category),
            Sec::ExtId => put_u16s(b, &c.ext_id),
            Sec::NameBuf => b.extend_from_slice(&self.names.buf),
            Sec::NameSamples => put_u32s(b, &self.names.samples),
            Sec::NameMoved => put_pairs32(b, &self.names.moved),
            Sec::NameMovedBits => put_u64s(b, &self.names.moved_bits),
            Sec::NameMeta => put_u64s(b, &[u64::from(self.names.base_len), self.names.garbage]),
            Sec::Children => put_u32s(b, &self.children),
            Sec::RowStart => put_u32s(b, &self.rows.start),
            Sec::RowLen => put_u32s(b, &self.rows.len),
            Sec::RowFiles => put_u32s(b, &self.rows.files),
            Sec::RowDirs => put_u32s(b, &self.rows.dirs),
            Sec::RowNewest => put_u32s(b, &self.rows.newest),
            Sec::RowOldest => put_u32s(b, &self.rows.oldest),
            Sec::RowLargestA => put_u32s(b, &self.rows.largest_a),
            Sec::RowLargestL => put_u32s(b, &self.rows.largest_l),
            Sec::RowSubL => put_u64s(b, &self.rows.sub_l),
            Sec::RowSubA => put_u64s(b, &self.rows.sub_a),
            Sec::RowOwnPartial => put_u64s(b, &self.rows.own_partial),
            Sec::Extra => {
                let mut rows: Vec<&u32> = self.extra.keys().collect();
                rows.sort_unstable();
                for r in rows {
                    let v = &self.extra[r];
                    put_u32s(b, &[*r, v.len() as u32]);
                    put_u32s(b, v);
                }
            }
            Sec::DeltaRows => put_pairs32(b, &self.delta_rows),
            Sec::FreeRows => put_u32s(b, &self.free_rows),
            Sec::DeltaKeys => {
                let mut keys: Vec<&u64> = self.delta_keys.keys().collect();
                keys.sort_unstable();
                for k in keys {
                    for &id in &self.delta_keys[k] {
                        put_u64s(b, &[*k, u64::from(id)]);
                    }
                }
            }
            Sec::LinkCounts => {
                let mut v: Vec<(&u64, &u32)> = self.link_counts.iter().collect();
                v.sort_unstable();
                for (k, n) in v {
                    put_u64s(b, &[*k, u64::from(*n)]);
                }
            }
            Sec::Free => put_u32s(b, &self.free),
            Sec::Detached => {
                let mut v: Vec<(&u32, &u64)> = self.detached.iter().collect();
                v.sort_unstable();
                for (id, r) in v {
                    put_u64s(b, &[u64::from(*id), *r]);
                }
            }
            Sec::Exts => {
                for n in &self.exts.names {
                    names::write_prefixed(b, n);
                }
            }
            Sec::Prefix => b.extend_from_slice(self.opts.volume.prefix.as_bytes()),
        }
    }
}

fn put_pairs32(b: &mut Vec<u8>, m: &HashMap<u32, u32>) {
    let mut v: Vec<(&u32, &u32)> = m.iter().collect();
    v.sort_unstable();
    for (k, x) in v {
        put_u32s(b, &[*k, *x]);
    }
}

fn get_pairs32(b: &[u8]) -> Result<HashMap<u32, u32>, CacheError> {
    let v = get_u32s(b)?;
    if !v.len().is_multiple_of(2) {
        return Err(CacheError::Invalid("odd pair section"));
    }
    Ok(v.chunks_exact(2).map(|p| (p[0], p[1])).collect())
}

// -----------------------------------------------------------------------------
// Load
// -----------------------------------------------------------------------------

/// Reads and verifies the header and section table.
///
/// # Errors
///
/// [`CacheError::BadMagic`], [`CacheError::UnsupportedVersion`],
/// [`CacheError::Truncated`] or a header [`CacheError::Checksum`] failure.
pub fn read_header(bytes: &[u8]) -> Result<CacheHeader, CacheError> {
    parse_header(bytes).map(|(h, _)| h)
}

struct RawHeader {
    base_len: u32,
    base_dirs: u32,
    root: u32,
    orphans: u32,
    metadata: u32,
    now: u64,
    table: Vec<(u32, usize, usize, u64)>,
}

fn parse_header(bytes: &[u8]) -> Result<(CacheHeader, RawHeader), CacheError> {
    if bytes.len() < HEADER_LEN {
        return Err(if bytes.get(..8).is_some_and(|m| m != MAGIC) {
            CacheError::BadMagic
        } else {
            CacheError::Truncated
        });
    }
    if bytes[..8] != MAGIC {
        return Err(CacheError::BadMagic);
    }
    let version = u32_at(bytes, 8);
    if version != FORMAT_VERSION {
        return Err(CacheError::UnsupportedVersion(version));
    }
    let count = u32_at(bytes, 92) as usize;
    if count > 1024 {
        return Err(CacheError::Invalid("section count"));
    }
    let table_end = HEADER_LEN + count * TABLE_ENTRY;
    let table_bytes = bytes
        .get(HEADER_LEN..table_end)
        .ok_or(CacheError::Truncated)?;
    let mut h = bytes[..120].to_vec();
    h.extend_from_slice(table_bytes);
    if xxh3_64(&h) != u64_at(bytes, 120) {
        return Err(CacheError::Checksum("header"));
    }
    let table = table_bytes
        .chunks_exact(TABLE_ENTRY)
        .map(|e| {
            (
                u32_at(e, 0),
                usize::try_from(u64_at(e, 8)).unwrap_or(usize::MAX),
                usize::try_from(u64_at(e, 16)).unwrap_or(usize::MAX),
                u64_at(e, 24),
            )
        })
        .collect();
    let flags = u32_at(bytes, 12);
    let ver = &bytes[40..56];
    let ver_len = ver.iter().position(|&b| b == 0).unwrap_or(16);
    let header = CacheHeader {
        version,
        lite: flags & 1 != 0,
        split_hardlinks: flags & 2 != 0,
        volume_serial: u64_at(bytes, 16),
        usn_journal_id: u64_at(bytes, 24),
        last_usn: u64_at(bytes, 32) as i64,
        build_version: String::from_utf8_lossy(&ver[..ver_len]).into_owned(),
        slots: u64_at(bytes, 56),
        live: u64_at(bytes, 64),
    };
    let raw = RawHeader {
        base_len: u32_at(bytes, 72),
        base_dirs: u32_at(bytes, 76),
        root: u32_at(bytes, 80),
        orphans: u32_at(bytes, 84),
        metadata: u32_at(bytes, 88),
        now: u64_at(bytes, 96),
        table,
    };
    Ok((header, raw))
}

impl Index {
    /// Decodes and validates a cache image.
    ///
    /// # Errors
    ///
    /// Any [`CacheError`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CacheError> {
        let (h, raw) = parse_header(bytes)?;
        let mut secs: HashMap<u32, &[u8]> = HashMap::new();
        for &(id, off, len, sum) in &raw.table {
            let end = off.checked_add(len).ok_or(CacheError::Truncated)?;
            let data = bytes.get(off..end).ok_or(CacheError::Truncated)?;
            let name = ALL_SECTIONS
                .iter()
                .find(|s| **s as u32 == id)
                .map_or("unknown", |s| s.name());
            if xxh3_64(data) != sum {
                return Err(CacheError::Checksum(name));
            }
            secs.insert(id, data);
        }
        let sec = |s: Sec| -> Result<&[u8], CacheError> {
            secs.get(&(s as u32))
                .copied()
                .ok_or(CacheError::Invalid("missing section"))
        };
        let slots = usize::try_from(h.slots).map_err(|_| CacheError::Invalid("slot count"))?;
        if slots > crate::index::MAX_ENTRIES as usize + (1 << 16) {
            return Err(CacheError::Invalid("slot count"));
        }

        let lite = h.lite;
        let big_raw = get_u64s(sec(Sec::Big)?)?;
        if !big_raw.len().is_multiple_of(3) {
            return Err(CacheError::Invalid("big size table"));
        }
        let mut big = HashMap::new();
        for t in big_raw.chunks_exact(3) {
            let id = u32::try_from(t[0]).map_err(|_| CacheError::Invalid("big size id"))?;
            big.insert(id, [t[1], t[2]]);
        }
        let col = Columns {
            parent: get_u32s(sec(Sec::Parent)?)?,
            flags: get_u32s(sec(Sec::Flags)?)?,
            owner_app: get_u32s(sec(Sec::Owner)?)?,
            logical: get_u32s(sec(Sec::Logical)?)?,
            allocated: get_u32s(sec(Sec::Allocated)?)?,
            big,
            mtime: get_u32s(sec(Sec::Mtime)?)?,
            ctime: get_u32s(sec(Sec::Ctime)?)?,
            atime: get_u32s(sec(Sec::Atime)?)?,
            mftchange: get_u32s(sec(Sec::Mftchange)?)?,
            file_ref: get_u64s(sec(Sec::FileRef)?)?,
            category: get_u16s(sec(Sec::Category)?)?,
            ext_id: get_u16s(sec(Sec::ExtId)?)?,
            lite,
        };
        let times_len = if lite { 0 } else { slots };
        let lens_ok = [
            col.parent.len(),
            col.flags.len(),
            col.owner_app.len(),
            col.logical.len(),
            col.allocated.len(),
            col.file_ref.len(),
            col.category.len(),
            col.ext_id.len(),
        ]
        .iter()
        .all(|&l| l == slots)
            && [
                col.mtime.len(),
                col.ctime.len(),
                col.atime.len(),
                col.mftchange.len(),
            ]
            .iter()
            .all(|&l| l == times_len);
        if !lens_ok {
            return Err(CacheError::Invalid("column length"));
        }

        let meta = get_u64s(sec(Sec::NameMeta)?)?;
        if meta.len() != 2 {
            return Err(CacheError::Invalid("name metadata"));
        }
        let names = NameStore {
            buf: sec(Sec::NameBuf)?.to_vec(),
            samples: get_u32s(sec(Sec::NameSamples)?)?,
            base_len: u32::try_from(meta[0]).map_err(|_| CacheError::Invalid("name base"))?,
            moved: get_pairs32(sec(Sec::NameMoved)?)?,
            moved_bits: get_u64s(sec(Sec::NameMovedBits)?)?,
            garbage: meta[1],
        };

        let rows = Rows {
            start: get_u32s(sec(Sec::RowStart)?)?,
            len: get_u32s(sec(Sec::RowLen)?)?,
            files: get_u32s(sec(Sec::RowFiles)?)?,
            dirs: get_u32s(sec(Sec::RowDirs)?)?,
            newest: get_u32s(sec(Sec::RowNewest)?)?,
            oldest: get_u32s(sec(Sec::RowOldest)?)?,
            largest_a: get_u32s(sec(Sec::RowLargestA)?)?,
            largest_l: get_u32s(sec(Sec::RowLargestL)?)?,
            sub_l: get_u64s(sec(Sec::RowSubL)?)?,
            sub_a: get_u64s(sec(Sec::RowSubA)?)?,
            own_partial: get_u64s(sec(Sec::RowOwnPartial)?)?,
        };

        let extra_raw = get_u32s(sec(Sec::Extra)?)?;
        let mut extra: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut i = 0;
        while i < extra_raw.len() {
            let (r, n) = (
                extra_raw[i],
                *extra_raw
                    .get(i + 1)
                    .ok_or(CacheError::Invalid("extra children"))? as usize,
            );
            let ids = extra_raw
                .get(i + 2..i + 2 + n)
                .ok_or(CacheError::Invalid("extra children"))?;
            extra.insert(r, ids.to_vec());
            i += 2 + n;
        }

        let pairs64 = |s: Sec| -> Result<Vec<(u64, u64)>, CacheError> {
            let v = get_u64s(sec(s)?)?;
            if !v.len().is_multiple_of(2) {
                return Err(CacheError::Invalid("odd pair section"));
            }
            Ok(v.chunks_exact(2).map(|p| (p[0], p[1])).collect())
        };
        let mut delta_keys: HashMap<u64, Vec<u32>> = HashMap::new();
        for (k, id) in pairs64(Sec::DeltaKeys)? {
            let id = u32::try_from(id).map_err(|_| CacheError::Invalid("delta key id"))?;
            delta_keys.entry(k).or_default().push(id);
        }
        let link_counts = pairs64(Sec::LinkCounts)?
            .into_iter()
            .map(|(k, n)| (k, n as u32))
            .collect();
        let mut detached = HashMap::new();
        for (id, r) in pairs64(Sec::Detached)? {
            let id = u32::try_from(id).map_err(|_| CacheError::Invalid("detached id"))?;
            detached.insert(id, r);
        }

        let mut ext_names = Vec::new();
        let eb = sec(Sec::Exts)?;
        let mut off = 0;
        while off < eb.len() {
            let (len, hdr) = checked_len(eb, off).ok_or(CacheError::Invalid("extensions"))?;
            let s = eb
                .get(off + hdr..off + hdr + len)
                .ok_or(CacheError::Invalid("extensions"))?;
            ext_names.push(Box::from(s));
            off += hdr + len;
        }
        if ext_names.is_empty() {
            return Err(CacheError::Invalid("extensions"));
        }
        let prefix = std::str::from_utf8(sec(Sec::Prefix)?)
            .map_err(|_| CacheError::Invalid("volume prefix"))?
            .to_owned();

        let index = Self {
            col,
            names,
            exts: ExtTable::from_names(ext_names),
            rows,
            children: get_u32s(sec(Sec::Children)?)?,
            extra,
            delta_rows: get_pairs32(sec(Sec::DeltaRows)?)?,
            free_rows: get_u32s(sec(Sec::FreeRows)?)?,
            base_len: raw.base_len,
            base_dirs: raw.base_dirs,
            delta_keys,
            link_counts,
            free: get_u32s(sec(Sec::Free)?)?,
            detached,
            pending: HashMap::new(),
            root: raw.root,
            orphans: raw.orphans,
            metadata: raw.metadata,
            live: u32::try_from(h.live).map_err(|_| CacheError::Invalid("live count"))?,
            opts: IndexOptions {
                split_hardlinks: h.split_hardlinks,
                lite,
                now: FileTime(raw.now),
                volume: VolumeInfo {
                    serial: h.volume_serial,
                    usn_journal_id: h.usn_journal_id,
                    last_usn: h.last_usn,
                    prefix,
                },
            },
            path_cache: PathCache::default(),
        };
        let mut index = index;
        index.validate()?;
        index.rebuild_pending();
        Ok(index)
    }

    /// Structural checks that make every later access in-bounds.
    fn validate(&self) -> Result<(), CacheError> {
        let bad = CacheError::Invalid;
        let n = self.col.len();
        let id_ok = |v: u32| (v as usize) < n;
        let link_ok = |v: u32| v >= DEAD || id_ok(v);
        if !(self.base_dirs <= self.base_len && self.base_len as usize <= n) {
            return Err(bad("base region"));
        }
        if !id_ok(self.root) || !id_ok(self.orphans) || !id_ok(self.metadata) {
            return Err(bad("root ids"));
        }
        if self.col.parent[self.root as usize] != NONE {
            return Err(bad("root parent"));
        }
        if !self.col.parent.iter().all(|&p| link_ok(p)) {
            return Err(bad("parent ids"));
        }
        let live = self.col.parent.iter().filter(|&&p| p != DEAD).count();
        if live != self.live as usize {
            return Err(bad("live count"));
        }
        let ext_max = self.exts.names.len();
        if !self
            .col
            .ext_id
            .iter()
            .all(|&e| (e as usize) < ext_max || e == EXT_OVERFLOW)
        {
            return Err(bad("extension ids"));
        }

        // Names.
        let nm = &self.names;
        if nm.base_len != self.base_len
            || nm.samples.len() != (nm.base_len as usize).div_ceil(SAMPLE)
            || nm.moved_bits.len() != (nm.base_len as usize).div_ceil(64)
        {
            return Err(bad("name layout"));
        }
        let mut off = 0usize;
        for i in 0..nm.base_len as usize {
            if i % SAMPLE == 0 && nm.samples[i / SAMPLE] as usize != off {
                return Err(bad("name samples"));
            }
            let (len, hdr) = checked_len(&nm.buf, off).ok_or(bad("name buffer"))?;
            off = off
                .checked_add(hdr + len)
                .filter(|&e| e <= nm.buf.len())
                .ok_or(bad("name buffer"))?;
        }
        for (&id, &o) in &nm.moved {
            let (len, hdr) = checked_len(&nm.buf, o as usize).ok_or(bad("name override"))?;
            if !id_ok(id) || o as usize + hdr + len > nm.buf.len() {
                return Err(bad("name override"));
            }
        }

        // Rows and children.
        let r = &self.rows;
        let rn = r.start.len();
        let same = [
            r.len.len(),
            r.files.len(),
            r.dirs.len(),
            r.newest.len(),
            r.oldest.len(),
            r.largest_a.len(),
            r.largest_l.len(),
            r.sub_l.len(),
            r.sub_a.len(),
        ];
        if same.iter().any(|&l| l != rn)
            || r.own_partial.len() < rn.div_ceil(64)
            || (self.base_dirs as usize) > rn
        {
            return Err(bad("row lengths"));
        }
        let cl = self.children.len();
        for i in 0..rn {
            let s = r.start[i] as usize;
            let end = s.checked_add(r.len[i] as usize).ok_or(bad("row range"))?;
            if end > cl
                || !(r.largest_a[i] == NONE || id_ok(r.largest_a[i]))
                || !(r.largest_l[i] == NONE || id_ok(r.largest_l[i]))
            {
                return Err(bad("row range"));
            }
            if i < self.base_dirs as usize {
                let next = if i + 1 < self.base_dirs as usize {
                    r.start[i + 1] as usize
                } else {
                    cl
                };
                if next < end {
                    return Err(bad("row capacity"));
                }
            }
        }
        if !self.children.iter().all(|&c| id_ok(c)) {
            return Err(bad("child ids"));
        }
        for (&row, v) in &self.extra {
            if row as usize >= rn || !v.iter().all(|&c| id_ok(c)) {
                return Err(bad("extra children"));
            }
        }
        for (&id, &row) in &self.delta_rows {
            if !id_ok(id) || row as usize >= rn {
                return Err(bad("delta rows"));
            }
        }
        if !self.free_rows.iter().all(|&x| (x as usize) < rn)
            || !self.free.iter().all(|&x| id_ok(x))
            || !self.delta_keys.values().flatten().all(|&x| id_ok(x))
            || !self.detached.keys().all(|&x| id_ok(x))
        {
            return Err(bad("side tables"));
        }
        for id in 0..n as u32 {
            if self.col.parent[id as usize] == DEAD {
                continue;
            }
            if self.col.has(id, EntryFlags::DIR) && self.row_of(id).is_none() {
                return Err(bad("directory without row"));
            }
        }
        if self.row_of(self.root).is_none()
            || self.row_of(self.orphans).is_none()
            || self.row_of(self.metadata).is_none()
        {
            return Err(bad("group nodes"));
        }
        Ok(())
    }
}

/// LEB128 length at `off`, bounds-checked: `(length, prefix bytes)`.
fn checked_len(buf: &[u8], off: usize) -> Option<(usize, usize)> {
    let mut len = 0usize;
    let mut shift = 0;
    let mut i = off;
    loop {
        let b = *buf.get(i)?;
        len |= ((b & 0x7F) as usize).checked_shl(shift)?;
        i += 1;
        if b < 0x80 {
            return Some((len, i - off));
        }
        shift += 7;
        if shift > 28 {
            return None;
        }
    }
}
