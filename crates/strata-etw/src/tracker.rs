//! The event pipeline without any FFI: decode → map → aggregate.
//!
//! [`Tracker`] owns the decoder, the process and file maps and the
//! aggregator. The live monitor feeds it from the ETW callback; tests and
//! benchmarks feed it synthetic [`RawEvent`]s or decoded [`Event`]s with an
//! injected device map, process source and clock.
//!
//! Counting rules:
//! - **Bytes written:** `Write` events, except paging I/O
//!   (`IRP_PAGING_IO` / `IRP_SYNCHRONOUS_PAGING_IO`). A cached write is
//!   logged once in the writing process and again when the cache manager
//!   flushes it from the System process; counting only the first attributes
//!   it to the program and avoids double counting. Writes through memory
//!   mapped views only ever appear as paging I/O and are therefore not
//!   attributed.
//! - **Files created:** `CreateNewFile` events.
//! - **Files deleted:** `DeletePath` with a delete disposition, and `Create`
//!   with `FILE_DELETE_ON_CLOSE`.
//! - **Last writer:** every counted write and created file.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use strata_core::FileTime;
use strata_store::{Clock, Timestamp};
use strata_win::path::DeviceMap;

use crate::aggregate::{ActivityBatch, Aggregator, Counts, DirTotal, Window, WriterSummary};
use crate::decode::{
    Decoder, Event, EventKind, FILE_DELETE_ON_CLOSE, IRP_PAGING_IO, IRP_SYNCHRONOUS_PAGING_IO,
    RawEvent, file_id,
};
use crate::evidence::{Evidence, EvidenceConfig, evidence};
use crate::files::FileTable;
use crate::layout::Provider;
use crate::overhead::Sampler;
use crate::paths::{FileInfo, PathMapper};
use crate::processes::{ProcessSource, ProcessTable};

/// Pipeline counters (monotonic until [`Tracker::clear`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TrackerStats {
    /// Events offered to the tracker.
    pub events: u64,
    /// Events decoded and applied.
    pub decoded: u64,
    /// Events with an unknown layout or a truncated payload.
    pub decode_errors: u64,
    /// Writes skipped by sampling.
    pub sampled_out: u64,
    /// Paging writes skipped (counted at the original cached write).
    pub paging_writes: u64,
    /// Writes on a file whose name was never seen.
    pub unmapped_writes: u64,
    /// File events from a process that could not be identified.
    pub unattributed: u64,
}

/// The decode → map → aggregate pipeline.
#[derive(Debug)]
pub struct Tracker {
    decoder: Decoder,
    paths: PathMapper,
    files: FileTable,
    procs: ProcessTable,
    agg: Aggregator,
    sampler: Sampler,
    clock: Arc<dyn Clock>,
    stats: TrackerStats,
    last_decode_error: Option<String>,
}

impl Tracker {
    /// A tracker over the given decoder, device map, process source and
    /// clock.
    #[must_use]
    pub fn new(
        decoder: Decoder,
        devices: DeviceMap,
        processes: Box<dyn ProcessSource>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            decoder,
            paths: PathMapper::new(devices),
            files: FileTable::default(),
            procs: ProcessTable::new(processes),
            agg: Aggregator::default(),
            sampler: Sampler::default(),
            clock,
            stats: TrackerStats::default(),
            last_decode_error: None,
        }
    }

    /// Replaces the device map (call after volume arrival/removal).
    pub fn set_devices(&mut self, devices: DeviceMap) {
        self.paths.set_devices(devices);
    }

    /// Sets the write sampling rate (1 = every write).
    pub fn set_sample_rate(&mut self, rate: u32) {
        self.sampler.set_rate(rate);
    }

    /// Current write sampling rate.
    #[must_use]
    pub const fn sample_rate(&self) -> u32 {
        self.sampler.rate()
    }

    /// Feeds one raw event: filter, sample, decode, apply.
    pub fn process_raw(&mut self, raw: &RawEvent<'_>) {
        self.stats.events += 1;
        if !Decoder::wants(raw.provider, raw.id) {
            return;
        }
        let mut scale = 1;
        if raw.provider == Provider::KernelFile && raw.id == file_id::WRITE {
            if !self.sampler.admit() {
                self.stats.sampled_out += 1;
                return;
            }
            scale = self.sampler.rate();
        }
        match self.decoder.decode(raw) {
            Ok(Some(ev)) => self.apply(&ev, scale),
            Ok(None) => {}
            Err(e) => {
                self.stats.decode_errors += 1;
                if self.last_decode_error.is_none() {
                    self.last_decode_error = Some(e.to_string());
                }
            }
        }
    }

    /// Applies one decoded event; `scale` multiplies write sizes (sampling).
    pub fn apply(&mut self, ev: &Event, scale: u32) {
        self.stats.decoded += 1;
        let at = ev.timestamp;
        match &ev.kind {
            EventKind::ProcessStart {
                pid,
                create_time,
                image,
            } => {
                let image = self.paths.to_dos(image);
                self.procs.start(*pid, *create_time, image);
            }
            EventKind::ProcessStop { pid, create_time } => {
                self.procs.stop(*pid, *create_time, at);
            }
            EventKind::NameCreate { key, name } => {
                let f = self.paths.file(name);
                self.files.name_key(*key, f);
            }
            EventKind::NameDelete { key, name } => {
                let hash = strata_store::path_hash(&self.paths.to_dos(name));
                self.files.release_key(*key, hash);
            }
            EventKind::Create {
                file_object,
                options,
                name,
            } => {
                let f = self.paths.file(name);
                self.files.open(*file_object, f.clone());
                if options & FILE_DELETE_ON_CLOSE != 0 {
                    self.count(ev.pid, at, &f, Counts::deleted(), false);
                }
            }
            EventKind::CreateNewFile { file_object, name } => {
                let f = self.paths.file(name);
                self.files.open(*file_object, f.clone());
                self.count(ev.pid, at, &f, Counts::created(), true);
            }
            EventKind::Write {
                file_object,
                key,
                size,
                io_flags,
            } => {
                if io_flags & (IRP_PAGING_IO | IRP_SYNCHRONOUS_PAGING_IO) != 0 {
                    self.stats.paging_writes += 1;
                    return;
                }
                let Some(f) = self.files.lookup(*file_object, *key) else {
                    self.stats.unmapped_writes += 1;
                    return;
                };
                let bytes = u64::from(*size) * u64::from(scale.max(1));
                self.count(ev.pid, at, &f, Counts::write(bytes), true);
            }
            EventKind::DeletePath {
                file_object,
                key,
                path,
                ..
            } => {
                let f = self.path_or_mapping(path, *file_object, *key);
                self.count(ev.pid, at, &f, Counts::deleted(), false);
            }
            EventKind::RenamePath {
                file_object,
                key,
                path,
            } => {
                let dos = self.paths.to_dos(path);
                // NOTE: a volume-relative path names no drive; the NameCreate
                // that follows the rename supplies the full new name.
                if !is_absolute(&dos) {
                    return;
                }
                let new = self.paths.file_from_dos(dos);
                if let Some((old, new)) = self.files.rename(*file_object, *key, new) {
                    self.agg.rename_writer(old.hash, new.hash);
                }
            }
        }
    }

    /// The event's own path when it resolves to an absolute DOS or UNC path,
    /// else the mapped name (a volume-relative path names no drive).
    fn path_or_mapping(&mut self, path: &[u16], file_object: u64, key: u64) -> Arc<FileInfo> {
        let dos = self.paths.to_dos(path);
        if !is_absolute(&dos)
            && let Some(f) = self.files.lookup(file_object, key)
        {
            return f;
        }
        self.paths.file_from_dos(dos)
    }

    fn count(&mut self, pid: u32, at: FileTime, f: &FileInfo, c: Counts, writer: bool) {
        let Some(p) = self.procs.resolve(pid, at) else {
            self.stats.unattributed += 1;
            return;
        };
        let ts = Timestamp::from_filetime(at);
        self.agg.add(&p, &f.dir, ts, c);
        if writer {
            self.agg.note_writer(f.hash, &p, ts);
        }
    }

    /// Takes the rollups and last writers accumulated since the previous
    /// call, and prunes in-memory state that aged out.
    pub fn flush(&mut self) -> ActivityBatch {
        let now = self.clock.now();
        self.agg.expire(now);
        self.procs.prune(FileTime::from_unix_secs(now.0));
        self.paths.sweep();
        self.agg.drain()
    }

    /// Top writers over `w`.
    #[must_use]
    pub fn top_writers(&self, w: Window, limit: usize) -> Vec<WriterSummary> {
        self.agg.top_writers(w, self.clock.now(), limit)
    }

    /// Per-process, per-directory totals over `w`.
    #[must_use]
    pub fn dir_totals(&self, w: Window, limit: usize) -> Vec<DirTotal> {
        self.agg.dir_totals(w, self.clock.now(), limit)
    }

    /// Attribution evidence from the hourly windows since `since` (at most
    /// 48 hours back).
    #[must_use]
    pub fn evidence(&self, since: Timestamp, cfg: &EvidenceConfig) -> Vec<Evidence> {
        evidence(&self.agg.hourly_profile(since), cfg)
    }

    /// Pipeline counters.
    #[must_use]
    pub const fn stats(&self) -> TrackerStats {
        self.stats
    }

    /// The first decode error seen, for diagnostics.
    #[must_use]
    pub fn last_decode_error(&self) -> Option<&str> {
        self.last_decode_error.as_deref()
    }

    /// Forgets all activity held in memory (windows, pending rollups, last
    /// writers, counters). Name and process maps are kept: they hold no
    /// activity and rebuilding them would lose attribution of open files.
    pub fn clear(&mut self) {
        self.agg.clear();
        self.stats = TrackerStats::default();
        self.last_decode_error = None;
    }

    /// Sizes of the internal maps: (processes, file mappings, pending rows).
    #[must_use]
    pub fn sizes(&self) -> (usize, usize, usize) {
        (self.procs.len(), self.files.len(), self.agg.pending_rows())
    }
}

/// `X:\...` or `\\server\...`.
fn is_absolute(dos: &str) -> bool {
    let b = dos.as_bytes();
    (b.len() >= 3 && b[1] == b':' && b[2] == b'\\') || dos.starts_with(r"\\")
}
