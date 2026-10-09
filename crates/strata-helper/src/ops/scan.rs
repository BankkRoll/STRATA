//! `ScanVolume`: raw MFT scan streamed as `ScanBatch` frames (SPEC §6.4).
//!
//! Pipeline: the `strata-ntfs` scanner (its own I/O thread + rayon) hands
//! batches to a sink on this worker thread, which pushes them into a
//! bounded channel; a writer thread re-batches them to the client's batch
//! size and writes frames to the pipe.
//!
//! Backpressure: when the client reads slowly, pipe writes block, the
//! channel fills, the sink blocks, and the scanner's own bounded read-ahead
//! stops reading. Memory stays bounded by `QUEUE_DEPTH` scanner batches
//! plus one outgoing batch, however slow the client is. If the client
//! disconnects, the writer's send fails, it drops the channel, the sink sees
//! the closed channel and sets the scan's cancel flag.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, bounded};
use strata_core::{EntryFlags, ScanRecord};
use strata_ipc::protocol::{AuditOp, AuditPhase, Response, ScanOptions, ScanProgress, ScanStats};

use super::RequestCtx;
use crate::audit::AuditSubject;
use crate::error::HelperError;

/// Scanner batches buffered between the scanner and the pipe writer.
pub const QUEUE_DEPTH: usize = 4;
/// Smallest `ScanBatch` the helper sends (except the last).
pub const MIN_BATCH: u32 = 64;
/// Largest `ScanBatch` in records.
pub const MAX_BATCH: u32 = 65_536;
/// Largest `ScanBatch` in (estimated) encoded bytes, well under the 64 MiB
/// frame limit even for records with 1024 long hardlink names.
pub const MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;
/// Bounds for the progress interval.
const MIN_PROGRESS: Duration = Duration::from_millis(50);
const MAX_PROGRESS: Duration = Duration::from_secs(10);

/// Runs a scan request: audit, scan, stream, final progress and `ScanDone`.
///
/// # Errors
///
/// The volume cannot be opened or read, or the client disconnected.
pub fn scan_volume(
    ctx: &RequestCtx<'_>,
    volume: &str,
    options: ScanOptions,
) -> Result<(), HelperError> {
    let subject = AuditSubject {
        op: AuditOp::ScanVolume,
        volume: volume.to_owned(),
        file_ref: None,
        path: None,
    };
    ctx.audit(&subject, AuditPhase::Started, "")?;
    match run(ctx, volume, options) {
        Ok(stats) => {
            let detail = format!(
                "{} records{}",
                stats.records,
                if stats.cancelled { ", cancelled" } else { "" }
            );
            ctx.audit(&subject, AuditPhase::Succeeded, detail)?;
            ctx.send(Response::ScanDone { stats })
        }
        Err(e) if e.disconnected => Err(e),
        Err(e) => {
            ctx.audit(&subject, AuditPhase::Failed, e.message.clone())?;
            Err(e)
        }
    }
}

/// What the writer thread did.
#[derive(Debug, Default)]
struct Written {
    records: u64,
    max_record: u64,
    error: Option<HelperError>,
}

/// Rough encoded size of a record, for the per-frame byte budget.
fn estimate(r: &ScanRecord) -> usize {
    let links: usize = r.links.iter().map(|l| 12 + l.name.len() * 3).sum();
    let ads: usize = r.ads.iter().map(|a| 24 + a.name.len() * 3).sum();
    let reparse = r
        .reparse
        .as_ref()
        .map_or(0, |p| 8 + p.target.as_ref().map_or(0, |t| t.len() * 3));
    96 + links + ads + reparse
}

fn run(ctx: &RequestCtx<'_>, volume: &str, options: ScanOptions) -> Result<ScanStats, HelperError> {
    let vol = ctx.shared.volumes.open_fresh(volume)?;
    let record_size = u64::from(vol.layout().record_size);
    let bytes_total = vol.layout().readable_records().saturating_mul(record_size);
    let batch_size = options.batch_size.clamp(MIN_BATCH, MAX_BATCH) as usize;
    let interval = Duration::from_millis(u64::from(options.progress_interval_ms))
        .clamp(MIN_PROGRESS, MAX_PROGRESS);
    let cancel = ctx.cancel.flag();
    let ntfs_options = strata_ntfs::ScanOptions {
        cancel: Some(cancel.clone()),
        chunk_bytes: ctx.shared.scan_chunk_bytes,
        ..strata_ntfs::ScanOptions::default()
    };
    let (tx, rx) = bounded::<Vec<ScanRecord>>(QUEUE_DEPTH);
    let writer = Writer {
        ctx,
        batch_size,
        interval,
        include_metadata: options.include_metadata,
        record_size,
        bytes_total,
    };
    let (scanned, written) = std::thread::scope(|s| {
        let w = s.spawn(|| writer.run(&rx));
        let scanned = vol.scan(&ntfs_options, |batch| {
            if tx.send(batch).is_err() {
                cancel.store(true, Ordering::SeqCst);
            }
        });
        drop(tx);
        let written = w.join().unwrap_or_else(|_| Written {
            error: Some(HelperError::internal("scan writer panicked")),
            ..Written::default()
        });
        (scanned, written)
    });
    if let Some(e) = written.error {
        return Err(e);
    }
    let scanned = scanned?;
    ctx.send(Response::ScanProgress(ScanProgress {
        records: written.records,
        bytes_read: scanned.bytes_read,
        bytes_total: Some(bytes_total),
    }))?;
    Ok(ScanStats {
        records: written.records,
        corrupt: scanned.corrupt + scanned.torn + scanned.bad_signature + scanned.malformed,
        elapsed_ms: u64::try_from(scanned.elapsed.as_millis()).unwrap_or(u64::MAX),
        cancelled: scanned.cancelled || ctx.cancel.is_cancelled(),
    })
}

struct Writer<'a, 'b> {
    ctx: &'a RequestCtx<'b>,
    batch_size: usize,
    interval: Duration,
    include_metadata: bool,
    record_size: u64,
    bytes_total: u64,
}

impl Writer<'_, '_> {
    fn run(&self, rx: &Receiver<Vec<ScanRecord>>) -> Written {
        let mut out = Written::default();
        let mut pending: Vec<ScanRecord> = Vec::with_capacity(self.batch_size);
        let mut pending_bytes = 0usize;
        let mut last_progress = Instant::now();
        loop {
            let batch = match rx.recv_timeout(self.interval) {
                Ok(b) => Some(b),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            for r in batch.into_iter().flatten() {
                if !self.include_metadata && r.flags.contains(EntryFlags::NTFS_METADATA) {
                    continue;
                }
                out.max_record = out.max_record.max(r.id.record());
                pending_bytes += estimate(&r);
                pending.push(r);
                if pending.len() >= self.batch_size || pending_bytes >= MAX_BATCH_BYTES {
                    if let Err(e) = self.flush(&mut pending, &mut out) {
                        out.error = Some(e);
                        return out;
                    }
                    pending_bytes = 0;
                }
            }
            if last_progress.elapsed() >= self.interval {
                last_progress = Instant::now();
                // NOTE: the scanner reports bytes read only at the end; the
                // highest record number delivered so far is a faithful
                // position in the MFT, so progress is derived from it.
                let position = (out.max_record + 1).saturating_mul(self.record_size);
                let progress = Response::ScanProgress(ScanProgress {
                    records: out.records,
                    bytes_read: position.min(self.bytes_total),
                    bytes_total: Some(self.bytes_total),
                });
                if let Err(e) = self.ctx.send(progress) {
                    out.error = Some(e);
                    return out;
                }
            }
        }
        if let Err(e) = self.flush(&mut pending, &mut out) {
            out.error = Some(e);
        }
        out
    }

    fn flush(&self, pending: &mut Vec<ScanRecord>, out: &mut Written) -> Result<(), HelperError> {
        if pending.is_empty() {
            return Ok(());
        }
        let records = std::mem::replace(pending, Vec::with_capacity(self.batch_size));
        let n = records.len() as u64;
        self.ctx.send(Response::ScanBatch { records })?;
        out.records += n;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::{FileRef, NameLink, WideName};

    #[test]
    fn estimate_grows_with_names() {
        let mut r = ScanRecord {
            id: FileRef(1),
            links: vec![],
            attributes: 0,
            flags: EntryFlags::default(),
            times: Default::default(),
            fn_created: None,
            sizes: Default::default(),
            reparse: None,
            ads: vec![],
        };
        let base = estimate(&r);
        r.links.push(NameLink {
            parent: FileRef(5),
            name: WideName::from_str_lossless(&"x".repeat(255)),
        });
        assert!(estimate(&r) >= base + 255 * 2);
        // 1024 links of 255 characters each stay far below the frame limit
        // per record, and the byte budget keeps batches of them bounded.
        const { assert!(1024 * (12 + 255 * 3) < MAX_BATCH_BYTES) };
    }
}
