//! The full-MFT scan pipeline.
//!
//! Three stages joined by bounded channels:
//!
//! 1. **Reader** (own thread): walks the read plan, which is the MFT in VCN
//!    order cut into chunks, minus the all-free regions `$MFT:$BITMAP`
//!    reveals. With a [`QueuedReader`](crate::QueuedReader) it keeps up to
//!    [`ScanOptions::io_depth`] chunk reads in flight; otherwise it reads one
//!    chunk at a time. Chunks leave in plan order either way.
//! 2. **Parser** (own thread): fans each chunk out to the `rayon` pool for
//!    fixups and parsing, then sorts the results into ready records,
//!    extension records and records that need completion.
//! 3. **Caller's thread**: hands each chunk's ready records to the sink,
//!    while the stages above work on the next chunks.
//!
//! Records that need other records (attribute lists) or disk reads
//! (non-resident reparse buffers) are completed after the pass, together
//! with every extension record, merged by base reference.
//!
//! Output order: base records in record-number order as chunks complete,
//! then the deferred records in record-number order. The order does not
//! depend on I/O timing.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use rayon::prelude::*;
use strata_core::ScanRecord;

use crate::assemble::assemble;
use crate::error::{NtfsError, Result};
use crate::io::{AlignedBuf, QueuedReader, ReadAt};
use crate::overlapped::{Batch, Part};
use crate::record::{ParsedRecord, RecordOutcome, parse_record};
use crate::volume::{NtfsVolume, read_segments};

/// Default read size per I/O request.
pub const DEFAULT_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Default number of chunk reads kept in flight when the source supports
/// queued reads.
pub const DEFAULT_IO_DEPTH: usize = 8;

/// Upper bound on records per batch handed to the sink after the pass.
const DEFERRED_BATCH: usize = 65_536;

/// Runs of unused records shorter than this are read through rather than
/// skipped: one extra request costs more than reading a few hundred KiB.
const MIN_SKIP_BYTES: u64 = 256 * 1024;

/// Largest single queued read. Keeps lengths inside `ReadFile`'s `u32`.
const MAX_PART: usize = 64 * 1024 * 1024;

/// Scan configuration.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Bytes per read. Must be a non-zero multiple of the record size.
    pub chunk_bytes: usize,
    /// Chunks read ahead of the parser, and parsed chunks waiting for the
    /// sink (bounded channel capacities, ≥ 1).
    pub queue_depth: usize,
    /// Chunk reads kept in flight at once when the source offers queued
    /// reads ([`ReadAt::queued`]). 1 reads one chunk at a time.
    pub io_depth: usize,
    /// Read `$MFT:$BITMAP` first and skip records it marks unused; regions
    /// with no used records are not read at all. If the bitmap cannot be
    /// read, every record is read instead.
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
            io_depth: DEFAULT_IO_DEPTH,
            use_mft_bitmap: true,
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
    /// Read requests issued to the volume by the scan.
    pub reads: u64,
    /// Most reads the scan had in flight at once (1 without queued reads).
    pub peak_in_flight: u64,
    /// Time the reader spent with reads outstanding (issuing them or
    /// waiting for them), excluding time it was blocked on a full queue.
    pub read_time: Duration,
    /// Time the parser sat idle waiting for the next chunk to arrive. Close
    /// to [`ScanStats::elapsed`] means the scan is I/O bound.
    pub io_wait: Duration,
    /// Wall-clock time spent parsing chunks (fixups, attributes, assembly of
    /// self-contained records) on the thread pool.
    pub parse_time: Duration,
    /// Wall-clock time spent completing records that need other records
    /// (attribute lists) or extra reads, after the pass.
    pub assemble_time: Duration,
    /// Time spent inside the caller's sink.
    pub sink_time: Duration,
    /// Wall-clock time of the scan.
    pub elapsed: Duration,
    /// Whether the scan stopped early because of cancellation.
    pub cancelled: bool,
}

/// One chunk read by the reader stage.
struct Chunk {
    first: u64,
    count: usize,
    buf: AlignedBuf,
    /// Record indices within the chunk that could not be read.
    failed: Vec<usize>,
    /// Bytes actually read from the volume.
    bytes: u64,
}

/// A chunk read that is either finished or still in flight.
enum InFlight<'q> {
    Done(Result<Chunk>),
    Queued {
        first: u64,
        count: usize,
        batch: Batch<'q>,
    },
}

/// What the reader stage reports when it finishes.
#[derive(Default)]
struct ReadTotals {
    reads: u64,
    peak_in_flight: u64,
    read_time: Duration,
}

/// What the parser stage hands back after the pass.
struct ParseOutput {
    stats: ScanStats,
    deferred: BTreeMap<u64, ParsedRecord>,
    extensions: HashMap<u64, Vec<ParsedRecord>>,
    error: Option<NtfsError>,
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
    /// Invalid options, or a chunk where not a single record could be read
    /// (device gone).
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
        let chunk_records = (opts.chunk_bytes / rs) as u64;
        let readable = layout.readable_records();
        let mut stats = ScanStats {
            records_total: layout.record_count(),
            ..ScanStats::default()
        };
        // Records past the initialized size were never written: free by definition.
        stats.free = stats.records_total.saturating_sub(readable);

        // NOTE: the bitmap only narrows what is read, so a bitmap that cannot
        // be read costs speed, not results.
        let bitmap = if opts.use_mft_bitmap {
            self.mft_bitmap().ok().flatten()
        } else {
            None
        };
        let in_use_bit = |n: u64| -> bool { record_bit(bitmap.as_deref(), n) };

        let queued = if opts.io_depth > 1 {
            self.reader().queued()
        } else {
            None
        };
        let align = self
            .reader()
            .alignment()
            .max(queued.map_or(1, QueuedReader::alignment))
            .max(1)
            .next_power_of_two();
        let align_records = (align / rs).max(1) as u64;
        let min_skip = (MIN_SKIP_BYTES / rs as u64).max(align_records);
        let plan = read_plan(
            readable,
            chunk_records,
            align_records,
            min_skip,
            bitmap.as_deref(),
        );
        let planned: u64 = plan.iter().map(|&(_, c)| c as u64).sum();
        stats.free += readable - planned;
        stats.skipped_by_bitmap += readable - planned;

        let cancelled = || {
            opts.cancel
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Relaxed))
        };
        let depth = if queued.is_some() { opts.io_depth } else { 1 };
        let mut emitted = 0u64;
        let mut sink_time = Duration::ZERO;
        let mut stopped = false;

        let (totals, parsed) = std::thread::scope(|scope| {
            let (chunk_tx, chunk_rx) = bounded::<Result<Chunk>>(opts.queue_depth.max(1));
            let (batch_tx, batch_rx) = bounded::<Vec<ScanRecord>>(opts.queue_depth.max(1));
            let (recycle_tx, recycle_rx) = unbounded::<AlignedBuf>();
            let plan = &plan;
            let cancelled = &cancelled;
            let in_use_bit = &in_use_bit;
            let reader = scope.spawn(move || {
                let new_buf = || AlignedBuf::new(opts.chunk_bytes, align);
                self.read_chunks(
                    plan,
                    queued,
                    depth,
                    &new_buf,
                    &recycle_rx,
                    &chunk_tx,
                    cancelled,
                )
            });
            let parser = scope.spawn(move || {
                self.parse_chunks(stats, &chunk_rx, &batch_tx, &recycle_tx, in_use_bit)
            });

            for batch in &batch_rx {
                emitted += batch.len() as u64;
                let t = Instant::now();
                sink(batch);
                sink_time += t.elapsed();
                if cancelled() {
                    stopped = true;
                    break;
                }
            }
            // Dropping the receiver unblocks the other stages if we stopped early.
            drop(batch_rx);
            let parsed = parser
                .join()
                .unwrap_or_else(|p| std::panic::resume_unwind(p));
            let totals = reader
                .join()
                .unwrap_or_else(|p| std::panic::resume_unwind(p));
            (totals, parsed)
        });
        let ParseOutput {
            mut stats,
            deferred,
            mut extensions,
            error,
        } = parsed;
        if let Some(e) = error {
            return Err(e);
        }
        stats.records_emitted = emitted;
        stats.sink_time = sink_time;
        stats.cancelled = stopped || cancelled();
        stats.reads = totals.reads;
        stats.peak_in_flight = totals.peak_in_flight;
        stats.read_time = totals.read_time;

        let assemble_started = Instant::now();
        let sink_before = stats.sink_time;
        let cs = self.boot().cluster_size;
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
                deliver(&mut stats, &mut sink, std::mem::take(&mut batch));
            }
        }
        if !batch.is_empty() {
            deliver(&mut stats, &mut sink, batch);
        }
        stats.extensions_orphaned += extensions.values().map(|v| v.len() as u64).sum::<u64>();
        stats.assemble_time = assemble_started
            .elapsed()
            .saturating_sub(stats.sink_time - sink_before);
        stats.elapsed = started.elapsed();
        Ok(stats)
    }

    /// The reader stage: reads every planned chunk, keeping up to `depth`
    /// reads in flight, and sends the chunks in plan order. Stops at the
    /// first fatal read error, on cancellation, or when the parser hangs up.
    #[allow(clippy::too_many_arguments)]
    fn read_chunks(
        &self,
        plan: &[(u64, usize)],
        queued: Option<&QueuedReader>,
        depth: usize,
        new_buf: &dyn Fn() -> AlignedBuf,
        recycle: &Receiver<AlignedBuf>,
        tx: &Sender<Result<Chunk>>,
        cancelled: &dyn Fn() -> bool,
    ) -> ReadTotals {
        let started = Instant::now();
        let mut blocked = Duration::ZERO;
        let mut totals = ReadTotals::default();
        let mut ring: VecDeque<InFlight<'_>> = VecDeque::with_capacity(depth);
        let mut send = |chunk: Result<Chunk>| -> bool {
            let fatal = chunk.is_err();
            let t = Instant::now();
            let delivered = tx.send(chunk).is_ok();
            blocked += t.elapsed();
            delivered && !fatal
        };
        let mut open = true;
        for &(first, count) in plan {
            if cancelled() {
                open = false;
                break;
            }
            while open && ring.len() >= depth.max(1) {
                if let Some(f) = ring.pop_front() {
                    open = send(self.finish_read(f, new_buf, &mut totals));
                }
            }
            if !open {
                break;
            }
            let buf = recycle.try_recv().unwrap_or_else(|_| new_buf());
            ring.push_back(self.begin_read(queued, first, count, buf, new_buf, &mut totals));
            let in_flight = ring
                .iter()
                .filter(|f| matches!(f, InFlight::Queued { .. }))
                .count()
                .max(1);
            totals.peak_in_flight = totals.peak_in_flight.max(in_flight as u64);
        }
        while open && let Some(f) = ring.pop_front() {
            if cancelled() {
                break;
            }
            open = send(self.finish_read(f, new_buf, &mut totals));
        }
        // Whatever is still in `ring` is cancelled and waited for on drop.
        drop(ring);
        totals.read_time = started.elapsed().saturating_sub(blocked);
        totals
    }

    /// Starts reading one chunk. Without a queued reader the read happens
    /// here, synchronously. Sparse and unaligned pieces are always filled
    /// synchronously; the rest is issued as overlapped reads.
    fn begin_read<'q>(
        &self,
        queued: Option<&'q QueuedReader>,
        first: u64,
        count: usize,
        mut buf: AlignedBuf,
        new_buf: &dyn Fn() -> AlignedBuf,
        totals: &mut ReadTotals,
    ) -> InFlight<'q> {
        let Some(q) = queued else {
            return InFlight::Done(self.read_chunk(first, count, buf, new_buf, totals));
        };
        let layout = self.layout();
        let rs = layout.record_size as usize;
        let len = count * rs;
        let Some(segs) = first
            .checked_mul(rs as u64)
            .and_then(|start| layout.map(start, len as u64))
        else {
            return InFlight::Done(Err(NtfsError::OutOfRange(first)));
        };
        let align = q.alignment().max(self.reader().alignment());
        let mut parts = Vec::with_capacity(segs.len());
        let mut pos = 0usize;
        let mut sync_failed = false;
        for s in &segs {
            let Some(n) = usize::try_from(s.len).ok().filter(|&n| pos + n <= len) else {
                sync_failed = true;
                break;
            };
            let dst = &mut buf.as_mut_slice()[pos..pos + n];
            match s.disk {
                None => dst.fill(0),
                Some(off)
                    if off.is_multiple_of(align as u64)
                        && n.is_multiple_of(align)
                        && pos.is_multiple_of(align) =>
                {
                    let mut done = 0;
                    while done < n {
                        let piece = (n - done).min(MAX_PART);
                        parts.push(Part {
                            offset: off + done as u64,
                            at: pos + done,
                            len: piece,
                        });
                        done += piece;
                    }
                }
                Some(off) => {
                    totals.reads += 1;
                    if self.reader().read_at(off, dst).is_err() {
                        sync_failed = true;
                        break;
                    }
                }
            }
            pos += n;
        }
        if sync_failed {
            return InFlight::Done(self.read_records(first, count, buf, new_buf, totals));
        }
        totals.reads += parts.len() as u64;
        InFlight::Queued {
            first,
            count,
            batch: q.start(buf, &parts),
        }
    }

    /// Waits for a chunk read. A failed queued read is retried record by
    /// record, so one bad sector costs one record.
    fn finish_read(
        &self,
        f: InFlight<'_>,
        new_buf: &dyn Fn() -> AlignedBuf,
        totals: &mut ReadTotals,
    ) -> Result<Chunk> {
        match f {
            InFlight::Done(c) => c,
            InFlight::Queued {
                first,
                count,
                batch,
            } => match batch.wait() {
                (buf, Ok(())) => Ok(Chunk {
                    first,
                    count,
                    buf,
                    failed: Vec::new(),
                    bytes: (count * self.layout().record_size as usize) as u64,
                }),
                (buf, Err(_)) => self.read_records(first, count, buf, new_buf, totals),
            },
        }
    }

    /// Reads `count` records starting at `first` into `buf` with one
    /// synchronous read per extent. If that fails, falls back to per-record
    /// reads so one bad sector costs one record, not a whole chunk.
    fn read_chunk(
        &self,
        first: u64,
        count: usize,
        mut buf: AlignedBuf,
        new_buf: &dyn Fn() -> AlignedBuf,
        totals: &mut ReadTotals,
    ) -> Result<Chunk> {
        let layout = self.layout();
        let rs = layout.record_size as usize;
        let len = count * rs;
        let segs = first
            .checked_mul(rs as u64)
            .and_then(|start| layout.map(start, len as u64))
            .ok_or(NtfsError::OutOfRange(first))?;
        totals.reads += segs.iter().filter(|s| s.disk.is_some()).count() as u64;
        let dst = &mut buf.as_mut_slice()[..len];
        if read_segments(self.reader(), &segs, dst).is_ok() {
            return Ok(Chunk {
                first,
                count,
                buf,
                failed: Vec::new(),
                bytes: len as u64,
            });
        }
        self.read_records(first, count, buf, new_buf, totals)
    }

    /// Reads `count` records one at a time, zero-filling and listing the
    /// ones that fail.
    fn read_records(
        &self,
        first: u64,
        count: usize,
        mut buf: AlignedBuf,
        new_buf: &dyn Fn() -> AlignedBuf,
        totals: &mut ReadTotals,
    ) -> Result<Chunk> {
        let layout = self.layout();
        let rs = layout.record_size as usize;
        let len = count * rs;
        if buf.len() < len {
            buf = new_buf();
        }
        let mut failed = Vec::new();
        let mut last_err = None;
        for (i, rec) in buf.as_mut_slice()[..len].chunks_exact_mut(rs).enumerate() {
            let n = first + i as u64;
            totals.reads += 1;
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
        Ok(Chunk {
            first,
            count,
            buf,
            failed,
            bytes,
        })
    }

    /// The parser stage: parses each chunk on the rayon pool, keeps
    /// extension and deferred records for after the pass, and forwards
    /// ready records to the sink stage.
    fn parse_chunks(
        &self,
        mut stats: ScanStats,
        rx: &Receiver<Result<Chunk>>,
        tx: &Sender<Vec<ScanRecord>>,
        recycle: &Sender<AlignedBuf>,
        in_use_bit: &(dyn Fn(u64) -> bool + Sync),
    ) -> ParseOutput {
        let rs = self.layout().record_size as usize;
        let base_opts = self.parse_options();
        let cs = self.boot().cluster_size;
        let mut deferred = BTreeMap::new();
        let mut extensions: HashMap<u64, Vec<ParsedRecord>> = HashMap::new();
        let mut error = None;
        loop {
            let waited = Instant::now();
            let Ok(msg) = rx.recv() else { break };
            stats.io_wait += waited.elapsed();
            let mut chunk = match msg {
                Ok(c) => c,
                Err(e) => {
                    error = Some(e);
                    break;
                }
            };
            let parse_started = Instant::now();
            stats.bytes_read += chunk.bytes;
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
            let _ = recycle.send(chunk.buf);
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
            stats.parse_time += parse_started.elapsed();
            if !batch.is_empty() && tx.send(batch).is_err() {
                break;
            }
        }
        ParseOutput {
            stats,
            deferred,
            extensions,
            error,
        }
    }
}

/// Hands a batch completed after the pass to the sink.
fn deliver<F: FnMut(Vec<ScanRecord>)>(stats: &mut ScanStats, sink: &mut F, batch: Vec<ScanRecord>) {
    stats.records_emitted += batch.len() as u64;
    let t = Instant::now();
    sink(batch);
    stats.sink_time += t.elapsed();
}

/// Whether `bitmap` marks record `n` used. Records past the end of the
/// bitmap, and every record when there is no bitmap, count as used.
fn record_bit(bitmap: Option<&[u8]>, n: u64) -> bool {
    bitmap.is_none_or(|b| {
        usize::try_from(n / 8)
            .ok()
            .and_then(|i| b.get(i))
            .is_none_or(|byte| byte & (1 << (n % 8)) != 0)
    })
}

/// First record in `from..limit` whose bit equals `used`, or `limit`.
fn next_with(bitmap: &[u8], mut n: u64, limit: u64, used: bool) -> u64 {
    let skip = if used { 0x00 } else { 0xFF };
    while n < limit {
        // PERF: whole bytes that cannot match are skipped eight records at a time.
        if n.is_multiple_of(8)
            && usize::try_from(n / 8)
                .ok()
                .and_then(|i| bitmap.get(i))
                .is_some_and(|&b| b == skip)
        {
            n += 8;
            continue;
        }
        if record_bit(Some(bitmap), n) == used {
            return n;
        }
        n += 1;
    }
    limit
}

/// Cuts records `0..readable` into reads of at most `chunk` records.
///
/// With a bitmap, runs of at least `min_skip` unused records are left out
/// entirely. Read boundaries next to a skipped run are rounded outwards to
/// multiples of `align` records so reads stay sector aligned; records the
/// rounding pulls in are read but still skipped by the parser.
fn read_plan(
    readable: u64,
    chunk: u64,
    align: u64,
    min_skip: u64,
    bitmap: Option<&[u8]>,
) -> Vec<(u64, usize)> {
    let chunk = chunk.max(1);
    let align = align.max(1);
    let mut extents: Vec<(u64, u64)> = Vec::new();
    match bitmap {
        None => extents.push((0, readable)),
        Some(b) => {
            let mut n = 0;
            while n < readable {
                let start = next_with(b, n, readable, true);
                if start >= readable {
                    break;
                }
                let end = next_with(b, start, readable, false);
                let s = start - start % align;
                let e = end.div_ceil(align).saturating_mul(align).min(readable);
                match extents.last_mut() {
                    Some(last) if s <= last.1.saturating_add(min_skip) => last.1 = last.1.max(e),
                    _ => extents.push((s, e)),
                }
                n = end;
            }
        }
    }
    let mut plan = Vec::new();
    for (s, e) in extents {
        let mut f = s;
        while f < e {
            let c = (e - f).min(chunk);
            plan.push((f, c as usize));
            f += c;
        }
    }
    plan
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A bitmap of `records` bits with the half-open `(start, end)` runs set.
    fn bits(used: &[(u64, u64)], records: u64) -> Vec<u8> {
        let mut b = vec![0u8; records.div_ceil(8) as usize];
        for &(start, end) in used {
            for n in start..end {
                b[(n / 8) as usize] |= 1 << (n % 8);
            }
        }
        b
    }

    #[test]
    fn plan_without_bitmap_covers_everything_in_chunks() {
        assert_eq!(read_plan(10, 4, 1, 1, None), vec![(0, 4), (4, 4), (8, 2)]);
        assert!(read_plan(0, 4, 1, 1, None).is_empty());
    }

    #[test]
    fn plan_skips_long_free_runs_and_keeps_alignment() {
        let b = bits(&[(3, 5), (6, 7), (100, 101), (2000, 2003)], 4096);
        let plan = read_plan(4096, 64, 4, 16, Some(&b));
        // 3..7 and 100 are 93 records apart (> 16): separate reads, aligned to 4.
        assert_eq!(plan, vec![(0, 8), (100, 4), (2000, 4)]);
        for &(f, c) in &plan {
            assert_eq!(f % 4, 0);
            assert!(c % 4 == 0);
        }
        // Short gaps are read through.
        let b = bits(&[(0, 1), (10, 11)], 64);
        assert_eq!(read_plan(64, 64, 1, 16, Some(&b)), vec![(0, 11)]);
    }

    #[test]
    fn plan_reads_records_past_the_bitmap_and_clamps_rounding() {
        let b = bits(&[], 16);
        assert_eq!(read_plan(30, 8, 4, 4, Some(&b)), vec![(16, 8), (24, 6)]);
        let b = bits(&[(29, 30)], 30);
        assert_eq!(read_plan(30, 64, 4, 4, Some(&b)), vec![(28, 2)]);
        let b = vec![0xFF; 4];
        assert_eq!(
            read_plan(32, 10, 1, 1, Some(&b)),
            vec![(0, 10), (10, 10), (20, 10), (30, 2)]
        );
    }
}
