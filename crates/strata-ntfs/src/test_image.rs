//! Byte-exact synthetic NTFS images for tests, benchmarks and fuzz seeds.
//!
//! Enabled by the `test-image` feature. The builder lays out a boot sector,
//! a (optionally fragmented) `$MFT` whose `$DATA` may live partly in an
//! extension record reached through an attribute list, `$MFT:$BITMAP`,
//! optional system files (records 1–11, including a real `$Bitmap`), and any
//! records the caller constructs attribute by attribute. Every multi-sector
//! record gets a correct update sequence array.
//!
//! Panics on misuse (a record that does not fit, an unreachable `$MFT`
//! extension): this is test infrastructure, and a loud failure is the point.
//!
//! # Example
//!
//! ```
//! use strata_ntfs::test_image::{Geometry, ImageBuilder, RecordBuilder, ROOT};
//! use strata_ntfs::{NtfsVolume, ScanOptions};
//!
//! let mut img = ImageBuilder::new(Geometry::default()).with_system_files();
//! img.insert(64, RecordBuilder::file(1, ROOT, "hello.txt").data("", b"hi"));
//! let vol = NtfsVolume::open(img.finish()).unwrap();
//! let rec = vol.read_record(64).unwrap().unwrap();
//! assert_eq!(rec.sizes.logical, 2);
//! assert_eq!(rec.links[0].name.to_string_lossy(), "hello.txt");
//! ```

use std::collections::BTreeMap;

use strata_core::{FileRef, FileTime, Times, win32};

use crate::attr::{
    AT_ATTRIBUTE_LIST, AT_BITMAP, AT_DATA, AT_FILE_NAME, AT_INDEX_ALLOCATION, AT_INDEX_ROOT,
    AT_REPARSE_POINT, AT_STANDARD_INFORMATION, ATTR_FLAG_COMPRESSED, ATTR_FLAG_SPARSE,
    NS_WIN32_AND_DOS,
};
use crate::fixup::FIXUP_STRIDE;
use crate::record::{RECORD_IN_USE, RECORD_IS_DIRECTORY};
use crate::runlist::{Run, encode_runlist};

/// The root directory's reference (record 5, sequence 5).
pub const ROOT: FileRef = FileRef::from_parts(5, 5);

/// `$Extend`'s reference (record 11, sequence 11).
pub const EXTEND: FileRef = FileRef::from_parts(11, 11);

/// Timestamps used by the convenience constructors: 2024-01-02 03:04:05 UTC
/// plus a per-field offset so the fields are distinguishable.
pub const DEFAULT_TIMES: Times = Times {
    created: FileTime(133_485_062_450_000_000),
    modified: FileTime(133_485_062_460_000_000),
    accessed: FileTime(133_485_062_470_000_000),
    changed: FileTime(133_485_062_480_000_000),
};

/// Volume geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// Bytes per sector (512 or 4096).
    pub sector_size: u32,
    /// Bytes per cluster (sector size .. 2 MiB).
    pub cluster_size: u32,
    /// Bytes per MFT record (512 .. 64 KiB).
    pub record_size: u32,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            sector_size: 512,
            cluster_size: 4096,
            record_size: 1024,
        }
    }
}

// -----------------------------------------------------------------------------
// Attribute values
// -----------------------------------------------------------------------------

/// Encodes a `$STANDARD_INFORMATION` value (NTFS 3.x, 0x48 bytes).
#[must_use]
pub fn std_info_value(times: Times, attributes: u32) -> Vec<u8> {
    let mut v = vec![0u8; 0x48];
    v[0x00..0x08].copy_from_slice(&times.created.0.to_le_bytes());
    v[0x08..0x10].copy_from_slice(&times.modified.0.to_le_bytes());
    v[0x10..0x18].copy_from_slice(&times.changed.0.to_le_bytes());
    v[0x18..0x20].copy_from_slice(&times.accessed.0.to_le_bytes());
    v[0x20..0x24].copy_from_slice(&attributes.to_le_bytes());
    v
}

/// Encodes a `$FILE_NAME` value.
#[must_use]
pub fn file_name_value(parent: FileRef, name: &[u16], namespace: u8, times: Times) -> Vec<u8> {
    assert!(name.len() <= 255, "file names are at most 255 UTF-16 units");
    let mut v = vec![0u8; 0x42];
    v[0x00..0x08].copy_from_slice(&parent.0.to_le_bytes());
    v[0x08..0x10].copy_from_slice(&times.created.0.to_le_bytes());
    v[0x10..0x18].copy_from_slice(&times.modified.0.to_le_bytes());
    v[0x18..0x20].copy_from_slice(&times.changed.0.to_le_bytes());
    v[0x20..0x28].copy_from_slice(&times.accessed.0.to_le_bytes());
    v[0x40] = name.len() as u8;
    v[0x41] = namespace;
    for u in name {
        v.extend_from_slice(&u.to_le_bytes());
    }
    v
}

/// Encodes a symbolic link reparse buffer (`IO_REPARSE_TAG_SYMLINK`).
#[must_use]
pub fn symlink_reparse(substitute: &str, print: &str, relative: bool) -> Vec<u8> {
    name_pair_reparse(
        win32::IO_REPARSE_TAG_SYMLINK,
        substitute,
        print,
        Some(u32::from(relative)),
    )
}

/// Encodes a junction / mount point reparse buffer (`IO_REPARSE_TAG_MOUNT_POINT`).
#[must_use]
pub fn mount_point_reparse(substitute: &str, print: &str) -> Vec<u8> {
    name_pair_reparse(win32::IO_REPARSE_TAG_MOUNT_POINT, substitute, print, None)
}

fn name_pair_reparse(tag: u32, substitute: &str, print: &str, flags: Option<u32>) -> Vec<u8> {
    let s: Vec<u8> = substitute
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let p: Vec<u8> = print.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut body = Vec::new();
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&(s.len() as u16).to_le_bytes());
    body.extend_from_slice(&((s.len() + 2) as u16).to_le_bytes());
    body.extend_from_slice(&(p.len() as u16).to_le_bytes());
    if let Some(f) = flags {
        body.extend_from_slice(&f.to_le_bytes());
    }
    body.extend_from_slice(&s);
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(&p);
    body.extend_from_slice(&[0, 0]);
    raw_reparse(tag, &body)
}

/// Encodes a reparse buffer with an arbitrary tag and payload.
#[must_use]
pub fn raw_reparse(tag: u32, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + data.len());
    v.extend_from_slice(&tag.to_le_bytes());
    v.extend_from_slice(&(data.len() as u16).to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(data);
    v
}

/// One entry for [`attr_list_value`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrListSpec {
    /// Attribute type.
    pub type_code: u32,
    /// Attribute name.
    pub name: Vec<u16>,
    /// First VCN of the instance.
    pub start_vcn: u64,
    /// Holding record.
    pub holder: FileRef,
    /// Attribute id in the holding record.
    pub id: u16,
}

impl AttrListSpec {
    /// An unnamed entry.
    #[must_use]
    pub fn new(type_code: u32, start_vcn: u64, holder: FileRef) -> Self {
        Self {
            type_code,
            name: Vec::new(),
            start_vcn,
            holder,
            id: 0,
        }
    }
}

/// Encodes an `$ATTRIBUTE_LIST` value.
#[must_use]
pub fn attr_list_value(entries: &[AttrListSpec]) -> Vec<u8> {
    let mut v = Vec::new();
    for e in entries {
        let len = (0x1A + e.name.len() * 2).next_multiple_of(8);
        let start = v.len();
        v.resize(start + len, 0);
        let b = &mut v[start..];
        b[0..4].copy_from_slice(&e.type_code.to_le_bytes());
        b[4..6].copy_from_slice(&(len as u16).to_le_bytes());
        b[6] = e.name.len() as u8;
        b[7] = 0x1A;
        b[8..16].copy_from_slice(&e.start_vcn.to_le_bytes());
        b[16..24].copy_from_slice(&e.holder.0.to_le_bytes());
        b[24..26].copy_from_slice(&e.id.to_le_bytes());
        for (i, u) in e.name.iter().enumerate() {
            b[0x1A + 2 * i..0x1C + 2 * i].copy_from_slice(&u.to_le_bytes());
        }
    }
    v
}

// -----------------------------------------------------------------------------
// Attributes and records
// -----------------------------------------------------------------------------

/// A non-resident attribute header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonResidentSpec {
    /// First VCN of this instance.
    pub start_vcn: u64,
    /// Runs of this instance, VCNs starting at `start_vcn`.
    pub runs: Vec<Run>,
    /// Allocated size field (0x28).
    pub allocated: u64,
    /// Real size field (0x30).
    pub real: u64,
    /// Initialized size field (0x38).
    pub initialized: u64,
    /// Total-allocated field (0x40); written only when `Some`.
    pub total_allocated: Option<u64>,
    /// Compression unit (0x22).
    pub compression_unit: u16,
}

impl NonResidentSpec {
    /// A plain instance at VCN 0: allocated = clusters × `cluster_size`,
    /// initialized = real.
    #[must_use]
    pub fn new(runs: Vec<Run>, real: u64, cluster_size: u32) -> Self {
        let clusters: u64 = runs.iter().map(|r| r.len).sum();
        Self {
            start_vcn: 0,
            runs,
            allocated: clusters * u64::from(cluster_size),
            real,
            initialized: real,
            total_allocated: None,
            compression_unit: 0,
        }
    }

    /// A continuation instance (VCN > 0): sizes are zero as on real volumes.
    #[must_use]
    pub fn continuation(runs: Vec<Run>) -> Self {
        Self {
            start_vcn: runs.first().map_or(0, |r| r.vcn),
            runs,
            allocated: 0,
            real: 0,
            initialized: 0,
            total_allocated: None,
            compression_unit: 0,
        }
    }

    /// Sets the total-allocated ("compressed size") field.
    #[must_use]
    pub fn total_allocated(mut self, bytes: u64) -> Self {
        self.total_allocated = Some(bytes);
        self
    }
}

/// An attribute value: resident bytes or a non-resident header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrValue {
    /// Resident value bytes.
    Resident(Vec<u8>),
    /// Non-resident header.
    NonResident(NonResidentSpec),
}

/// One attribute to place in a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrSpec {
    /// Type code.
    pub type_code: u32,
    /// Name (UTF-16).
    pub name: Vec<u16>,
    /// Attribute flags.
    pub flags: u16,
    /// Value.
    pub value: AttrValue,
}

/// Builds one MFT record attribute by attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordBuilder {
    sequence: u16,
    flags: u16,
    base: FileRef,
    link_count: u16,
    attrs: Vec<AttrSpec>,
    signature: [u8; 4],
    torn: bool,
}

impl RecordBuilder {
    /// An empty in-use record with sequence `sequence`.
    #[must_use]
    pub fn new(sequence: u16) -> Self {
        Self {
            sequence,
            flags: RECORD_IN_USE,
            base: FileRef(0),
            link_count: 0,
            attrs: Vec::new(),
            signature: *b"FILE",
            torn: false,
        }
    }

    /// A file with default times, `FILE_ATTRIBUTE_ARCHIVE`, and one
    /// Win32&DOS name. Add `$DATA` with [`Self::data`] or [`Self::data_nonresident`].
    #[must_use]
    pub fn file(sequence: u16, parent: FileRef, name: &str) -> Self {
        Self::new(sequence)
            .std_info(DEFAULT_TIMES, win32::FILE_ATTRIBUTE_ARCHIVE)
            .name(parent, name, NS_WIN32_AND_DOS)
    }

    /// A directory with default times, one Win32&DOS name and an empty
    /// resident `$I30` index root.
    #[must_use]
    pub fn dir(sequence: u16, parent: FileRef, name: &str) -> Self {
        Self::new(sequence)
            .directory()
            .std_info(DEFAULT_TIMES, 0)
            .name(parent, name, NS_WIN32_AND_DOS)
            .attr(
                AT_INDEX_ROOT,
                "$I30",
                0,
                AttrValue::Resident(vec![0u8; 0x20]),
            )
    }

    /// Sets the directory flag.
    #[must_use]
    pub fn directory(mut self) -> Self {
        self.flags |= RECORD_IS_DIRECTORY;
        self
    }

    /// Clears the in-use flag.
    #[must_use]
    pub fn free(mut self) -> Self {
        self.flags &= !RECORD_IN_USE;
        self
    }

    /// Makes this an extension record of `base`.
    #[must_use]
    pub fn extension_of(mut self, base: FileRef) -> Self {
        self.base = base;
        self
    }

    /// Writes the `BAAD` signature.
    #[must_use]
    pub fn baad(mut self) -> Self {
        self.signature = *b"BAAD";
        self
    }

    /// Writes an arbitrary signature.
    #[must_use]
    pub fn signature(mut self, sig: [u8; 4]) -> Self {
        self.signature = sig;
        self
    }

    /// Corrupts the first stride's tail after fixups are applied (torn write).
    #[must_use]
    pub fn torn(mut self) -> Self {
        self.torn = true;
        self
    }

    /// Appends an arbitrary attribute.
    #[must_use]
    pub fn attr(mut self, type_code: u32, name: &str, flags: u16, value: AttrValue) -> Self {
        self.attrs.push(AttrSpec {
            type_code,
            name: name.encode_utf16().collect(),
            flags,
            value,
        });
        self
    }

    /// Appends an attribute whose name is raw UTF-16.
    #[must_use]
    pub fn attr_units(
        mut self,
        type_code: u32,
        name: &[u16],
        flags: u16,
        value: AttrValue,
    ) -> Self {
        self.attrs.push(AttrSpec {
            type_code,
            name: name.to_vec(),
            flags,
            value,
        });
        self
    }

    /// Appends `$STANDARD_INFORMATION`.
    #[must_use]
    pub fn std_info(self, times: Times, attributes: u32) -> Self {
        self.attr(
            AT_STANDARD_INFORMATION,
            "",
            0,
            AttrValue::Resident(std_info_value(times, attributes)),
        )
    }

    /// Appends a `$FILE_NAME` (FN times = [`DEFAULT_TIMES`]) and bumps the link count.
    #[must_use]
    pub fn name(self, parent: FileRef, name: &str, namespace: u8) -> Self {
        let units: Vec<u16> = name.encode_utf16().collect();
        self.name_units(parent, &units, namespace, DEFAULT_TIMES)
    }

    /// Appends a `$FILE_NAME` with raw UTF-16 units and explicit FN times.
    #[must_use]
    pub fn name_units(
        mut self,
        parent: FileRef,
        name: &[u16],
        namespace: u8,
        times: Times,
    ) -> Self {
        self.link_count += 1;
        self.attr(
            AT_FILE_NAME,
            "",
            0,
            AttrValue::Resident(file_name_value(parent, name, namespace, times)),
        )
    }

    /// Appends a resident `$DATA` stream (`""` = unnamed).
    #[must_use]
    pub fn data(self, stream: &str, bytes: &[u8]) -> Self {
        self.attr(AT_DATA, stream, 0, AttrValue::Resident(bytes.to_vec()))
    }

    /// Appends a non-resident `$DATA` instance with attribute `flags`.
    #[must_use]
    pub fn data_nonresident(self, stream: &str, flags: u16, spec: NonResidentSpec) -> Self {
        self.attr(AT_DATA, stream, flags, AttrValue::NonResident(spec))
    }

    /// Appends a sparse `$DATA` instance (flag + total-allocated field).
    #[must_use]
    pub fn data_sparse(self, stream: &str, spec: NonResidentSpec, on_disk: u64) -> Self {
        self.data_nonresident(stream, ATTR_FLAG_SPARSE, spec.total_allocated(on_disk))
    }

    /// Appends a compressed `$DATA` instance (flag + total-allocated field,
    /// compression unit 4 = 16 clusters).
    #[must_use]
    pub fn data_compressed(self, stream: &str, mut spec: NonResidentSpec, on_disk: u64) -> Self {
        spec.compression_unit = 4;
        self.data_nonresident(stream, ATTR_FLAG_COMPRESSED, spec.total_allocated(on_disk))
    }

    /// Appends a resident `$REPARSE_POINT`.
    #[must_use]
    pub fn reparse(self, buffer: Vec<u8>) -> Self {
        self.attr(AT_REPARSE_POINT, "", 0, AttrValue::Resident(buffer))
    }

    /// Appends a non-resident `$INDEX_ALLOCATION` named `name` and its `$BITMAP`.
    #[must_use]
    pub fn index_allocation(self, name: &str, spec: NonResidentSpec) -> Self {
        self.attr(AT_INDEX_ALLOCATION, name, 0, AttrValue::NonResident(spec))
            .attr(
                AT_BITMAP,
                name,
                0,
                AttrValue::Resident(vec![1, 0, 0, 0, 0, 0, 0, 0]),
            )
    }

    /// Appends a resident `$ATTRIBUTE_LIST`.
    #[must_use]
    pub fn attr_list(self, entries: &[AttrListSpec]) -> Self {
        self.attr(
            AT_ATTRIBUTE_LIST,
            "",
            0,
            AttrValue::Resident(attr_list_value(entries)),
        )
    }

    /// Serializes the record for `geometry` at index `record`, with fixups.
    ///
    /// # Panics
    ///
    /// The attributes do not fit in one record.
    #[must_use]
    pub fn build(&self, geometry: Geometry, record: u64) -> Vec<u8> {
        let rs = geometry.record_size as usize;
        let usa_count = rs / FIXUP_STRIDE + 1;
        let first_attr = (0x30 + 2 * usa_count).next_multiple_of(8);
        let mut body = Vec::new();
        for (i, a) in self.attrs.iter().enumerate() {
            body.extend_from_slice(&encode_attr(a, i as u16));
        }
        let used = first_attr + body.len() + 8;
        assert!(
            used <= rs,
            "record {record}: {used} bytes of attributes do not fit in {rs}"
        );
        let mut b = vec![0u8; rs];
        b[0..4].copy_from_slice(&self.signature);
        b[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        b[6..8].copy_from_slice(&(usa_count as u16).to_le_bytes());
        b[0x10..0x12].copy_from_slice(&self.sequence.to_le_bytes());
        b[0x12..0x14].copy_from_slice(&self.link_count.to_le_bytes());
        b[0x14..0x16].copy_from_slice(&(first_attr as u16).to_le_bytes());
        b[0x16..0x18].copy_from_slice(&self.flags.to_le_bytes());
        b[0x18..0x1C].copy_from_slice(&(used as u32).to_le_bytes());
        b[0x1C..0x20].copy_from_slice(&(rs as u32).to_le_bytes());
        b[0x20..0x28].copy_from_slice(&self.base.0.to_le_bytes());
        b[0x28..0x2A].copy_from_slice(&(self.attrs.len() as u16).to_le_bytes());
        b[0x2C..0x30].copy_from_slice(&(record as u32).to_le_bytes());
        b[first_attr..first_attr + body.len()].copy_from_slice(&body);
        let end = first_attr + body.len();
        b[end..end + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        protect(&mut b, self.sequence.wrapping_add(1).max(1));
        if self.torn {
            b[FIXUP_STRIDE - 1] ^= 0xFF;
        }
        b
    }
}

/// Inserts update sequence protection: saves every stride tail into the
/// array at 0x30 and overwrites it with `usn`.
fn protect(b: &mut [u8], usn: u16) {
    let usa_count = b.len() / FIXUP_STRIDE + 1;
    b[0x30..0x32].copy_from_slice(&usn.to_le_bytes());
    for i in 1..usa_count {
        let tail = i * FIXUP_STRIDE - 2;
        let saved = [b[tail], b[tail + 1]];
        b[0x30 + 2 * i..0x32 + 2 * i].copy_from_slice(&saved);
        b[tail..tail + 2].copy_from_slice(&usn.to_le_bytes());
    }
}

fn encode_attr(a: &AttrSpec, id: u16) -> Vec<u8> {
    let name_bytes: Vec<u8> = a.name.iter().flat_map(|u| u.to_le_bytes()).collect();
    match &a.value {
        AttrValue::Resident(value) => {
            let name_off = 0x18usize;
            let value_off = (name_off + name_bytes.len()).next_multiple_of(8);
            let len = (value_off + value.len()).next_multiple_of(8);
            let mut b = vec![0u8; len];
            header(&mut b, a, id, 0, name_off);
            b[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
            b[0x14..0x16].copy_from_slice(&(value_off as u16).to_le_bytes());
            b[0x16] = u8::from(a.type_code == AT_FILE_NAME);
            b[name_off..name_off + name_bytes.len()].copy_from_slice(&name_bytes);
            b[value_off..value_off + value.len()].copy_from_slice(value);
            b
        }
        AttrValue::NonResident(nr) => {
            let name_off = if nr.total_allocated.is_some() {
                0x48
            } else {
                0x40
            };
            let runs_off = (name_off + name_bytes.len()).next_multiple_of(8);
            let runlist = encode_runlist(&nr.runs);
            let len = (runs_off + runlist.len()).next_multiple_of(8);
            let mut b = vec![0u8; len];
            header(&mut b, a, id, 1, name_off);
            let clusters: u64 = nr.runs.iter().map(|r| r.len).sum();
            let last_vcn = (nr.start_vcn + clusters).wrapping_sub(1);
            b[0x10..0x18].copy_from_slice(&nr.start_vcn.to_le_bytes());
            b[0x18..0x20].copy_from_slice(&last_vcn.to_le_bytes());
            b[0x20..0x22].copy_from_slice(&(runs_off as u16).to_le_bytes());
            b[0x22..0x24].copy_from_slice(&nr.compression_unit.to_le_bytes());
            b[0x28..0x30].copy_from_slice(&nr.allocated.to_le_bytes());
            b[0x30..0x38].copy_from_slice(&nr.real.to_le_bytes());
            b[0x38..0x40].copy_from_slice(&nr.initialized.to_le_bytes());
            if let Some(t) = nr.total_allocated {
                b[0x40..0x48].copy_from_slice(&t.to_le_bytes());
            }
            b[name_off..name_off + name_bytes.len()].copy_from_slice(&name_bytes);
            b[runs_off..runs_off + runlist.len()].copy_from_slice(&runlist);
            b
        }
    }
}

fn header(b: &mut [u8], a: &AttrSpec, id: u16, non_resident: u8, name_off: usize) {
    b[0..4].copy_from_slice(&a.type_code.to_le_bytes());
    let len = b.len() as u32;
    b[4..8].copy_from_slice(&len.to_le_bytes());
    b[8] = non_resident;
    b[9] = a.name.len() as u8;
    b[10..12].copy_from_slice(&(name_off as u16).to_le_bytes());
    b[12..14].copy_from_slice(&a.flags.to_le_bytes());
    b[14..16].copy_from_slice(&id.to_le_bytes());
}

/// Re-bases runs so their VCNs start at `start` and follow each other.
#[must_use]
pub fn chain_runs(start: u64, parts: &[Vec<Run>]) -> Vec<Run> {
    let mut vcn = start;
    let mut out = Vec::new();
    for r in parts.iter().flatten() {
        out.push(Run { vcn, ..*r });
        vcn += r.len;
    }
    out
}

/// A sparse run of `len` clusters at VCN 0 (re-base with [`chain_runs`]).
#[must_use]
pub fn sparse_run(len: u64) -> Vec<Run> {
    vec![Run {
        vcn: 0,
        lcn: None,
        len,
    }]
}

// -----------------------------------------------------------------------------
// Image
// -----------------------------------------------------------------------------

enum Slot {
    Built(RecordBuilder),
    Raw(Vec<u8>),
}

/// Lays out a complete NTFS image.
pub struct ImageBuilder {
    geometry: Geometry,
    records: BTreeMap<u64, Slot>,
    writes: Vec<(u64, Vec<u8>)>,
    used: Vec<(u64, u64)>,
    next_lcn: u64,
    claimed_clusters: u64,
    min_records: u64,
    mft_fragments: usize,
    mft_extension: Option<u64>,
    mft_attr_list_nonresident: bool,
    system_files: bool,
    serial: u64,
}

impl std::fmt::Debug for ImageBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageBuilder")
            .field("geometry", &self.geometry)
            .field("records", &self.records.len())
            .field("next_lcn", &self.next_lcn)
            .finish_non_exhaustive()
    }
}

/// Size of `$Boot` in bytes.
const BOOT_BYTES: u64 = 8192;

impl ImageBuilder {
    /// A builder for `geometry` with a single-fragment MFT and no system files.
    ///
    /// # Panics
    ///
    /// The geometry is not a valid NTFS geometry.
    #[must_use]
    pub fn new(geometry: Geometry) -> Self {
        let Geometry {
            sector_size: ss,
            cluster_size: cs,
            record_size: rs,
        } = geometry;
        assert!(ss.is_power_of_two() && (512..=4096).contains(&ss));
        assert!(cs.is_power_of_two() && cs >= ss && cs <= 2 * 1024 * 1024);
        assert!(rs.is_power_of_two() && (512..=65536).contains(&rs));
        let boot_clusters = BOOT_BYTES.div_ceil(u64::from(cs));
        Self {
            geometry,
            records: BTreeMap::new(),
            writes: Vec::new(),
            used: vec![(0, boot_clusters)],
            next_lcn: boot_clusters,
            claimed_clusters: 0,
            min_records: 16,
            mft_fragments: 1,
            mft_extension: None,
            mft_attr_list_nonresident: false,
            system_files: false,
            serial: 0x1234_5678_9ABC_DEF0,
        }
    }

    /// The geometry.
    #[must_use]
    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// Bytes per cluster.
    #[must_use]
    pub fn cluster_size(&self) -> u32 {
        self.geometry.cluster_size
    }

    /// Splits `$MFT:$DATA` into `n` fragments laid out in descending LCN order
    /// (so the runlist contains negative offsets), with a free cluster between
    /// each.
    #[must_use]
    pub fn mft_fragments(mut self, n: usize) -> Self {
        self.mft_fragments = n.max(1);
        self
    }

    /// Moves the second half of `$MFT:$DATA`'s runs into extension record
    /// `record`, referenced from record 0's attribute list.
    #[must_use]
    pub fn mft_data_in_extension(mut self, record: u64) -> Self {
        self.mft_extension = Some(record);
        self
    }

    /// Stores record 0's attribute list non-resident.
    #[must_use]
    pub fn mft_attr_list_nonresident(mut self) -> Self {
        self.mft_attr_list_nonresident = true;
        self
    }

    /// Ensures the MFT has at least `n` records.
    #[must_use]
    pub fn min_records(mut self, n: u64) -> Self {
        self.min_records = self.min_records.max(n);
        self
    }

    /// Declares at least `n` clusters in the boot sector. Clusters past the
    /// last written byte are not backed by image bytes, which lets tests
    /// describe huge files cheaply.
    #[must_use]
    pub fn claim_clusters(mut self, n: u64) -> Self {
        self.claimed_clusters = n;
        self
    }

    /// Adds records 1–11 (`$MFTMirr` … `$Extend`, root directory included)
    /// with consistent cluster allocations and a real `$Bitmap`.
    #[must_use]
    pub fn with_system_files(mut self) -> Self {
        self.system_files = true;
        self
    }

    /// Allocates `clusters` contiguous clusters (VCNs from 0) and marks them used.
    pub fn alloc(&mut self, clusters: u64) -> Vec<Run> {
        if clusters == 0 {
            return Vec::new();
        }
        let lcn = self.next_lcn;
        self.next_lcn += clusters;
        self.used.push((lcn, clusters));
        vec![Run {
            vcn: 0,
            lcn: Some(lcn),
            len: clusters,
        }]
    }

    /// Allocates `clusters` in `pieces` fragments in descending LCN order
    /// with one free cluster between fragments.
    pub fn alloc_fragmented(&mut self, clusters: u64, pieces: usize) -> Vec<Run> {
        let pieces = (pieces as u64).clamp(1, clusters.max(1));
        let base = clusters / pieces;
        let extra = clusters % pieces;
        let lens: Vec<u64> = (0..pieces).map(|i| base + u64::from(i < extra)).collect();
        let mut lcns = vec![0u64; lens.len()];
        for i in (0..lens.len()).rev() {
            lcns[i] = self.next_lcn;
            self.used.push((self.next_lcn, lens[i]));
            self.next_lcn += lens[i] + 1;
        }
        let mut vcn = 0;
        lens.iter()
            .zip(lcns)
            .map(|(&len, lcn)| {
                let r = Run {
                    vcn,
                    lcn: Some(lcn),
                    len,
                };
                vcn += len;
                r
            })
            .collect()
    }

    /// Writes `data` into the clusters of `runs` (sparse runs are skipped).
    pub fn write_runs(&mut self, runs: &[Run], data: &[u8]) {
        let cs = u64::from(self.geometry.cluster_size);
        let mut pos = 0usize;
        for r in runs {
            let n = usize::try_from(r.len * cs)
                .unwrap_or(usize::MAX)
                .min(data.len() - pos);
            if let Some(lcn) = r.lcn {
                self.writes.push((lcn * cs, data[pos..pos + n].to_vec()));
            }
            pos += n;
            if pos >= data.len() {
                break;
            }
        }
    }

    /// Places a record at index `n`.
    pub fn insert(&mut self, n: u64, record: RecordBuilder) {
        self.records.insert(n, Slot::Built(record));
    }

    /// Places raw bytes at index `n` (no fixups applied; padded or
    /// truncated to the record size).
    pub fn insert_raw(&mut self, n: u64, bytes: Vec<u8>) {
        self.records.insert(n, Slot::Raw(bytes));
    }

    /// Lays out the image and returns its bytes.
    ///
    /// # Panics
    ///
    /// A record does not fit, or the `$MFT` extension record is not
    /// reachable through the runs held by record 0.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        let g = self.geometry;
        let cs = u64::from(g.cluster_size);
        let rs = u64::from(g.record_size);

        // MFT size: enough records, whole clusters, at least one cluster per fragment.
        let wanted = self
            .records
            .keys()
            .next_back()
            .map_or(0, |&n| n + 1)
            .max(self.min_records)
            .max(if self.system_files { 16 } else { 1 })
            .max(self.mft_extension.map_or(0, |e| 2 * (e + 1)));
        let mut clusters = (wanted * rs).div_ceil(cs).max(self.mft_fragments as u64);
        if self.mft_extension.is_some() {
            clusters = clusters.max(2);
        }
        let record_count = clusters * cs / rs;
        let mft_runs = self.alloc_fragmented(clusters, self.mft_fragments);
        let mirror_runs = self.alloc((4 * rs).div_ceil(cs));
        let bitmap_bytes = record_count.div_ceil(8).next_multiple_of(8);
        let mft_bitmap_runs = self.alloc(bitmap_bytes.div_ceil(cs));
        let mft_bytes = record_count * rs;

        // Record 0 ($MFT), optionally with $DATA split into an extension record.
        let mft_ref = FileRef::from_parts(0, 1);
        let (head, tail) = match self.mft_extension {
            Some(_) => {
                let split = mft_runs.len().div_ceil(2).max(1);
                if split < mft_runs.len() {
                    (mft_runs[..split].to_vec(), mft_runs[split..].to_vec())
                } else {
                    // One fragment: split it into two runs so there is a tail.
                    let r = mft_runs[0];
                    let h = r.len.div_ceil(2);
                    (
                        vec![Run { len: h, ..r }],
                        vec![Run {
                            vcn: h,
                            lcn: r.lcn.map(|l| l + h),
                            len: r.len - h,
                        }],
                    )
                }
            }
            None => (mft_runs.clone(), Vec::new()),
        };
        let head_spec = NonResidentSpec {
            start_vcn: 0,
            runs: head.clone(),
            allocated: clusters * cs,
            real: mft_bytes,
            initialized: mft_bytes,
            total_allocated: None,
            compression_unit: 0,
        };
        let mut rec0 = RecordBuilder::new(1).std_info(
            DEFAULT_TIMES,
            win32::FILE_ATTRIBUTE_HIDDEN | win32::FILE_ATTRIBUTE_SYSTEM,
        );
        if let Some(ext) = self.mft_extension {
            let head_end = head.last().map_or(0, Run::end_vcn);
            assert!(
                (ext + 1) * rs <= head_end * cs,
                "$MFT extension record {ext} is not inside the runs held by record 0"
            );
            let list = attr_list_value(&[
                AttrListSpec::new(AT_STANDARD_INFORMATION, 0, mft_ref),
                AttrListSpec::new(AT_FILE_NAME, 0, mft_ref),
                AttrListSpec::new(AT_DATA, 0, mft_ref),
                AttrListSpec::new(AT_DATA, head_end, FileRef::from_parts(ext, 1)),
                AttrListSpec::new(AT_BITMAP, 0, mft_ref),
            ]);
            rec0 = if self.mft_attr_list_nonresident {
                let runs = self.alloc((list.len() as u64).div_ceil(cs));
                self.write_runs(&runs, &list);
                rec0.attr(
                    AT_ATTRIBUTE_LIST,
                    "",
                    0,
                    AttrValue::NonResident(NonResidentSpec::new(
                        runs,
                        list.len() as u64,
                        g.cluster_size,
                    )),
                )
            } else {
                rec0.attr(AT_ATTRIBUTE_LIST, "", 0, AttrValue::Resident(list))
            };
            self.insert(
                ext,
                RecordBuilder::new(1)
                    .extension_of(mft_ref)
                    .data_nonresident("", 0, NonResidentSpec::continuation(tail)),
            );
        }
        rec0 = rec0
            .name(ROOT, "$MFT", NS_WIN32_AND_DOS)
            .data_nonresident("", 0, head_spec)
            .attr(
                AT_BITMAP,
                "",
                0,
                AttrValue::NonResident(NonResidentSpec::new(
                    mft_bitmap_runs.clone(),
                    bitmap_bytes,
                    g.cluster_size,
                )),
            );
        self.records.insert(0, Slot::Built(rec0));

        if self.system_files {
            self.add_system_files(&mirror_runs, rs);
        }

        // Volume size, then $Bitmap sized for it.
        let mut total = self.claimed_clusters.max(self.next_lcn + 16);
        let mut vol_bitmap_clusters = 0;
        if self.system_files {
            for _ in 0..4 {
                vol_bitmap_clusters = total.div_ceil(8).next_multiple_of(8).div_ceil(cs);
                total = self
                    .claimed_clusters
                    .max(self.next_lcn + vol_bitmap_clusters + 16);
            }
            let runs = self.alloc(vol_bitmap_clusters);
            let bytes = total.div_ceil(8).next_multiple_of(8);
            let mut bits = vec![0u8; bytes as usize];
            for &(lcn, len) in &self.used {
                for c in lcn..lcn + len {
                    if c < total {
                        bits[(c / 8) as usize] |= 1 << (c % 8);
                    }
                }
            }
            self.write_runs(&runs, &bits);
            self.insert(
                6,
                RecordBuilder::new(6)
                    .std_info(
                        DEFAULT_TIMES,
                        win32::FILE_ATTRIBUTE_HIDDEN | win32::FILE_ATTRIBUTE_SYSTEM,
                    )
                    .name(ROOT, "$Bitmap", NS_WIN32_AND_DOS)
                    .data_nonresident("", 0, NonResidentSpec::new(runs, bytes, g.cluster_size)),
            );
            // $BadClus:$Bad spans the volume as one sparse run, without the sparse flag.
            self.insert(
                8,
                RecordBuilder::new(8)
                    .std_info(
                        DEFAULT_TIMES,
                        win32::FILE_ATTRIBUTE_HIDDEN | win32::FILE_ATTRIBUTE_SYSTEM,
                    )
                    .name(ROOT, "$BadClus", NS_WIN32_AND_DOS)
                    .data("", &[])
                    .data_nonresident(
                        "$Bad",
                        0,
                        NonResidentSpec::new(sparse_run(total), total * cs, g.cluster_size),
                    ),
            );
        }

        // Render records into MFT space and the $MFT:$BITMAP.
        let mut mft = vec![0u8; mft_bytes as usize];
        let mut mft_bits = vec![0u8; bitmap_bytes as usize];
        for n in 0..record_count {
            let bytes = match self.records.get(&n) {
                Some(Slot::Built(b)) => {
                    if b.flags & RECORD_IN_USE != 0 || b.signature != *b"FILE" {
                        mft_bits[(n / 8) as usize] |= 1 << (n % 8);
                    }
                    b.build(g, n)
                }
                Some(Slot::Raw(raw)) => {
                    mft_bits[(n / 8) as usize] |= 1 << (n % 8);
                    let mut v = raw.clone();
                    v.resize(rs as usize, 0);
                    v
                }
                None => RecordBuilder::new(1).free().build(g, n),
            };
            let off = (n * rs) as usize;
            mft[off..off + rs as usize].copy_from_slice(&bytes);
        }
        self.write_runs(&mft_runs, &mft);
        self.write_runs(&mirror_runs, &mft[..(4 * rs) as usize]);
        self.write_runs(&mft_bitmap_runs, &mft_bits);

        // Boot sector.
        let mut boot = vec![0u8; 512];
        boot[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
        boot[3..11].copy_from_slice(b"NTFS    ");
        boot[0x0B..0x0D].copy_from_slice(&(g.sector_size as u16).to_le_bytes());
        let spc = g.cluster_size / g.sector_size;
        boot[0x0D] = if spc <= 0x80 {
            spc as u8
        } else {
            (256 - spc.trailing_zeros()) as u8
        };
        boot[0x15] = 0xF8;
        let sectors = total * u64::from(spc);
        boot[0x28..0x30].copy_from_slice(&sectors.to_le_bytes());
        boot[0x30..0x38].copy_from_slice(&mft_runs[0].lcn.unwrap_or(0).to_le_bytes());
        boot[0x38..0x40].copy_from_slice(&mirror_runs[0].lcn.unwrap_or(0).to_le_bytes());
        boot[0x40] = encode_clusters_per(g.record_size, g.cluster_size);
        boot[0x44] = encode_clusters_per(4096, g.cluster_size);
        boot[0x48..0x50].copy_from_slice(&self.serial.to_le_bytes());
        boot[0x1FE] = 0x55;
        boot[0x1FF] = 0xAA;
        self.writes.push((0, boot));

        let end = self
            .writes
            .iter()
            .map(|(o, d)| o + d.len() as u64)
            .max()
            .unwrap_or(0)
            .next_multiple_of(cs);
        let mut img = vec![0u8; end as usize];
        for (o, d) in &self.writes {
            img[*o as usize..*o as usize + d.len()].copy_from_slice(d);
        }
        img
    }

    fn add_system_files(&mut self, mirror_runs: &[Run], rs: u64) {
        let g = self.geometry;
        let cs = u64::from(g.cluster_size);
        let hs = win32::FILE_ATTRIBUTE_HIDDEN | win32::FILE_ATTRIBUTE_SYSTEM;
        let sys = |n: u64, name: &str| {
            RecordBuilder::new(n as u16)
                .std_info(DEFAULT_TIMES, hs)
                .name(ROOT, name, NS_WIN32_AND_DOS)
        };
        let mirror = NonResidentSpec::new(mirror_runs.to_vec(), 4 * rs, g.cluster_size);
        self.insert(1, sys(1, "$MFTMirr").data_nonresident("", 0, mirror));
        let log = self.alloc(4);
        self.insert(
            2,
            sys(2, "$LogFile").data_nonresident(
                "",
                0,
                NonResidentSpec::new(log, 4 * cs, g.cluster_size),
            ),
        );
        self.insert(3, sys(3, "$Volume").data("", &[]));
        let attrdef = self.alloc(2560u64.div_ceil(cs));
        self.insert(
            4,
            sys(4, "$AttrDef").data_nonresident(
                "",
                0,
                NonResidentSpec::new(attrdef, 2560, g.cluster_size),
            ),
        );
        self.insert(
            5,
            RecordBuilder::new(5)
                .directory()
                .std_info(DEFAULT_TIMES, hs)
                .name(ROOT, ".", NS_WIN32_AND_DOS)
                .attr(
                    AT_INDEX_ROOT,
                    "$I30",
                    0,
                    AttrValue::Resident(vec![0u8; 0x20]),
                ),
        );
        let boot_clusters = BOOT_BYTES.div_ceil(cs);
        let boot_runs = vec![Run {
            vcn: 0,
            lcn: Some(0),
            len: boot_clusters,
        }];
        self.insert(
            7,
            sys(7, "$Boot").data_nonresident(
                "",
                0,
                NonResidentSpec::new(boot_runs, BOOT_BYTES, g.cluster_size),
            ),
        );
        let sds = self.alloc(1);
        self.insert(
            9,
            sys(9, "$Secure")
                .data_nonresident("$SDS", 0, NonResidentSpec::new(sds, 512, g.cluster_size))
                .attr(
                    AT_INDEX_ROOT,
                    "$SDH",
                    0,
                    AttrValue::Resident(vec![0u8; 0x20]),
                )
                .attr(
                    AT_INDEX_ROOT,
                    "$SII",
                    0,
                    AttrValue::Resident(vec![0u8; 0x20]),
                ),
        );
        let upcase = self.alloc(131_072u64.div_ceil(cs));
        self.insert(
            10,
            sys(10, "$UpCase").data_nonresident(
                "",
                0,
                NonResidentSpec::new(upcase, 131_072, g.cluster_size),
            ),
        );
        self.insert(
            11,
            RecordBuilder::new(11)
                .directory()
                .std_info(DEFAULT_TIMES, hs)
                .name(ROOT, "$Extend", NS_WIN32_AND_DOS)
                .attr(
                    AT_INDEX_ROOT,
                    "$I30",
                    0,
                    AttrValue::Resident(vec![0u8; 0x20]),
                ),
        );
    }
}

/// Encodes the signed clusters-per-record byte.
fn encode_clusters_per(bytes: u32, cluster_size: u32) -> u8 {
    if bytes >= cluster_size {
        (bytes / cluster_size) as u8
    } else {
        (-(bytes.trailing_zeros() as i8)) as u8
    }
}

// -----------------------------------------------------------------------------
// Samples (fuzz seeds, property tests, benchmarks)
// -----------------------------------------------------------------------------

/// A representative set of records covering every attribute shape the
/// scanner understands, each paired with its record number.
#[must_use]
pub fn sample_records() -> Vec<(u64, RecordBuilder)> {
    use crate::attr::{NS_DOS, NS_POSIX, NS_WIN32};
    let runs = |lcn: u64, len: u64| {
        vec![Run {
            vcn: 0,
            lcn: Some(lcn),
            len,
        }]
    };
    let dir = FileRef::from_parts(64, 1);
    let base = FileRef::from_parts(80, 1);
    let wof = [1, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    vec![
        (
            64,
            RecordBuilder::dir(1, ROOT, "dir")
                .index_allocation("$I30", NonResidentSpec::new(runs(40, 2), 8192, 4096)),
        ),
        (
            65,
            RecordBuilder::file(1, dir, "plain.txt").data("", b"hello world"),
        ),
        (
            66,
            RecordBuilder::new(1)
                .std_info(DEFAULT_TIMES, win32::FILE_ATTRIBUTE_ARCHIVE)
                .name(dir, "Long Name.txt", NS_WIN32)
                .name(dir, "LONGNA~1.TXT", NS_DOS)
                .name(ROOT, "link", NS_POSIX)
                .data("", b"x"),
        ),
        (
            67,
            RecordBuilder::file(1, dir, "big.bin").data_nonresident(
                "",
                0,
                NonResidentSpec::new(runs(50, 3), 10_000, 4096),
            ),
        ),
        (
            68,
            RecordBuilder::file(1, dir, "sparse").data_sparse(
                "",
                NonResidentSpec::new(chain_runs(0, &[runs(60, 1), sparse_run(99)]), 400_000, 4096),
                4096,
            ),
        ),
        (
            69,
            RecordBuilder::file(1, dir, "packed").data_compressed(
                "",
                NonResidentSpec::new(chain_runs(0, &[runs(70, 5), sparse_run(11)]), 60_000, 4096),
                20_480,
            ),
        ),
        (
            70,
            RecordBuilder::file(1, dir, "wof.exe")
                .data_sparse("", NonResidentSpec::new(sparse_run(10), 40_000, 4096), 0)
                .data_nonresident(
                    "WofCompressedData",
                    0,
                    NonResidentSpec::new(runs(80, 2), 7000, 4096),
                )
                .reparse(raw_reparse(win32::IO_REPARSE_TAG_WOF, &wof)),
        ),
        (
            71,
            RecordBuilder::file(1, dir, "cloud.docx")
                .reparse(raw_reparse(0x9000_601A, &[0; 16]))
                .data("", &[]),
        ),
        (
            72,
            RecordBuilder::file(1, dir, "sym")
                .reparse(symlink_reparse(r"\??\C:\t", r"C:\t", false))
                .data("", &[]),
        ),
        (
            73,
            RecordBuilder::dir(1, dir, "junction").reparse(mount_point_reparse(r"\??\D:\", "")),
        ),
        (
            74,
            RecordBuilder::file(1, dir, "ads")
                .data("", b"a")
                .data("Zone.Identifier", b"[ZoneTransfer]"),
        ),
        (
            80,
            RecordBuilder::file(1, dir, "split").attr_list(&[AttrListSpec::new(
                AT_DATA,
                0,
                FileRef::from_parts(81, 1),
            )]),
        ),
        (
            81,
            RecordBuilder::new(1).extension_of(base).data_nonresident(
                "",
                0,
                NonResidentSpec::new(runs(90, 1), 100, 4096),
            ),
        ),
        (82, RecordBuilder::file(1, dir, "baad").baad()),
        (
            83,
            RecordBuilder::file(1, dir, "torn").data("", b"t").torn(),
        ),
    ]
}

/// A small image holding [`sample_records`] with a fragmented MFT whose
/// `$DATA` continues in an extension record.
#[must_use]
pub fn sample_image() -> Vec<u8> {
    let mut b = ImageBuilder::new(Geometry::default())
        .with_system_files()
        .mft_fragments(3)
        .mft_data_in_extension(15)
        .claim_clusters(4096);
    for (n, r) in sample_records() {
        b.insert(n, r);
    }
    b.finish()
}
