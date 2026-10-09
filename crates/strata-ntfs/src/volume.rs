//! An opened NTFS volume: `$MFT` bootstrap, single-record reads and
//! non-resident value reads.
//!
//! Responsibilities:
//! - Locate every `$MFT` fragment from record 0, following its attribute
//!   list when `$DATA` spills into extension records.
//! - Map MFT record numbers to disk offsets ([`MftLayout`]).
//! - [`NtfsVolume::read_record`]: one fully merged record, following the
//!   attribute list to its holding records (used for USN refreshes).
//! - Read non-resident values (attribute lists, reparse buffers, bitmaps).
//! - Count used clusters from `$Bitmap` for volume reconciliation.

use std::collections::BTreeSet;

use strata_core::{FileRef, Reparse, ScanRecord};

use crate::assemble::assemble;
use crate::attr::{AT_BITMAP, AT_DATA, parse_attr_list, parse_reparse};
use crate::boot::BootSector;
use crate::error::{NtfsError, RecordError, Result};
use crate::io::ReadAt;
use crate::record::{
    DataPiece, ParseOptions, ParsedRecord, RecordOutcome, ReparseLoc, ValueLoc, parse_record,
};
use crate::runlist::Run;

/// Record number of `$MFT`.
pub const MFT_RECORD: u64 = 0;
/// Record number of `$Bitmap` (volume cluster allocation bitmap).
pub const BITMAP_RECORD: u64 = 6;

/// Largest attribute list or reparse buffer read from disk. NTFS caps
/// attribute lists at 256 KiB and reparse buffers at 16 KiB.
const MAX_SMALL_VALUE: u64 = 16 * 1024 * 1024;
/// Largest `$MFT:$BITMAP` read (covers ~2 billion records).
const MAX_MFT_BITMAP: u64 = 256 * 1024 * 1024;
/// Read granularity when streaming large values.
const STREAM_CHUNK: u64 = 4 * 1024 * 1024;

/// A contiguous piece of the virtual MFT byte range on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Segment {
    /// Disk byte offset; `None` for a sparse (zero) range.
    pub disk: Option<u64>,
    /// Length in bytes.
    pub len: u64,
}

/// Where the MFT lives on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MftLayout {
    /// Bytes per record.
    pub record_size: u32,
    /// Bytes per cluster.
    pub cluster_size: u64,
    /// Every `$MFT:$DATA` run in VCN order.
    pub runs: Vec<Run>,
    /// `$MFT` logical size in bytes.
    pub data_size: u64,
    /// `$MFT` initialized size in bytes. Records past it read as zeros.
    pub initialized_size: u64,
}

impl MftLayout {
    /// Records in the MFT (from its logical size).
    #[must_use]
    pub fn record_count(&self) -> u64 {
        self.data_size / u64::from(self.record_size)
    }

    /// Records that can be read: bounded by the initialized size and by the
    /// clusters the runlist actually maps.
    #[must_use]
    pub fn readable_records(&self) -> u64 {
        let mapped = self
            .runs
            .last()
            .map_or(0, |r| r.end_vcn().saturating_mul(self.cluster_size));
        self.data_size.min(self.initialized_size).min(mapped) / u64::from(self.record_size)
    }

    /// Number of extents (fragments) the MFT occupies.
    #[must_use]
    pub fn fragment_count(&self) -> usize {
        self.runs.len()
    }

    /// Maps `len` bytes of the MFT starting at virtual byte `start` to disk
    /// segments. `None` if any byte is unmapped.
    pub(crate) fn map(&self, start: u64, len: u64) -> Option<Vec<Segment>> {
        map_runs(&self.runs, self.cluster_size, start, len)
    }
}

/// Maps a virtual byte range of an attribute onto its runs.
pub(crate) fn map_runs(runs: &[Run], cs: u64, start: u64, len: u64) -> Option<Vec<Segment>> {
    let end = start.checked_add(len)?;
    let mut out = Vec::new();
    let mut pos = start;
    let first = runs.partition_point(|r| r.end_vcn().saturating_mul(cs) <= start);
    for r in runs.get(first..)? {
        if pos >= end {
            break;
        }
        let r_start = r.vcn.checked_mul(cs)?;
        let r_end = r.end_vcn().checked_mul(cs)?;
        if r_start > pos {
            return None;
        }
        let take = r_end.min(end).checked_sub(pos).filter(|&t| t > 0)?;
        let disk = match r.lcn {
            Some(l) => Some(l.checked_mul(cs)?.checked_add(pos - r_start)?),
            None => None,
        };
        out.push(Segment { disk, len: take });
        pos += take;
    }
    (pos >= end).then_some(out)
}

/// Reads mapped segments into `buf`; sparse segments become zeros.
pub(crate) fn read_segments<R: ReadAt + ?Sized>(
    reader: &R,
    segments: &[Segment],
    buf: &mut [u8],
) -> std::io::Result<()> {
    let mut pos = 0usize;
    for s in segments {
        let len = usize::try_from(s.len).map_err(|_| std::io::ErrorKind::InvalidInput)?;
        let dst = buf
            .get_mut(pos..pos + len)
            .ok_or(std::io::ErrorKind::InvalidInput)?;
        match s.disk {
            Some(off) => reader.read_at(off, dst)?,
            None => dst.fill(0),
        }
        pos += len;
    }
    Ok(())
}

/// An NTFS volume (or image) with its MFT located.
///
/// # Example
///
/// ```
/// use strata_ntfs::test_image::{Geometry, ImageBuilder};
/// use strata_ntfs::NtfsVolume;
/// let img = ImageBuilder::new(Geometry::default()).with_system_files().finish();
/// let vol = NtfsVolume::open(img).unwrap();
/// let root = vol.read_record(5).unwrap().unwrap();
/// assert!(root.is_dir());
/// ```
#[derive(Debug)]
pub struct NtfsVolume<R> {
    reader: R,
    boot: BootSector,
    layout: MftLayout,
    mft_bitmap: Option<ValueLoc>,
}

impl<R: ReadAt> NtfsVolume<R> {
    /// Reads the boot sector and bootstraps the MFT layout.
    ///
    /// # Errors
    ///
    /// The boot sector is invalid, record 0 is unreadable or not a valid
    /// in-use `$MFT` record, or `$MFT:$DATA` cannot be fully located.
    pub fn open(reader: R) -> Result<Self> {
        let mut sector = vec![0u8; BootSector::LEN];
        reader.read_at(0, &mut sector)?;
        let boot = BootSector::parse(&sector)?;
        let (layout, mft_bitmap) = bootstrap_mft(&reader, &boot)?;
        Ok(Self {
            reader,
            boot,
            layout,
            mft_bitmap,
        })
    }

    /// The decoded boot sector.
    #[must_use]
    pub fn boot(&self) -> &BootSector {
        &self.boot
    }

    /// The MFT layout.
    #[must_use]
    pub fn layout(&self) -> &MftLayout {
        &self.layout
    }

    /// The underlying reader.
    #[must_use]
    pub fn reader(&self) -> &R {
        &self.reader
    }

    /// Parse options matching this volume's geometry.
    #[must_use]
    pub fn parse_options(&self) -> ParseOptions {
        ParseOptions::scan(self.boot.total_clusters)
    }

    /// Reads record `n` without applying fixups.
    ///
    /// # Errors
    ///
    /// `n` is past the readable MFT, or the read fails.
    pub fn read_raw_record(&self, n: u64) -> Result<Vec<u8>> {
        if n >= self.layout.readable_records() {
            return Err(NtfsError::OutOfRange(n));
        }
        let rs = u64::from(self.layout.record_size);
        let segs = n
            .checked_mul(rs)
            .and_then(|start| self.layout.map(start, rs))
            .ok_or(NtfsError::OutOfRange(n))?;
        let mut buf = vec![0u8; self.layout.record_size as usize];
        read_segments(&self.reader, &segs, &mut buf)?;
        Ok(buf)
    }

    /// Reads and parses record `n` (fixups applied) with `opts`.
    ///
    /// # Errors
    ///
    /// As [`Self::read_raw_record`].
    pub fn parse_record_at(&self, n: u64, opts: &ParseOptions) -> Result<RecordOutcome> {
        let mut buf = self.read_raw_record(n)?;
        Ok(parse_record(&mut buf, n, opts))
    }

    /// Reads one fully merged file record, following `$ATTRIBUTE_LIST` to
    /// the extension records that hold its attributes.
    ///
    /// Returns `None` for a free record and for extension records (which are
    /// part of another file). Compare the returned id's sequence with the
    /// reference you hold to detect reuse.
    ///
    /// # Errors
    ///
    /// The record is corrupt (`BAAD`, torn, malformed), past the MFT, or a
    /// read fails.
    pub fn read_record(&self, n: u64) -> Result<Option<ScanRecord>> {
        let opts = self.parse_options().for_record(n);
        let p = match self.parse_record_at(n, &opts)? {
            RecordOutcome::InUse(p) => *p,
            RecordOutcome::Free => return Ok(None),
            other => return Err(outcome_error(n, other)),
        };
        if p.base.is_some() {
            return Ok(None);
        }
        let extensions = self.extensions_via_list(&p, &opts)?;
        let reparse = self.resolve_reparse(&p, &extensions);
        Ok(Some(assemble(
            p,
            extensions,
            reparse,
            self.boot.cluster_size,
        )))
    }

    /// Reads the extension records named by `base`'s attribute list.
    ///
    /// Holders that are free, corrupt, or whose base reference does not match
    /// `base` (stale list entries) are skipped.
    ///
    /// # Errors
    ///
    /// The attribute list cannot be read or decoded, or a read fails.
    pub fn extensions_via_list(
        &self,
        base: &ParsedRecord,
        opts: &ParseOptions,
    ) -> Result<Vec<ParsedRecord>> {
        let Some(list) = &base.attr_list else {
            return Ok(Vec::new());
        };
        let bytes = self.read_value(list, MAX_SMALL_VALUE)?;
        let entries = parse_attr_list(&bytes).map_err(|reason| NtfsError::Record {
            record: base.record,
            source: RecordError::Attribute { offset: 0, reason },
        })?;
        let holders: BTreeSet<u64> = entries
            .iter()
            .map(|e| e.holder.record())
            .filter(|&h| h != base.record)
            .collect();
        let mut out = Vec::new();
        for h in holders {
            if let Ok(RecordOutcome::InUse(e)) = self.parse_record_at(h, opts)
                && e.base == Some(base.file_ref())
            {
                out.push(*e);
            }
        }
        Ok(out)
    }

    /// Decodes a non-resident reparse buffer held by `base` or one of its
    /// extensions. Returns `None` when the value is resident (the assembler
    /// uses it directly) or unreadable.
    pub(crate) fn resolve_reparse(
        &self,
        base: &ParsedRecord,
        extensions: &[ParsedRecord],
    ) -> Option<Reparse> {
        let loc = std::iter::once(base)
            .chain(extensions)
            .find_map(|p| match &p.reparse {
                Some(ReparseLoc::NonResident(v)) => Some(v),
                _ => None,
            })?;
        let bytes = self.read_value(loc, MAX_SMALL_VALUE).ok()?;
        parse_reparse(&bytes).ok()
    }

    /// Reads a resident or non-resident value, refusing values over `max` bytes.
    ///
    /// # Errors
    ///
    /// The value exceeds `max`, its runs do not cover its size, or a read fails.
    pub fn read_value(&self, loc: &ValueLoc, max: u64) -> Result<Vec<u8>> {
        match loc {
            ValueLoc::Resident(v) => Ok(v.clone()),
            ValueLoc::NonResident { runs, size } => {
                if *size > max {
                    return Err(NtfsError::InvalidOption(format!(
                        "value of {size} bytes exceeds the {max}-byte limit"
                    )));
                }
                let segs = map_runs(runs, self.boot.cluster_size, 0, *size).ok_or_else(|| {
                    NtfsError::Mft("non-resident value runs do not cover its size".into())
                })?;
                let mut buf = vec![0u8; *size as usize];
                read_segments(&self.reader, &segs, &mut buf)?;
                Ok(buf)
            }
        }
    }

    /// The `$MFT:$BITMAP` value (one bit per record, set = in use).
    ///
    /// # Errors
    ///
    /// The bitmap cannot be read.
    pub fn mft_bitmap(&self) -> Result<Option<Vec<u8>>> {
        self.mft_bitmap
            .as_ref()
            .map(|loc| self.read_value(loc, MAX_MFT_BITMAP))
            .transpose()
    }

    /// Counts clusters marked used in `$Bitmap` (record 6), bounded by the
    /// volume's cluster count.
    ///
    /// # Errors
    ///
    /// `$Bitmap` is missing, corrupt, or unreadable.
    pub fn count_used_clusters(&self) -> Result<u64> {
        let opts = ParseOptions {
            decode_runs: true,
            ..self.parse_options()
        };
        let p = match self.parse_record_at(BITMAP_RECORD, &opts)? {
            RecordOutcome::InUse(p) => *p,
            other => return Err(outcome_error(BITMAP_RECORD, other)),
        };
        let mut pieces: Vec<DataPiece> = p.data.clone();
        for mut e in self.extensions_via_list(&p, &opts)? {
            pieces.append(&mut e.data);
        }
        let pieces: Vec<DataPiece> = pieces.into_iter().filter(|d| d.name.is_empty()).collect();
        let size = pieces
            .iter()
            .find(|d| d.start_vcn == 0)
            .ok_or_else(|| NtfsError::Mft("$Bitmap has no $DATA".into()))?
            .initialized;
        let total = self.boot.total_clusters;
        let bytes_needed = total.div_ceil(8).min(size);
        let mut used = 0u64;
        let mut bit_base = 0u64;
        let mut stream = |chunk: &[u8]| {
            for &b in chunk {
                if bit_base >= total {
                    break;
                }
                let valid = (total - bit_base).min(8) as u32;
                let mask = if valid == 8 { 0xFF } else { (1u8 << valid) - 1 };
                used += u64::from((b & mask).count_ones());
                bit_base += 8;
            }
        };
        let runs = merge_piece_runs(pieces)
            .ok_or_else(|| NtfsError::Mft("$Bitmap runlist is discontiguous".into()))?;
        if runs.is_empty() {
            // Resident bitmap (tiny volumes): the value was not captured, so re-read it.
            let v = self.resident_data(BITMAP_RECORD)?;
            stream(v.get(..bytes_needed as usize).unwrap_or(&v));
            return Ok(used);
        }
        let mut pos = 0u64;
        while pos < bytes_needed {
            let len = STREAM_CHUNK.min(bytes_needed - pos);
            let segs = map_runs(&runs, self.boot.cluster_size, pos, len)
                .ok_or_else(|| NtfsError::Mft("$Bitmap runs do not cover the volume".into()))?;
            let mut buf = vec![0u8; len as usize];
            read_segments(&self.reader, &segs, &mut buf)?;
            stream(&buf);
            pos += len;
        }
        Ok(used)
    }

    fn resident_data(&self, n: u64) -> Result<Vec<u8>> {
        let mut buf = self.read_raw_record(n)?;
        crate::fixup::apply_fixups(&mut buf).map_err(|_| NtfsError::Record {
            record: n,
            source: RecordError::Header("fixup mismatch"),
        })?;
        let first = usize::from(crate::le::u16_at(&buf, 0x14).unwrap_or(0));
        let used = crate::le::u32_at(&buf, 0x18).unwrap_or(0) as usize;
        for a in crate::attr::AttrIter::new(&buf, first, used) {
            let a = a.map_err(|source| NtfsError::Record { record: n, source })?;
            if let (AT_DATA, crate::attr::AttrForm::Resident { value }) = (a.type_code, a.form)
                && a.is_unnamed()
            {
                return Ok(value.to_vec());
            }
        }
        Err(NtfsError::Mft(format!("record {n} has no resident $DATA")))
    }
}

/// Converts a non-in-use outcome into an error for single-record reads.
fn outcome_error(record: u64, o: RecordOutcome) -> NtfsError {
    let source = match o {
        RecordOutcome::Malformed(e) => e,
        RecordOutcome::Baad => RecordError::Header("record is marked BAAD"),
        RecordOutcome::Torn => RecordError::Header("fixup mismatch (torn write)"),
        RecordOutcome::BadSignature => RecordError::Header("bad record signature"),
        RecordOutcome::Free => RecordError::Header("record is not in use"),
        RecordOutcome::InUse(_) => RecordError::Header("unexpected record state"),
    };
    NtfsError::Record { record, source }
}

/// Concatenates the runs of pieces in VCN order. `None` on gaps or overlaps.
fn merge_piece_runs(mut pieces: Vec<DataPiece>) -> Option<Vec<Run>> {
    pieces.sort_by_key(|p| p.start_vcn);
    let mut runs: Vec<Run> = Vec::new();
    for p in pieces {
        if p.resident {
            continue;
        }
        let next = runs.last().map_or(0, Run::end_vcn);
        if p.start_vcn != next {
            return None;
        }
        runs.extend(p.runs?);
    }
    Some(runs)
}

/// Locates every `$MFT:$DATA` run, following record 0's attribute list
/// into extension records that are reachable through the runs found so far.
fn bootstrap_mft<R: ReadAt + ?Sized>(
    reader: &R,
    boot: &BootSector,
) -> Result<(MftLayout, Option<ValueLoc>)> {
    let opts = ParseOptions {
        total_clusters: boot.total_clusters,
        decode_runs: true,
        capture_bitmap: true,
    };
    let rs = u64::from(boot.record_size);
    let mut rec0 = vec![0u8; boot.record_size as usize];
    reader.read_at(boot.mft_offset(), &mut rec0)?;
    let base = match parse_record(&mut rec0, MFT_RECORD, &opts) {
        RecordOutcome::InUse(p) if p.base.is_none() => *p,
        other => {
            return Err(NtfsError::Mft(format!(
                "record 0 is not a valid in-use base record: {other:?}"
            )));
        }
    };
    let unnamed = |p: &ParsedRecord| -> Vec<DataPiece> {
        p.data
            .iter()
            .filter(|d| d.name.is_empty() && !d.resident)
            .cloned()
            .collect()
    };
    let mut pieces = unnamed(&base);
    let mut bitmap = base.bitmap.clone();
    let mut pending: BTreeSet<u64> = BTreeSet::new();
    if let Some(list) = &base.attr_list {
        let bytes = match list {
            ValueLoc::Resident(v) => v.clone(),
            ValueLoc::NonResident { runs, size } => {
                if *size > MAX_SMALL_VALUE {
                    return Err(NtfsError::Mft("$MFT attribute list is too large".into()));
                }
                let segs = map_runs(runs, boot.cluster_size, 0, *size)
                    .ok_or_else(|| NtfsError::Mft("$MFT attribute list runs are short".into()))?;
                let mut buf = vec![0u8; *size as usize];
                read_segments(reader, &segs, &mut buf)?;
                buf
            }
        };
        let entries = parse_attr_list(&bytes)
            .map_err(|e| NtfsError::Mft(format!("$MFT attribute list: {e}")))?;
        pending = entries
            .iter()
            .filter(|e| matches!(e.type_code, AT_DATA | AT_BITMAP) && e.name.is_empty())
            .map(|e| e.holder.record())
            .filter(|&h| h != MFT_RECORD)
            .collect();
    }
    let base_ref = FileRef::from_parts(MFT_RECORD, base.sequence);
    while !pending.is_empty() {
        let known = merge_piece_runs(pieces.clone())
            .ok_or_else(|| NtfsError::Mft("$MFT:$DATA pieces overlap or have gaps".into()))?;
        let mut progressed = false;
        for h in pending.clone() {
            let Some(segs) = h
                .checked_mul(rs)
                .and_then(|s| map_runs(&known, boot.cluster_size, s, rs))
            else {
                continue;
            };
            let mut buf = vec![0u8; boot.record_size as usize];
            read_segments(reader, &segs, &mut buf)?;
            match parse_record(&mut buf, h, &opts) {
                RecordOutcome::InUse(e) if e.base == Some(base_ref) => {
                    pieces.extend(unnamed(&e));
                    if bitmap.is_none() {
                        bitmap = e.bitmap.clone();
                    }
                }
                other => {
                    return Err(NtfsError::Mft(format!(
                        "$MFT extension record {h} is invalid: {other:?}"
                    )));
                }
            }
            pending.remove(&h);
            progressed = true;
        }
        if !progressed {
            return Err(NtfsError::Mft(format!(
                "$MFT extension records {pending:?} are not reachable through known runs"
            )));
        }
    }
    let vcn0 = pieces
        .iter()
        .find(|p| p.start_vcn == 0)
        .cloned()
        .ok_or_else(|| NtfsError::Mft("$MFT has no non-resident $DATA at VCN 0".into()))?;
    let runs = merge_piece_runs(pieces)
        .ok_or_else(|| NtfsError::Mft("$MFT:$DATA pieces overlap or have gaps".into()))?;
    if vcn0.logical < rs {
        return Err(NtfsError::Mft("$MFT is smaller than one record".into()));
    }
    Ok((
        MftLayout {
            record_size: boot.record_size,
            cluster_size: boot.cluster_size,
            runs,
            data_size: vcn0.logical,
            initialized_size: vcn0.initialized,
        },
        bitmap,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_runs_splits_and_handles_sparse() {
        let runs = vec![
            Run {
                vcn: 0,
                lcn: Some(10),
                len: 2,
            },
            Run {
                vcn: 2,
                lcn: None,
                len: 1,
            },
            Run {
                vcn: 3,
                lcn: Some(5),
                len: 1,
            },
        ];
        let segs = map_runs(&runs, 512, 256, 1536).unwrap();
        assert_eq!(
            segs,
            vec![
                Segment {
                    disk: Some(10 * 512 + 256),
                    len: 768
                },
                Segment {
                    disk: None,
                    len: 512
                },
                Segment {
                    disk: Some(5 * 512),
                    len: 256
                },
            ]
        );
        assert!(map_runs(&runs, 512, 0, 4 * 512 + 1).is_none());
        assert!(map_runs(&runs, 512, u64::MAX, 2).is_none());
        assert_eq!(map_runs(&runs, 512, 0, 0), Some(vec![]));
    }
}
