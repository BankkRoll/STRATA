//! Scan ingestion: records → index → classification → published volume.
//!
//! Both scanners (the unelevated walker and the elevated helper's MFT scan)
//! feed [`Ingest`], which:
//!
//! 1. stages every record in an [`IndexBuilder`] (the final index);
//! 2. optionally maintains a **preview**: once the root record has arrived,
//!    the records seen so far are built into an index that is published at
//!    once, and every later batch is applied to it through the index's live
//!    update path (`Index::apply`) on a separate preview thread, so the UI
//!    browses a growing tree with stable ids while the scan streams and the
//!    scanner never waits for it. The preview is re-classified adaptively
//!    (every few seconds, at most ~10% of the time). If the preview thread
//!    falls more than a bounded queue behind, the preview freezes as it is
//!    until the final index arrives. It is used when a volume has no index
//!    yet; a rescan keeps showing the previous complete index until the new
//!    one is ready;
//! 3. on [`Ingest::finish`], adds the "Unaccounted / system reserved" and
//!    "System Restore / Shadow copies" virtual blocks, builds
//!    the final index, classifies it, computes the id remap from the index
//!    being replaced and swaps it in.
//!
//! [`walk`] drives the walker on the calling thread; the helper path lives
//! in [`crate::helper`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use rayon::prelude::*;
use strata_core::{FileRef, ScanRecord};
use strata_index::{EntryId, Index, IndexBuilder, IndexOptions, Update};

use crate::classify::{Classified, Engine, classify_index, ext_slots};
use crate::ids::{self, GONE, Remap};
use crate::model::{ScannerUsed, VolumeData, now_filetime, unix_ms};

/// Name of the virtual block for the unexplained gap between used space and scanned bytes.
pub const UNACCOUNTED_NAME: &str = "Unaccounted / system reserved";
/// Name of the virtual block for Volume Shadow Copy storage.
pub const SHADOW_NAME: &str = "System Restore / Shadow copies";

/// How often a preview is re-classified.
const PREVIEW_CLASSIFY_EVERY: Duration = Duration::from_secs(3);

/// Facts about the volume when a scan covers a whole volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeFacts {
    /// Volume GUID path.
    pub guid_path: String,
    /// Serial number.
    pub serial: u64,
    /// Capacity.
    pub total_bytes: u64,
    /// Free bytes.
    pub free_bytes: u64,
}

/// What to scan.
#[derive(Debug, Clone)]
pub struct ScanTarget {
    /// Directory to walk (a volume root such as `C:\`, or any folder).
    pub root: PathBuf,
    /// Display path of the root (`C:\`).
    pub root_display: String,
    /// Set when `root` is a whole volume (enables reconciliation blocks).
    pub volume: Option<VolumeFacts>,
}

/// Shadow-storage bytes for the reconciliation block, when known.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShadowBytes {
    /// Bytes used by shadow copies (`None` = could not be queried).
    pub used: Option<u64>,
}

/// Progress of a running scan (`ScanProgress` in `ui/src/lib/volumes.ts`).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgress {
    /// Entries processed.
    pub entries: u64,
    /// Allocated bytes accounted.
    pub bytes: u64,
    /// 0–1 when the total is known.
    pub fraction: Option<f64>,
    /// Estimated seconds left.
    pub eta_secs: Option<f64>,
}

/// Why a scan could not produce an index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanError {
    /// The volume has more entries than wire ids can address.
    TooLarge(usize),
    /// The index rejected the data.
    Index(String),
    /// The scanner failed (root missing, helper gone, ...).
    Scanner(String),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(n) => write!(
                f,
                "this volume has {n} entries, more than Strata can index ({})",
                ids::MAX_SLOTS
            ),
            Self::Index(e) | Self::Scanner(e) => f.write_str(e),
        }
    }
}

/// Receives preview publications and progress.
pub trait ScanObserver {
    /// Progress (throttled by the scanner).
    fn progress(&self, p: &ScanProgress);
    /// A preview became browsable, or was refreshed.
    fn preview(&self) {}
}

/// Where the published index lives (one per volume session).
pub type DataSlot = RwLock<Option<VolumeData>>;

/// Streams records into a new index; see the module docs.
pub struct Ingest {
    engine: Arc<Engine>,
    target: ScanTarget,
    slot: Arc<DataSlot>,
    scanner: ScannerUsed,
    builder: IndexBuilder,
    preview: PreviewState,
    generation: u8,
    allocated_sum: u64,
    records: u64,
    started: Instant,
    started_ms: i64,
}

enum PreviewState {
    Off,
    Waiting(Vec<ScanRecord>),
    Live {
        tx: Sender<Vec<ScanRecord>>,
        stop: Arc<AtomicBool>,
        worker: std::thread::JoinHandle<()>,
    },
    /// The preview worker fell behind the scanner and stopped; the preview
    /// stays browsable as it was until the final index replaces it.
    Frozen(std::thread::JoinHandle<()>),
}

/// Batches the preview worker may lag behind before the preview freezes.
const PREVIEW_QUEUE: usize = 64;

/// Applies scan batches to the published preview off the scanner's thread,
/// so the scanner never waits for live-update or classification work.
/// Re-classification is adaptive: at most every
/// [`PREVIEW_CLASSIFY_EVERY`], and never more than ~10% of the time.
fn preview_worker(
    engine: &Engine,
    slot: &DataSlot,
    rx: &Receiver<Vec<ScanRecord>>,
    stop: &AtomicBool,
) {
    let mut classified_at = Instant::now();
    let mut cost = Duration::ZERO;
    while !stop.load(Ordering::Acquire) {
        let batch = match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(b) => Some(b),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let due = classified_at.elapsed() >= PREVIEW_CLASSIFY_EVERY.max(cost * 10);
        if batch.is_none() && !due {
            continue;
        }
        let mut g = write(slot);
        let Some(data) = g.as_mut().filter(|d| d.preview) else {
            return;
        };
        if let Some(b) = batch {
            apply_live(&mut data.index, b.into_iter());
        }
        if due {
            let t = Instant::now();
            reclassify(engine, data);
            cost = t.elapsed();
            classified_at = Instant::now();
        }
    }
}

impl std::fmt::Debug for Ingest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ingest")
            .field("records", &self.records)
            .finish_non_exhaustive()
    }
}

fn read<T>(m: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write<T>(m: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Ingest {
    /// Starts ingesting a scan of `target` into `slot`. A preview is shown
    /// only when `preview` is set and the slot holds no index yet.
    #[must_use]
    pub fn new(
        engine: Arc<Engine>,
        target: ScanTarget,
        slot: Arc<DataSlot>,
        scanner: ScannerUsed,
        preview: bool,
    ) -> Self {
        let current = read(&slot).as_ref().map(|d| d.generation);
        let generation = current.map_or(0, |g| g.wrapping_add(1) & 3);
        let builder = IndexBuilder::new(index_options(&target));
        Self {
            engine,
            slot,
            scanner,
            builder,
            preview: if preview && current.is_none() {
                PreviewState::Waiting(Vec::new())
            } else {
                PreviewState::Off
            },
            generation,
            allocated_sum: 0,
            records: 0,
            started: Instant::now(),
            started_ms: unix_ms(),
            target,
        }
    }

    /// Records ingested so far.
    #[must_use]
    pub const fn records(&self) -> u64 {
        self.records
    }

    /// Allocated bytes of the records ingested so far.
    #[must_use]
    pub const fn allocated(&self) -> u64 {
        self.allocated_sum
    }

    /// Progress estimate: the fraction is scanned bytes over the volume's
    /// used bytes (a lower bound, since the gap is only known at the end).
    #[must_use]
    pub fn progress(&self) -> ScanProgress {
        let fraction = self.target.volume.as_ref().and_then(|v| {
            let used = v.total_bytes.saturating_sub(v.free_bytes);
            (used > 0).then(|| (self.allocated_sum as f64 / used as f64).min(0.99))
        });
        let elapsed = self.started.elapsed().as_secs_f64();
        let eta_secs = fraction
            .filter(|&f| f > 0.02 && elapsed > 1.0)
            .map(|f| elapsed * (1.0 - f) / f);
        ScanProgress {
            entries: self.records,
            bytes: self.allocated_sum,
            fraction,
            eta_secs,
        }
    }

    /// Ingests one batch.
    ///
    /// # Errors
    ///
    /// [`ScanError::TooLarge`] past the id limit; index errors.
    pub fn push(
        &mut self,
        batch: Vec<ScanRecord>,
        observer: &dyn ScanObserver,
    ) -> Result<(), ScanError> {
        for r in &batch {
            if !r.links.is_empty() {
                self.allocated_sum = self.allocated_sum.saturating_add(r.sizes.total_allocated());
            }
        }
        self.records += batch.len() as u64;
        match &mut self.preview {
            PreviewState::Off => {}
            PreviewState::Waiting(pending) => {
                let root_seen = batch.iter().any(is_root_record);
                pending.extend(batch.iter().cloned());
                if root_seen {
                    let pending = std::mem::take(pending);
                    self.start_preview(pending)?;
                    observer.preview();
                }
            }
            PreviewState::Live { tx, .. } => {
                // NOTE: never block the scanner on the preview: when the
                // worker falls behind, the preview freezes instead.
                if tx.try_send(batch.clone()).is_err()
                    && let PreviewState::Live { worker, .. } =
                        std::mem::replace(&mut self.preview, PreviewState::Off)
                {
                    self.preview = PreviewState::Frozen(worker);
                }
            }
            PreviewState::Frozen(_) => {}
        }
        if self.builder.staged() + batch.len() > ids::MAX_SLOTS {
            return Err(ScanError::TooLarge(self.builder.staged() + batch.len()));
        }
        self.builder
            .push_batch(batch)
            .map_err(|e| ScanError::Index(e.to_string()))
    }

    fn start_preview(&mut self, pending: Vec<ScanRecord>) -> Result<(), ScanError> {
        let mut b = IndexBuilder::new(index_options(&self.target));
        b.push_batch(pending)
            .map_err(|e| ScanError::Index(e.to_string()))?;
        let index = b.finish().map_err(|e| ScanError::Index(e.to_string()))?;
        let mut data = VolumeData::new(
            self.generation,
            index,
            Classified::default(),
            self.target.root_display.clone(),
            self.scanner,
        );
        reclassify(&self.engine, &mut data);
        data.preview = true;
        data.partial = true;
        data.scanned_at_ms = self.started_ms;
        *write(&self.slot) = Some(data);
        self.generation = self.generation.wrapping_add(1) & 3;
        let (tx, rx) = bounded(PREVIEW_QUEUE);
        let engine = self.engine.clone();
        let slot = self.slot.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let worker = std::thread::Builder::new()
            .name("strata-preview".into())
            .spawn(move || preview_worker(&engine, &slot, &rx, &flag))
            .map_err(|e| ScanError::Scanner(e.to_string()))?;
        self.preview = PreviewState::Live { tx, stop, worker };
        Ok(())
    }

    /// Stops the preview worker without applying what it still has queued
    /// (the final index replaces the preview anyway).
    fn stop_preview(&mut self) {
        let worker = match std::mem::replace(&mut self.preview, PreviewState::Off) {
            PreviewState::Live { tx, stop, worker } => {
                stop.store(true, Ordering::Release);
                drop(tx);
                Some(worker)
            }
            PreviewState::Frozen(worker) => Some(worker),
            PreviewState::Off | PreviewState::Waiting(_) => None,
        };
        if let Some(w) = worker {
            let _ = w.join();
        }
    }

    /// Builds, classifies and publishes the final index. `partial` marks a
    /// cancelled or incomplete scan; `shadow` is the shadow-storage query
    /// result for whole-volume scans.
    ///
    /// # Errors
    ///
    /// Index build errors (the previous index stays published).
    pub fn finish(mut self, partial: bool, shadow: ShadowBytes) -> Result<FinishStats, ScanError> {
        let mut stats = FinishStats::default();
        let t = Instant::now();
        self.stop_preview();
        stats.preview_drain = t.elapsed();
        if let Some(v) = &self.target.volume {
            let used = v.total_bytes.saturating_sub(v.free_bytes);
            let mut gap = used.saturating_sub(self.allocated_sum);
            if let Some(s) = shadow.used.filter(|&s| s > 0) {
                let s = s.min(gap);
                self.builder.add_virtual_block(SHADOW_NAME, s, s);
                gap -= s;
            }
            if gap > 0 {
                self.builder.add_virtual_block(UNACCOUNTED_NAME, gap, gap);
            }
        }
        self.builder.set_partial(partial);
        let t = Instant::now();
        let builder = std::mem::replace(
            &mut self.builder,
            IndexBuilder::new(IndexOptions::default()),
        );
        let mut index = builder
            .finish()
            .map_err(|e| ScanError::Index(e.to_string()))?;
        stats.build = t.elapsed();
        if index.slot_count() > ids::MAX_SLOTS {
            return Err(ScanError::TooLarge(index.slot_count()));
        }
        let t = Instant::now();
        let slots = ext_slots(&index);
        let classified =
            classify_index(&self.engine, &mut index, &self.target.root_display, &slots);
        stats.classify = t.elapsed();
        let mut data = VolumeData::new(
            self.generation,
            index,
            classified,
            self.target.root_display.clone(),
            self.scanner,
        );
        data.partial = partial;
        let t = Instant::now();
        let remap = read(&self.slot).as_ref().map(|old| Remap {
            generation: old.generation,
            map: remap_ids(&old.index, &data.index),
        });
        data.remap = remap;
        stats.remap = t.elapsed();
        *write(&self.slot) = Some(data);
        Ok(stats)
    }
}

impl Drop for Ingest {
    fn drop(&mut self) {
        self.stop_preview();
    }
}

/// Phase timings of [`Ingest::finish`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FinishStats {
    /// Waiting for the preview worker to apply its queue.
    pub preview_drain: Duration,
    /// `IndexBuilder::finish`.
    pub build: Duration,
    /// Classification, attribution and color keys.
    pub classify: Duration,
    /// Old → new id remap.
    pub remap: Duration,
}

fn index_options(target: &ScanTarget) -> IndexOptions {
    let mut opts = IndexOptions {
        now: now_filetime(),
        ..IndexOptions::default()
    };
    opts.volume.prefix = target.root_display.trim_end_matches('\\').to_owned();
    opts.volume.serial = target.volume.as_ref().map_or(0, |v| v.serial);
    opts
}

/// The root record links to itself (NTFS record 5, the walker's root).
fn is_root_record(r: &ScanRecord) -> bool {
    r.links.iter().any(|l| l.parent == r.id)
}

/// Applies records through the live path. A record the index refuses (for
/// example one that would replace the root) is skipped; the final index is
/// built from the complete record set regardless.
fn apply_live(index: &mut Index, batch: impl Iterator<Item = ScanRecord> + Clone) {
    if index.apply(batch.clone().map(Update::Upsert)).is_err() {
        for r in batch {
            let _ = index.upsert(r);
        }
    }
}

fn reclassify(engine: &Engine, data: &mut VolumeData) {
    let slots = ext_slots(&data.index);
    let c = classify_index(engine, &mut data.index, &data.root_path, &slots);
    data.classes = c.classes;
    data.keys = c.keys;
}

/// Maps every live id of `old` to the id of the same link in `new`
/// ([`GONE`] when it no longer exists). Links of one record are matched by
/// rank (primary first); virtual nodes by role or name.
#[must_use]
pub fn remap_ids(old: &Index, new: &Index) -> Vec<u32> {
    let mut map = vec![GONE; old.slot_count()];
    let virtual_by_name: std::collections::HashMap<String, u32> = new
        .children(new.root())
        .filter(|&k| new.file_ref(k).is_none())
        .map(|k| (new.name_lossy(k), k.0))
        .collect();
    map.par_iter_mut().enumerate().for_each(|(i, slot)| {
        let id = EntryId(i as u32);
        if !old.is_live(id) {
            return;
        }
        *slot = if id == old.root() {
            new.root().0
        } else if id == old.orphans_node() {
            new.orphans_node().0
        } else if id == old.metadata_node() {
            new.metadata_node().0
        } else if let Some(fr) = old.file_ref(id) {
            same_link(old, new, fr, id)
        } else {
            virtual_by_name
                .get(&old.name_lossy(id))
                .copied()
                .unwrap_or(GONE)
        };
    });
    map
}

fn same_link(old: &Index, new: &Index, fr: FileRef, id: EntryId) -> u32 {
    // PERF: almost every record has one link; `lookup` avoids allocating
    // two link lists per entry, which dominated the remap of large volumes.
    if old.link_count(id) <= 1 && new.lookup(fr).is_some_and(|e| new.link_count(e) <= 1) {
        return new.lookup(fr).map_or(GONE, |e| e.0);
    }
    let new_links = new.links(fr);
    if new_links.len() <= 1 {
        return new_links.first().map_or(GONE, |e| e.0);
    }
    let rank = old.links(fr).iter().position(|&e| e == id).unwrap_or(0);
    new_links.get(rank).map_or(GONE, |e| e.0)
}

/// Walks `target.root` with the unelevated walker, streaming into `ingest`.
/// Returns the walk statistics; cancellation is reported through them.
///
/// # Errors
///
/// Root problems from the walker, or ingest errors.
#[cfg(windows)]
pub fn walk(
    ingest: &mut Ingest,
    opts: strata_walk::WalkOptions,
    cancel: &strata_walk::CancelToken,
    observer: &dyn ScanObserver,
) -> Result<strata_walk::WalkStats, ScanError> {
    struct Sink<'s> {
        ingest: &'s mut Ingest,
        observer: &'s dyn ScanObserver,
        cancel: &'s strata_walk::CancelToken,
        error: Option<ScanError>,
        last: Instant,
    }
    impl strata_walk::WalkSink for Sink<'_> {
        fn records(&mut self, batch: Vec<ScanRecord>) {
            if self.error.is_some() {
                return;
            }
            if let Err(e) = self.ingest.push(batch, self.observer) {
                self.error = Some(e);
                self.cancel.cancel();
            }
        }
        fn progress(&mut self, _: &strata_walk::Progress) {
            if self.last.elapsed() >= Duration::from_millis(250) {
                self.last = Instant::now();
                self.observer.progress(&self.ingest.progress());
            }
        }
    }
    let walker = strata_walk::Walker::new(&ingest.target.root, opts)
        .map_err(|e| ScanError::Scanner(e.to_string()))?;
    let mut sink = Sink {
        ingest,
        observer,
        cancel,
        error: None,
        last: Instant::now(),
    };
    let stats = walker
        .run(&mut sink, cancel)
        .map_err(|e| ScanError::Scanner(e.to_string()))?;
    match sink.error {
        Some(e) => Err(e),
        None => Ok(stats),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::{EntryFlags, NameLink, Sizes, Times, WideName};

    fn rec(id: u64, parent: u64, name: &str, size: u64, dir: bool) -> ScanRecord {
        let r = |n| FileRef::from_parts(n, 1);
        ScanRecord {
            id: r(id),
            links: vec![NameLink {
                parent: r(parent),
                name: WideName::from_str_lossless(name),
            }],
            attributes: 0,
            flags: if dir {
                EntryFlags::DIR
            } else {
                EntryFlags::EMPTY
            },
            times: Times::default(),
            fn_created: None,
            sizes: Sizes {
                logical: size,
                allocated: size,
                ..Sizes::default()
            },
            reparse: None,
            ads: vec![],
        }
    }

    fn build(recs: Vec<ScanRecord>) -> Index {
        let mut b = IndexBuilder::new(IndexOptions::default());
        b.push_batch(recs).unwrap();
        b.finish().unwrap()
    }

    #[test]
    fn remap_follows_records_across_rebuilds() {
        let base = vec![
            rec(5, 5, "", 0, true),
            rec(30, 5, "a", 0, true),
            rec(31, 30, "x", 10, false),
            rec(40, 5, "gone", 7, false),
        ];
        let old = build(base.clone());
        let mut next = base[..3].to_vec();
        next.push(rec(20, 5, "new", 1, true));
        let new = build(next);
        let map = remap_ids(&old, &new);
        let x_old = old.lookup(FileRef::from_parts(31, 1)).unwrap();
        let x_new = new.lookup(FileRef::from_parts(31, 1)).unwrap();
        assert_eq!(map[x_old.index()], x_new.0);
        assert_eq!(map[old.root().index()], new.root().0);
        assert_eq!(map[old.orphans_node().index()], new.orphans_node().0);
        let gone = old.lookup(FileRef::from_parts(40, 1)).unwrap();
        assert_eq!(map[gone.index()], GONE);
    }

    #[test]
    fn root_record_detection() {
        assert!(is_root_record(&rec(5, 5, "", 0, true)));
        assert!(!is_root_record(&rec(6, 5, "a", 0, true)));
    }
}
