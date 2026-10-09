//! The full-MFT scan pipeline.
//!
//! An I/O thread reads the MFT in large chunks (fragment by fragment, in
//! VCN order) and hands them over a bounded channel to the calling thread,
//! which fans each chunk out to the `rayon` pool for fixups and parsing.
//! Records that need other records (attribute lists) or disk reads
//! (non-resident reparse buffers) are buffered and completed after the pass,
//! together with every extension record, merged by base reference.
//!
//! Output order: base records in record-number order as chunks complete,
//! then the deferred records in record-number order.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::bounded;
use rayon::prelude::*;
use strata_core::ScanRecord;

use crate::assemble::assemble;
use crate::error::{NtfsError, Result};
use crate::io::{AlignedBuf, ReadAt};
use crate::record::{ParsedRecord, RecordOutcome, parse_record};
use crate::volume::{NtfsVolume, read_segments};

/// Default read size per I/O request.
pub const DEFAULT_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// Upper bound on records per batch handed to the sink after the pass.
const DEFERRED_BATCH: usize = 65_536;

/// Scan configuration.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Bytes per read. Must be a non-zero multiple of the record size.
    pub chunk_bytes: usize,
    /// Chunks read ahead of the parser (bounded channel capacity, ≥ 1).
    pub queue_depth: usize,
    /// Read `$MFT:$BITMAP` first and skip records it marks unused; chunks
    /// with no used records are not read at all.
    pub use_mft_bitmap: bool,
    /// Set to `true` from any thread to stop the scan. Records already
    /// delivered stay valid; [`ScanStats::cancelled`] is set.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            chunk_bytes: DEFAULT_CHUNK_BYTES,
            queue_depth: 4,
            use_mft_bitmap: false,
            cancel: None,
        }
    }
}

/// Counters describing a completed (or cancelled) scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// Records in the MFT according to its logical size.
    pub records_total: u64,
    /// In-use `FILE` records parsed (base and extension).
    pub in_use: u64,
    /// Free or never-initialized records (including those skipped by the bitmap).
    pub free: u64,
    /// Records skipped because `$MFT:$BITMAP` marked them unused.
    pub skipped_by_bitmap: u64,
    /// `BAAD` records.
    pub corrupt: u64,
    /// Records with a fixup mismatch (torn writes).
    pub torn: u64,
    /// Records with a signature other than `FILE`, `BAAD` or zero.
    pub bad_signature: u64,
    /// `FILE` records with inconsistent headers or attributes.
    pub malformed: u64,
    /// Records that could not be read from disk (bad sectors).
    pub unreadable: u64,
    /// In-use extension records seen.
    pub extension_records: u64,
    /// Extension records merged into their base.
    pub extensions_merged: u64,
    /// Extension records whose base was missing, stale, or had no attribute list.
    pub extensions_orphaned: u64,
    /// [`ScanRecord`]s delivered to the sink.
    pub records_emitted: u64,
    /// On-disk bytes of non-resident attributes that are not file content
    /// or directory indexes (`$ATTRIBUTE_LIST`, `$BITMAP`, `$EA`,
    /// `$LOGGED_UTILITY_STREAM`, non-resident reparse buffers).
    pub other_attr_allocated: u64,
    /// Bytes read from the volume by the scan.
    pub bytes_read: u64,
    /// Wall-clock time of the scan.
    pub elapsed: Duration,
    /// Whether the scan stopped early because of cancellation.
    pub cancelled: bool,
}

/// One chunk read by the I/O thread.
struct Chunk {
    first: u64,
    count: usize,
    buf: AlignedBuf,
    /// Record indices within the chunk that could not be read.
    failed: Vec<usize>,
}

/// Per-record parse result produced on the rayon pool.
enum Item {
    Ready(ScanRecord, u64),
    Extension(Box<ParsedRecord>),
    Deferred(Box<ParsedRecord>),
    Free,
    SkippedByBitmap,
    Unreadable,
    Baad,
    Torn,
    BadSignature,
    Malformed,
}

impl<R: ReadAt + Sync> NtfsVolume<R> {
    /// Scans every MFT record and delivers merged [`ScanRecord`]s to `sink`
    /// in batches.
    ///
    /// Per-record problems (corrupt, torn, malformed, unreadable) are
    /// counted in the returned [`ScanStats`], never fatal.
    ///
    /// # Errors
    ///
    /// Invalid options, an unreadable `$MFT:$BITMAP` (when requested), or a
    /// chunk where not a single record could be read (device gone).
    ///
    /// # Example
    ///
    /// ```
    /// use strata_ntfs::test_image::{Geometry, ImageBuilder};
    /// use strata_ntfs::{NtfsVolume, ScanOptions};
    /// let img = ImageBuilder::new(Geometry::default()).with_system_files().finish();
    /// let vol = NtfsVolume::open(img).unwrap();
    /// let mut n = 0;
    /// let stats = vol.scan(&ScanOptions::default(), |batch| n += batch.len()).unwrap();
    /// assert_eq!(stats.records_emitted, n as u64);
    /// ```
    pub fn scan<F: FnMut(Vec<ScanRecord>)>(
        &self,
        opts: &ScanOptions,
        mut sink: F,
    ) -> Result<ScanStats> {
        let started = Instant::now();
        let layout = self.layout();
        let rs = layout.record_size as usize;
        if opts.chunk_bytes == 0 || !opts.chunk_bytes.is_multiple_of(rs) {
            return Err(NtfsError::InvalidOption(format!(
                "chunk size {} is not a non-zero multiple of the {rs}-byte record size",
                opts.chunk_bytes
            )));
        }
        let chunk_records = opts.chunk_bytes / rs;
        let readable = layout.readable_records();
        let mut stats = ScanStats {
            records_total: layout.record_count(),
            ..ScanStats::default()
        };
        // Records past the initialized size were never written: free by definition.
        stats.free = stats.records_total.saturating_sub(readable);

        let bitmap = if opts.use_mft_bitmap {
            self.mft_bitmap()?
        } else {
            None
        };
        let in_use_bit = |n: u64| -> bool {
            bitmap.as_ref().is_none_or(|b| {
                usize::try_from(n / 8)
                    .ok()
                    .and_then(|i| b.get(i))
                    .is_none_or(|byte| byte & (1 << (n % 8)) != 0)
            })
        };

        let mut plan: Vec<(u64, usize)> = Vec::new();
        let mut first = 0u64;
        while first < readable {
            let count = (readable - first).min(chunk_records as u64) as usize;
            if (first..first + count as u64).any(in_use_bit) {
                plan.push((first, count));
            } else {
                stats.free += count as u64;
                stats.skipped_by_bitmap += count as u64;
            }
            first += count as u64;
        }

        let cancelled = || {
            opts.cancel
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Relaxed))
        };
        let base_opts = self.parse_options();
        let cs = self.boot().cluster_size;
        let align = self.reader().alignment();
        let mut deferred: BTreeMap<u64, ParsedRecord> = BTreeMap::new();
        let mut extensions: HashMap<u64, Vec<ParsedRecord>> = HashMap::new();
        let mut bytes_read = 0u64;
        let mut io_error: Option<NtfsError> = None;

        std::thread::scope(|scope| {
            let (tx, rx) =
                bounded::<std::result::Result<(Chunk, u64), NtfsError>>(opts.queue_depth.max(1));
            let (recycle_tx, recycle_rx) = crossbeam_channel::unbounded::<AlignedBuf>();
            let plan = &plan;
            let cancelled = &cancelled;
            scope.spawn(move || {
                for &(first, count) in plan {
                    if cancelled() {
                        break;
                    }
                    let buf = recycle_rx
                        .try_recv()
                        .unwrap_or_else(|_| AlignedBuf::new(opts.chunk_bytes, align));
                    let msg = self.read_chunk(first, count, buf);
                    let failed = msg.is_err();
                    if tx.send(msg).is_err() || failed {
                        break;
                    }
                }
            });

            for msg in &rx {
                let (mut chunk, n_bytes) = match msg {
                    Ok(c) => c,
                    Err(e) => {
                        io_error = Some(e);
                        break;
                    }
                };
                bytes_read += n_bytes;
                let failed = std::mem::take(&mut chunk.failed);
                let first = chunk.first;
                let items: Vec<Item> = chunk.buf.as_mut_slice()[..chunk.count * rs]
                    .par_chunks_mut(rs)
                    .enumerate()
                    .map(|(i, rec)| {
                        let n = first + i as u64;
                        if failed.binary_search(&i).is_ok() {
                            return Item::Unreadable;
                        }
                        if !in_use_bit(n) {
                            return Item::SkippedByBitmap;
                        }
                        classify(parse_record(rec, n, &base_opts.for_record(n)), cs)
                    })
                    .collect();
                let mut batch = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        Item::Ready(r, other) => {
                            stats.in_use += 1;
                            stats.other_attr_allocated =
                                stats.other_attr_allocated.saturating_add(other);
                            batch.push(r);
                        }
                        Item::Extension(p) => {
                            stats.in_use += 1;
                            stats.extension_records += 1;
                            stats.other_attr_allocated =
                                stats.other_attr_allocated.saturating_add(p.other_allocated);
                            let base = p.base.map_or(0, |b| b.record());
                            extensions.entry(base).or_default().push(*p);
                        }
                        Item::Deferred(p) => {
                            stats.in_use += 1;
                            stats.other_attr_allocated =
                                stats.other_attr_allocated.saturating_add(p.other_allocated);
                            deferred.insert(p.record, *p);
                        }
                        Item::Free => stats.free += 1,
                        Item::SkippedByBitmap => {
                            stats.free += 1;
                            stats.skipped_by_bitmap += 1;
                        }
                        Item::Unreadable => stats.unreadable += 1,
                        Item::Baad => stats.corrupt += 1,
                        Item::Torn => stats.torn += 1,
                        Item::BadSignature => stats.bad_signature += 1,
                        Item::Malformed => stats.malformed += 1,
                    }
                }
                let _ = recycle_tx.send(chunk.buf);
                if !batch.is_empty() {
                    stats.records_emitted += batch.len() as u64;
                    sink(batch);
                }
                if cancelled() {
                    stats.cancelled = true;
                    break;
                }
            }
            // Dropping the receiver unblocks the I/O thread if we stopped early.
            drop(rx);
        });
        if let Some(e) = io_error {
            return Err(e);
        }
        stats.bytes_read = bytes_read;

        let mut batch = Vec::new();
        for (n, base) in deferred {
            let mut exts = extensions.remove(&n).unwrap_or_default();
            let base_ref = base.file_ref();
            let before = exts.len();
            exts.retain(|e| e.base == Some(base_ref));
            stats.extensions_orphaned += (before - exts.len()) as u64;
            stats.extensions_merged += exts.len() as u64;
            let complete = self.extensions_complete(&base, &exts);
            let reparse = self.resolve_reparse(&base, &exts);
            let mut rec = assemble(base, exts, reparse, cs);
            rec.flags.set(strata_core::EntryFlags::PARTIAL, !complete);
            batch.push(rec);
            if batch.len() >= DEFERRED_BATCH {
                stats.records_emitted += batch.len() as u64;
                sink(std::mem::take(&mut batch));
            }
        }
        if !batch.is_empty() {
            stats.records_emitted += batch.len() as u64;
            sink(batch);
        }
        stats.extensions_orphaned += extensions.values().map(|v| v.len() as u64).sum::<u64>();
        stats.elapsed = started.elapsed();
        Ok(stats)
    }

    /// Reads `count` records starting at `first` into `buf`. If the bulk read
    /// fails, falls back to per-record reads so one bad sector costs one
    /// record, not a whole chunk.
    fn read_chunk(&self, first: u64, count: usize, mut buf: AlignedBuf) -> Result<(Chunk, u64)> {
        let layout = self.layout();
        let rs = layout.record_size as usize;
        let len = count * rs;
        let segs = first
            .checked_mul(rs as u64)
            .and_then(|start| layout.map(start, len as u64))
            .ok_or(NtfsError::OutOfRange(first))?;
        let dst = &mut buf.as_mut_slice()[..len];
        if read_segments(self.reader(), &segs, dst).is_ok() {
            return Ok((
                Chunk {
                    first,
                    count,
                    buf,
                    failed: Vec::new(),
                },
                len as u64,
            ));
        }
        let mut failed = Vec::new();
        let mut last_err = None;
        for (i, rec) in dst.chunks_exact_mut(rs).enumerate() {
            let n = first + i as u64;
            let res = layout
                .map(n * rs as u64, rs as u64)
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))
                .and_then(|s| read_segments(self.reader(), &s, rec));
            if let Err(e) = res {
                rec.fill(0);
                failed.push(i);
                last_err = Some(e);
            }
        }
        if failed.len() == count
            && let Some(e) = last_err
        {
            return Err(NtfsError::Io(e));
        }
        let bytes = ((count - failed.len()) * rs) as u64;
        Ok((
            Chunk {
                first,
                count,
                buf,
                failed,
            },
            bytes,
        ))
    }
}

fn classify(outcome: RecordOutcome, cluster_size: u64) -> Item {
    match outcome {
        RecordOutcome::InUse(p) => {
            if p.base.is_some() {
                Item::Extension(p)
            } else if p.needs_completion() {
                Item::Deferred(p)
            } else {
                let other = p.other_allocated;
                Item::Ready(assemble(*p, Vec::new(), None, cluster_size), other)
            }
        }
        RecordOutcome::Free => Item::Free,
        RecordOutcome::Baad => Item::Baad,
        RecordOutcome::Torn => Item::Torn,
        RecordOutcome::BadSignature => Item::BadSignature,
        RecordOutcome::Malformed(_) => Item::Malformed,
    }
}
