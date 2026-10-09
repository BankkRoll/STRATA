//! Completing base records that need more than their own MFT record.
//!
//! A base record with an `$ATTRIBUTE_LIST` keeps some attributes in
//! extension records, and one with a non-resident `$REPARSE_POINT` keeps its
//! reparse buffer in clusters. During a full scan every extension record
//! arrives in the scan stream anyway, so completion needs no MFT re-reads:
//!
//! - **During the pass**: resident attribute lists are decoded on the parse
//!   pool. Non-resident attribute lists and reparse buffers are sent to the
//!   [`ValueReader`] thread, which reads them in batches (queued, many in
//!   flight, when the source allows) while the MFT reads continue.
//!   Extension records are grouped by base record number as they arrive
//!   ([`ExtensionGroups`]); the parser thread has the time while it waits
//!   for reads.
//! - **After the pass**: [`merge`] pairs each deferred base with its group
//!   and assembles every pair on the `rayon` pool.
//!
//! The output is identical to completing each record on its own with
//! [`NtfsVolume::read_record`]-style logic: same extension filtering, same
//! partial flag, same reparse resolution.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use rayon::prelude::*;
use strata_core::{EntryFlags, Reparse, ScanRecord};

use crate::assemble::assemble;
use crate::attr::{parse_attr_list, parse_reparse};
use crate::io::{AlignedBuf, QueuedReader, ReadAt};
use crate::overlapped::Part;
use crate::record::{ParsedRecord, ReparseLoc, ValueLoc};
use crate::volume::{MAX_SMALL_VALUE, NtfsVolume, map_runs};

/// Values read per batch at most.
const BATCH_VALUES: usize = 256;
/// Buffer bytes per batch at most (one oversized value still gets a batch).
const BATCH_BYTES: usize = 8 * 1024 * 1024;

/// The holders an attribute list names, as far as completion is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Holders {
    /// The record has no attribute list: nothing to wait for.
    NoList,
    /// Extension record numbers the list names (sorted, deduplicated,
    /// excluding the base itself).
    Listed(Vec<u64>),
    /// The list is non-resident and has been requested from the value reader.
    Pending,
    /// The list could not be read or decoded.
    Unreadable,
}

impl Holders {
    /// Decodes an attribute list value read for base record `record`.
    pub(crate) fn decode(bytes: &[u8], record: u64) -> Self {
        match parse_attr_list(bytes) {
            Ok(entries) => {
                let mut h: Vec<u64> = entries
                    .iter()
                    .map(|e| e.holder.record())
                    .filter(|&h| h != record)
                    .collect();
                h.sort_unstable();
                h.dedup();
                Self::Listed(h)
            }
            Err(_) => Self::Unreadable,
        }
    }

    /// What is known about `base`'s holders without any disk read.
    pub(crate) fn of(base: &ParsedRecord) -> Self {
        match &base.attr_list {
            None => Self::NoList,
            Some(ValueLoc::Resident(v)) => Self::decode(v, base.record),
            Some(ValueLoc::NonResident { .. }) => Self::Pending,
        }
    }

    /// Whether every listed holder is among `extensions` (sorted by record).
    /// A holder that is torn, corrupt, unreadable or reused leaves
    /// attributes unaccounted for, so the record must be marked partial.
    pub(crate) fn complete(&self, extensions: &[ParsedRecord]) -> bool {
        match self {
            Self::NoList => true,
            Self::Listed(h) => h
                .iter()
                .all(|&h| extensions.binary_search_by_key(&h, |e| e.record).is_ok()),
            Self::Pending | Self::Unreadable => false,
        }
    }
}

/// A base record waiting for completion after the pass.
#[derive(Debug)]
pub(crate) struct Deferred {
    pub base: ParsedRecord,
    pub holders: Holders,
    /// The base's own non-resident reparse buffer, once read: `Some(None)`
    /// when it could not be read or decoded.
    pub reparse: Option<Option<Reparse>>,
}

impl Deferred {
    /// Wraps a parsed base record, decoding a resident attribute list.
    pub(crate) fn new(mut base: ParsedRecord) -> Self {
        let holders = Holders::of(&base);
        if matches!(base.attr_list, Some(ValueLoc::Resident(_))) {
            // PERF: the decoded holders are all completion needs; the bytes
            // would only sit in memory until the end of the pass.
            base.attr_list = None;
        }
        Self {
            base,
            holders,
            reparse: None,
        }
    }
}

/// What the value reader should do with a value once read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Want {
    /// An attribute list: decode its holders.
    List,
    /// A reparse buffer: decode it.
    Reparse,
}

/// A non-resident value needed to complete deferred record `slot`.
#[derive(Debug)]
pub(crate) struct ValueRequest {
    pub slot: usize,
    pub record: u64,
    pub want: Want,
    pub loc: ValueLoc,
}

/// A decoded value for deferred record `slot`.
#[derive(Debug)]
pub(crate) enum Fetched {
    List(usize, Holders),
    Reparse(usize, Option<Reparse>),
}

/// What the value reader reports when it finishes.
#[derive(Debug, Default)]
pub(crate) struct ValueTotals {
    pub fetched: Vec<Fetched>,
    pub values: u64,
    pub busy: Duration,
}

/// One value's place in a batch buffer.
enum Piece {
    Zero { len: usize },
    Disk { at: usize, len: usize },
}

/// Reads the non-resident values deferred records need, alongside the pass.
pub(crate) struct ValueReader<'a, R> {
    volume: &'a NtfsVolume<R>,
    queued: Option<&'a QueuedReader>,
    align: usize,
}

impl<'a, R: ReadAt + Sync> ValueReader<'a, R> {
    pub(crate) fn new(volume: &'a NtfsVolume<R>, queued: Option<&'a QueuedReader>) -> Self {
        let align = volume
            .reader()
            .alignment()
            .max(queued.map_or(1, QueuedReader::alignment))
            .max(1)
            .next_power_of_two();
        Self {
            volume,
            queued,
            align,
        }
    }

    /// Serves requests until the sender hangs up. After cancellation the
    /// remaining requests are answered as unreadable without reading.
    pub(crate) fn run(
        &self,
        rx: &Receiver<ValueRequest>,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> ValueTotals {
        let mut totals = ValueTotals::default();
        let mut batch = Vec::with_capacity(BATCH_VALUES);
        while let Ok(first) = rx.recv() {
            batch.push(first);
            while batch.len() < BATCH_VALUES
                && let Ok(r) = rx.try_recv()
            {
                batch.push(r);
            }
            let started = Instant::now();
            if cancelled() {
                for r in batch.drain(..) {
                    totals.fetched.push(decode(&r, None));
                }
                continue;
            }
            totals.values += batch.len() as u64;
            self.read_batch(&mut batch, &mut totals.fetched);
            totals.busy += started.elapsed();
        }
        totals
    }

    /// Reads and decodes every request in `batch`, draining it.
    fn read_batch(&self, batch: &mut Vec<ValueRequest>, out: &mut Vec<Fetched>) {
        let Some(q) = self.queued else {
            for r in batch.drain(..) {
                let bytes = self.volume.read_value(&r.loc, MAX_SMALL_VALUE).ok();
                out.push(decode(&r, bytes.as_deref()));
            }
            return;
        };
        let mut start = 0;
        while start < batch.len() {
            let mut parts = Vec::new();
            let mut plans = Vec::new();
            let mut total = 0usize;
            let mut end = start;
            while end < batch.len() && (end == start || total < BATCH_BYTES) {
                plans.push(self.plan(&batch[end].loc, &mut parts, &mut total));
                end += 1;
            }
            let read = if parts.is_empty() {
                Some(AlignedBuf::new(0, 1))
            } else {
                match q.start(AlignedBuf::new(total, self.align), &parts).wait() {
                    (buf, Ok(())) => Some(buf),
                    (_, Err(_)) => None,
                }
            };
            for (r, plan) in batch[start..end].iter().zip(plans) {
                let bytes = match (&read, plan) {
                    (Some(buf), Some(pieces)) => Some(gather(buf.as_slice(), &pieces)),
                    // NOTE: a failed batch is retried value by value, so one
                    // bad sector costs one record's completeness.
                    _ => self.volume.read_value(&r.loc, MAX_SMALL_VALUE).ok(),
                };
                out.push(decode(r, bytes.as_deref()));
            }
            start = end;
        }
        batch.clear();
    }

    /// Lays out one value's disk segments as aligned parts at the end of the
    /// batch buffer. `None` when the value cannot be mapped (it is then read
    /// on its own, which reports the same error as a single read).
    fn plan(&self, loc: &ValueLoc, parts: &mut Vec<Part>, total: &mut usize) -> Option<Vec<Piece>> {
        let ValueLoc::NonResident { runs, size } = loc else {
            return None;
        };
        if *size > MAX_SMALL_VALUE {
            return None;
        }
        let segs = map_runs(runs, self.volume.boot().cluster_size, 0, *size)?;
        let a = self.align as u64;
        let mut pieces = Vec::with_capacity(segs.len());
        let mut staged = Vec::with_capacity(segs.len());
        let mut at = *total;
        for s in segs {
            let len = usize::try_from(s.len).ok()?;
            match s.disk {
                None => pieces.push(Piece::Zero { len }),
                Some(off) => {
                    let first = off - off % a;
                    let last = off.checked_add(s.len)?.checked_next_multiple_of(a)?;
                    let span = usize::try_from(last - first).ok()?;
                    staged.push(Part {
                        offset: first,
                        at,
                        len: span,
                    });
                    pieces.push(Piece::Disk {
                        at: at + (off - first) as usize,
                        len,
                    });
                    at = at.checked_add(span)?;
                }
            }
        }
        parts.extend(staged);
        *total = at;
        Some(pieces)
    }
}

/// Copies one value out of a batch buffer.
fn gather(buf: &[u8], pieces: &[Piece]) -> Vec<u8> {
    let mut v = Vec::new();
    for p in pieces {
        match *p {
            Piece::Zero { len } => v.resize(v.len() + len, 0),
            Piece::Disk { at, len } => v.extend_from_slice(&buf[at..at + len]),
        }
    }
    v
}

fn decode(r: &ValueRequest, bytes: Option<&[u8]>) -> Fetched {
    match r.want {
        Want::List => Fetched::List(
            r.slot,
            bytes.map_or(Holders::Unreadable, |b| Holders::decode(b, r.record)),
        ),
        Want::Reparse => Fetched::Reparse(r.slot, bytes.and_then(|b| parse_reparse(b).ok())),
    }
}

/// Applies fetched values, pairs the `deferred` bases (in record order) with
/// their extension groups and assembles every pair on the `rayon` pool.
/// Records come out in base record order; `deliver` receives them in
/// batches of at most `batch` records. Returns the extension records merged
/// and orphaned (no valid base among `deferred`).
pub(crate) fn merge<R: ReadAt + Sync>(
    volume: &NtfsVolume<R>,
    mut deferred: Vec<Deferred>,
    extensions: ExtensionGroups,
    fetched: Vec<Fetched>,
    batch: usize,
    mut deliver: impl FnMut(Vec<ScanRecord>),
) -> (u64, u64) {
    for f in fetched {
        match f {
            Fetched::List(slot, h) => {
                if let Some(d) = deferred.get_mut(slot) {
                    d.holders = h;
                }
            }
            Fetched::Reparse(slot, r) => {
                if let Some(d) = deferred.get_mut(slot) {
                    d.reparse = Some(r);
                }
            }
        }
    }

    let total = extensions.count;
    let mut map = extensions.map;
    let groups: Vec<Vec<ParsedRecord>> = deferred
        .iter()
        .map(|d| map.remove(&d.base.record).unwrap_or_default())
        .collect();
    drop(map);

    let cs = volume.boot().cluster_size;
    let out: Vec<(ScanRecord, u64)> = deferred
        .into_par_iter()
        .zip(groups)
        .map(|(d, exts)| complete_one(volume, d, exts, cs))
        .collect();
    let merged: u64 = out.iter().map(|&(_, m)| m).sum();
    let mut records = Vec::with_capacity(batch.min(out.len()));
    for (r, _) in out {
        records.push(r);
        if records.len() >= batch.max(1) {
            deliver(std::mem::replace(&mut records, Vec::with_capacity(batch)));
        }
    }
    if !records.is_empty() {
        deliver(records);
    }
    (merged, total - merged)
}

/// Extension records grouped by base record number as the pass delivers
/// them, so grouping costs nothing after the pass.
#[derive(Debug, Default)]
pub(crate) struct ExtensionGroups {
    map: HashMap<u64, Vec<ParsedRecord>, BuildHasherDefault<RecordHasher>>,
    count: u64,
}

impl ExtensionGroups {
    /// Adds an extension record. Records must arrive in record order, which
    /// keeps every group in record order.
    pub(crate) fn push(&mut self, e: ParsedRecord) {
        let base = e.base.map_or(0, |b| b.record());
        self.map.entry(base).or_default().push(e);
        self.count += 1;
    }
}

/// A multiplicative hasher for record numbers. They are dense integers,
/// not attacker-chosen strings, so `SipHash`'s collision resistance only
/// costs time here.
#[derive(Debug, Default)]
struct RecordHasher(u64);

impl Hasher for RecordHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(self.0.rotate_left(8) ^ u64::from(b));
        }
    }

    fn write_u64(&mut self, n: u64) {
        let x = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = x ^ (x >> 32);
    }
}

/// Assembles one deferred base with the extension records that name it.
/// Returns the record and how many extensions were merged.
fn complete_one<R: ReadAt + Sync>(
    volume: &NtfsVolume<R>,
    d: Deferred,
    mut exts: Vec<ParsedRecord>,
    cluster_size: u64,
) -> (ScanRecord, u64) {
    let Deferred {
        base,
        holders,
        reparse,
    } = d;
    let base_ref = base.file_ref();
    exts.retain(|e| e.base == Some(base_ref));
    let merged = exts.len() as u64;
    let complete = holders.complete(&exts);
    let reparse = match (&base.reparse, reparse) {
        (Some(ReparseLoc::NonResident(_)), Some(r)) => r,
        _ if exts
            .iter()
            .any(|e| matches!(e.reparse, Some(ReparseLoc::NonResident(_)))) =>
        {
            volume.resolve_reparse(&base, &exts)
        }
        (Some(ReparseLoc::NonResident(_)), None) => volume.resolve_reparse(&base, &exts),
        _ => None,
    };
    let mut rec = assemble(base, exts, reparse, cluster_size);
    rec.flags.set(EntryFlags::PARTIAL, !complete);
    (rec, merged)
}
