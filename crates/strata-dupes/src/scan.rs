//! The duplicate pipeline.
//!
//! 1. **Grouping.** Candidates pass the index-level gate (no directories,
//!    empty or small files, secondary hardlinks, cloud/offline files,
//!    non-content reparse points, metadata, unverifiable ids, Recycle Bin)
//!    and are de-duplicated by (volume, file reference).
//! 2. **Measuring.** Candidates with alternate streams are measured first
//!    (their index size includes the streams); then every member of a
//!    size group with two or more files is opened for attributes only. The
//!    handle must show the scan's file id and pass the placeholder gate.
//!    From here on the handle's size and last-write time are the truth.
//! 3. **Cache.** Hashes are looked up by (volume, file id, size, mtime).
//! 4. **Partial hash.** xxh3 over the first, middle and last 64 KiB; files
//!    are regrouped by (size, partial).
//! 5. **Full hash.** BLAKE3 over the whole unnamed stream, largest files
//!    first, on a dedicated pool of `concurrency` threads sharing one rate
//!    limit. Groups are (size, BLAKE3) with two or more files.
//!
//! Every hash opens the file again (no recall), re-verifies id, attributes,
//! size and mtime against the measurement, and re-reads size and mtime after
//! the last byte; any difference drops the file as [`SkipReason::Changed`]
//! and invalidates its cache row. New hashes are written to the cache in
//! small batches as they complete, which is what makes a cancelled scan
//! resumable: the next run finds them.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use strata_clean::CancelToken;
use strata_core::{EntryFlags, FileRef};

use crate::cache::{CachedHash, HashCache, HashKey};
use crate::candidate::{Candidate, VolumeKey};
use crate::gate::{self, Exclusion, GateConfig, SkipReason};
use crate::hash::{self, Expect, HashFail};
use crate::keep::{KeepContext, KeepReason, KeepSuggestion};
use crate::report::{
    DupFile, DuplicateGroup, DuplicateReport, Phase, Progress, ScanOutcome, ScanStats, SkippedFile,
};
use crate::throttle::Throttle;
use crate::win::{Access, Facts};

/// Default minimum file size (1 MiB).
pub const DEFAULT_MIN_SIZE: u64 = 1024 * 1024;

/// Tunables for [`find_duplicates`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanConfig {
    /// Files smaller than this are ignored. Zero-byte files always are.
    pub min_size: u64,
    /// Hashing threads. 1–2 suits spinning disks; SSDs benefit from more.
    pub concurrency: usize,
    /// Combined read limit in bytes per second (`None` = unlimited).
    pub max_bytes_per_sec: Option<u64>,
    /// Bytes per read during full hashing. Large sequential reads keep
    /// spinning disks streaming.
    pub read_buffer: usize,
    /// Treat WOF-compressed and deduplicated files (reparse points whose
    /// data is local) as ordinary files.
    pub allow_wof_and_dedup: bool,
    /// Minimum time between progress events while a phase runs.
    pub progress_interval: Duration,
    /// New hashes are written to the cache every this many files.
    pub cache_batch: usize,
    /// Locations and rules for keep suggestions.
    pub keep: KeepContext,
}

impl Default for ScanConfig {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(2, std::num::NonZero::get);
        Self {
            min_size: DEFAULT_MIN_SIZE,
            concurrency: cpus.clamp(1, 4),
            max_bytes_per_sec: None,
            read_buffer: 1024 * 1024,
            allow_wof_and_dedup: true,
            progress_interval: Duration::from_millis(100),
            cache_batch: 64,
            keep: KeepContext::default(),
        }
    }
}

impl ScanConfig {
    fn gate(&self) -> GateConfig {
        GateConfig {
            min_size: self.min_size.max(1),
            allow_wof_and_dedup: self.allow_wof_and_dedup,
        }
    }
}

/// A candidate after measuring.
#[derive(Debug, Clone)]
struct Item {
    cand: usize,
    facts: Facts,
    partial: Option<u64>,
    full: Option<[u8; 32]>,
}

impl Item {
    fn key(&self, pool: &[Candidate]) -> HashKey {
        HashKey {
            file_ref: pool[self.cand].file_ref,
            size: self.facts.size,
            mtime: self.facts.mtime,
        }
    }
}

/// Finds duplicate files among `candidates`.
///
/// Blocks until done or cancelled; call it from a background thread.
/// `progress` may be called from worker threads.
///
/// # Example
///
/// ```no_run
/// use strata_dupes::{find_duplicates, MemoryHashCache, ScanConfig, ScanOutcome};
/// # fn demo(candidates: Vec<strata_dupes::Candidate>) {
/// let cancel = strata_clean::CancelToken::new();
/// let cache = MemoryHashCache::new();
/// match find_duplicates(candidates, &ScanConfig::default(), &cache, &cancel, &|p| {
///     println!("{:?} {}/{}", p.phase, p.files_done, p.files_total);
/// }) {
///     ScanOutcome::Completed(report) => println!("{} groups", report.groups.len()),
///     ScanOutcome::Cancelled(_) => println!("cancelled; rerun to resume"),
/// }
/// # }
/// ```
pub fn find_duplicates(
    candidates: impl IntoIterator<Item = Candidate>,
    cfg: &ScanConfig,
    cache: &dyn HashCache,
    cancel: &CancelToken,
    progress: &(dyn Fn(&Progress) + Sync),
) -> ScanOutcome {
    let started = Instant::now();
    let mut run = Run {
        cfg,
        cache,
        cancel,
        progress,
        stats: ScanStats::default(),
        skipped: Vec::new(),
        throttle: Throttle::new(cfg.max_bytes_per_sec),
        pool: rayon::ThreadPoolBuilder::new()
            .num_threads(cfg.concurrency.max(1))
            .thread_name(|i| format!("strata-dupes-{i}"))
            .build()
            .ok(),
        cache_hits: AtomicU64::new(0),
    };
    let outcome = run.execute(candidates);
    match outcome {
        Some(mut report) => {
            run.stats.elapsed_ms = started.elapsed().as_millis() as u64;
            run.emit_final(Phase::Finished);
            report.stats = run.stats;
            report.skipped = run.skipped;
            ScanOutcome::Completed(report)
        }
        None => {
            run.stats.elapsed_ms = started.elapsed().as_millis() as u64;
            run.emit_final(Phase::Finished);
            ScanOutcome::Cancelled(run.stats)
        }
    }
}

/// A size shared by several candidates, with their indices.
pub type SizeGroup = (u64, Vec<usize>);

/// Groups candidates by size only, without opening anything: the dry run
/// the UI uses to show "N files in M size groups could be duplicates".
///
/// Returns `(size, candidate indices)` for every size shared by two or
/// more eligible candidates, plus the exclusion counts. Candidates with
/// alternate streams are grouped by their index size here.
#[must_use]
pub fn group_by_size(
    candidates: &[Candidate],
    min_size: u64,
) -> (Vec<SizeGroup>, BTreeMap<Exclusion, u64>) {
    let cfg = GateConfig {
        min_size: min_size.max(1),
        allow_wof_and_dedup: true,
    };
    let mut excluded = BTreeMap::new();
    let mut ok: Vec<usize> = Vec::with_capacity(candidates.len());
    for (i, c) in candidates.iter().enumerate() {
        match gate::check_candidate(c, cfg) {
            Ok(()) => ok.push(i),
            Err(e) => *excluded.entry(e).or_insert(0) += 1,
        }
    }
    let before = ok.len();
    ok.par_sort_unstable_by(|&a, &b| identity(&candidates[a]).cmp(&identity(&candidates[b])));
    ok.dedup_by(|a, b| identity(&candidates[*a]) == identity(&candidates[*b]));
    if before > ok.len() {
        *excluded.entry(Exclusion::SameFile).or_insert(0) += (before - ok.len()) as u64;
    }
    ok.par_sort_unstable_by_key(|&i| candidates[i].size);
    let groups = ok
        .chunk_by(|&a, &b| candidates[a].size == candidates[b].size)
        .filter(|g| g.len() >= 2)
        .map(|g| (candidates[g[0]].size, g.to_vec()))
        .collect();
    (groups, excluded)
}

fn identity(c: &Candidate) -> (u64, &str, FileRef) {
    (c.volume.serial, &c.volume.guid_path, c.file_ref)
}

struct Run<'a> {
    cfg: &'a ScanConfig,
    cache: &'a dyn HashCache,
    cancel: &'a CancelToken,
    progress: &'a (dyn Fn(&Progress) + Sync),
    stats: ScanStats,
    skipped: Vec<SkippedFile>,
    throttle: Throttle,
    pool: Option<rayon::ThreadPool>,
    cache_hits: AtomicU64,
}

/// Shared counters for one phase.
struct Tracker<'a> {
    phase: Phase,
    files_total: u64,
    bytes_total: u64,
    files: AtomicU64,
    bytes: AtomicU64,
    started: Instant,
    last: Mutex<Instant>,
    interval: Duration,
    hits: &'a AtomicU64,
    sink: &'a (dyn Fn(&Progress) + Sync),
}

impl Tracker<'_> {
    fn snapshot(&self) -> Progress {
        let files_done = self.files.load(Ordering::Relaxed);
        let bytes_done = self.bytes.load(Ordering::Relaxed);
        let secs = self.started.elapsed().as_secs_f64();
        let rate = if secs > 0.0 {
            bytes_done as f64 / secs
        } else {
            0.0
        };
        let eta_secs = if self.bytes_total > 0 && rate > 0.0 {
            Some(self.bytes_total.saturating_sub(bytes_done) as f64 / rate)
        } else if self.bytes_total == 0 && files_done > 0 && secs > 0.0 {
            let per = secs / files_done as f64;
            Some(self.files_total.saturating_sub(files_done) as f64 * per)
        } else {
            None
        };
        Progress {
            phase: self.phase,
            files_done,
            files_total: self.files_total,
            bytes_done,
            bytes_total: self.bytes_total,
            bytes_per_sec: rate,
            eta_secs,
            cache_hits: self.hits.load(Ordering::Relaxed),
        }
    }

    fn tick(&self) {
        let now = Instant::now();
        let due = {
            let Ok(mut last) = self.last.try_lock() else {
                return;
            };
            if now.duration_since(*last) >= self.interval {
                *last = now;
                true
            } else {
                false
            }
        };
        if due {
            (self.sink)(&self.snapshot());
        }
    }

    fn file_done(&self) {
        self.files.fetch_add(1, Ordering::Relaxed);
        self.tick();
    }
}

impl<'a> Run<'a> {
    fn tracker(&self, phase: Phase, files_total: u64, bytes_total: u64) -> Tracker<'_> {
        let t = Tracker {
            phase,
            files_total,
            bytes_total,
            files: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            started: Instant::now(),
            last: Mutex::new(Instant::now()),
            interval: self.cfg.progress_interval,
            hits: &self.cache_hits,
            sink: self.progress,
        };
        // Phase-start events are emitted synchronously on the calling
        // thread before any work of the phase begins.
        (self.progress)(&t.snapshot());
        t
    }

    fn emit_final(&self, phase: Phase) {
        let t = Tracker {
            phase,
            files_total: 0,
            bytes_total: 0,
            files: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            started: Instant::now(),
            last: Mutex::new(Instant::now()),
            interval: Duration::ZERO,
            hits: &self.cache_hits,
            sink: self.progress,
        };
        (self.progress)(&t.snapshot());
    }

    fn par_map<T: Sync, R: Send>(&self, items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
        match &self.pool {
            Some(p) => p.install(|| items.par_iter().with_max_len(1).map(&f).collect()),
            None => items.iter().map(f).collect(),
        }
    }

    fn execute(
        &mut self,
        candidates: impl IntoIterator<Item = Candidate>,
    ) -> Option<DuplicateReport> {
        let gate = self.cfg.gate();

        // 1. Grouping
        let _ = self.tracker(Phase::Grouping, 0, 0);
        let mut pool: Vec<Candidate> = Vec::new();
        for (n, c) in candidates.into_iter().enumerate() {
            if n % 65_536 == 0 && self.cancel.is_cancelled() {
                return None;
            }
            self.stats.candidates += 1;
            match gate::check_candidate(&c, gate) {
                Ok(()) => pool.push(c),
                Err(e) => *self.stats.excluded.entry(e).or_insert(0) += 1,
            }
        }
        let before = pool.len();
        pool.par_sort_unstable_by(|a, b| {
            identity(a)
                .cmp(&identity(b))
                .then_with(|| a.path.cmp(&b.path))
        });
        pool.dedup_by(|a, b| identity(a) == identity(b));
        if before > pool.len() {
            *self.stats.excluded.entry(Exclusion::SameFile).or_insert(0) +=
                (before - pool.len()) as u64;
        }

        // 2. Measuring: streams first, then size-group members.
        let ads: Vec<usize> = (0..pool.len())
            .filter(|&i| pool[i].flags.contains(EntryFlags::HAS_ADS))
            .collect();
        let mut measured: HashMap<usize, Facts> = HashMap::new();
        let ads_facts = self.measure(&pool, &ads)?;
        let mut sizes: Vec<(u64, usize)> = Vec::with_capacity(pool.len());
        for (i, c) in pool.iter().enumerate() {
            if c.flags.contains(EntryFlags::HAS_ADS) {
                continue;
            }
            sizes.push((c.size, i));
        }
        for (i, f) in ads_facts {
            sizes.push((f.size, i));
            measured.insert(i, f);
        }
        sizes.par_sort_unstable();
        let mut to_measure = Vec::new();
        for run in sizes.chunk_by(|a, b| a.0 == b.0) {
            if run.len() < 2 {
                continue;
            }
            to_measure.extend(
                run.iter()
                    .map(|&(_, i)| i)
                    .filter(|i| !measured.contains_key(i)),
            );
        }
        for (i, f) in self.measure(&pool, &to_measure)? {
            measured.insert(i, f);
        }
        let mut items: Vec<Item> = measured
            .into_iter()
            .filter_map(|(cand, facts)| match gate::check_size(facts.size, gate) {
                Ok(()) => Some(Item {
                    cand,
                    facts,
                    partial: None,
                    full: None,
                }),
                Err(e) => {
                    *self.stats.excluded.entry(e).or_insert(0) += 1;
                    None
                }
            })
            .collect();
        items = regroup(items, |it| (it.facts.size, 0, [0; 32]));

        // 3. Cache
        self.lookup_cache(&pool, &mut items);

        // 4. Partial hash
        let work: Vec<usize> = (0..items.len())
            .filter(|&i| items[i].partial.is_none())
            .collect();
        let cached = (items.len() - work.len()) as u64;
        self.stats.partial_cached += cached;
        self.cache_hits.fetch_add(cached, Ordering::Relaxed);
        let results = self.hash_phase(&pool, &items, &work, Phase::PartialHash)?;
        let mut drop = vec![false; items.len()];
        for (w, r) in work.iter().zip(results) {
            match r {
                Ok(h) => items[*w].partial = Some(h.partial),
                Err(()) => drop[*w] = true,
            }
        }
        items = keep_unmarked(items, &drop);
        items = regroup(items, |it| {
            (it.facts.size, it.partial.unwrap_or(0), [0; 32])
        });

        // 5. Full hash
        let mut work: Vec<usize> = (0..items.len())
            .filter(|&i| items[i].full.is_none())
            .collect();
        let cached = (items.len() - work.len()) as u64;
        self.stats.full_cached += cached;
        self.cache_hits.fetch_add(cached, Ordering::Relaxed);
        work.sort_by(|&a, &b| items[b].facts.size.cmp(&items[a].facts.size));
        let results = self.hash_phase(&pool, &items, &work, Phase::FullHash)?;
        let mut drop = vec![false; items.len()];
        for (w, r) in work.iter().zip(results) {
            match r {
                Ok(h) => items[*w].full = h.full,
                Err(()) => drop[*w] = true,
            }
        }
        items = keep_unmarked(items, &drop);
        items = regroup(items, |it| (it.facts.size, 0, it.full.unwrap_or([0; 32])));

        Some(self.build_report(&pool, items))
    }

    /// Opens each candidate for attributes only and verifies it.
    fn measure(&mut self, pool: &[Candidate], which: &[usize]) -> Option<Vec<(usize, Facts)>> {
        if which.is_empty() {
            return Some(Vec::new());
        }
        let allow = self.cfg.allow_wof_and_dedup;
        let tracker = self.tracker(Phase::Measuring, which.len() as u64, 0);
        let results = self.par_map(which, |&i| {
            if self.cancel.is_cancelled() {
                return Err(HashFail::Cancelled);
            }
            let c = &pool[i];
            let expect = Expect {
                file_ref: c.file_ref,
                state: None,
                allow_wof_and_dedup: allow,
            };
            let r = hash::open_checked(&c.path, Access::Metadata, &expect)
                .map(|(_, f)| f)
                .map_err(HashFail::Skip);
            tracker.file_done();
            r
        });
        (self.progress)(&tracker.snapshot());
        if self.cancel.is_cancelled() {
            return None;
        }
        let mut out = Vec::with_capacity(which.len());
        for (&i, r) in which.iter().zip(results) {
            self.stats.measured += 1;
            match r {
                Ok(f) => out.push((i, f)),
                Err(HashFail::Skip(reason)) => self.skip(&pool[i], reason),
                Err(HashFail::Cancelled) => return None,
            }
        }
        Some(out)
    }

    fn skip(&mut self, c: &Candidate, reason: SkipReason) {
        self.stats.skipped += 1;
        self.skipped.push(SkippedFile {
            path: c.path.clone(),
            file_ref: c.file_ref,
            reason,
        });
    }

    fn lookup_cache(&mut self, pool: &[Candidate], items: &mut [Item]) {
        let mut by_volume: BTreeMap<&VolumeKey, Vec<usize>> = BTreeMap::new();
        for (i, it) in items.iter().enumerate() {
            by_volume.entry(&pool[it.cand].volume).or_default().push(i);
        }
        for (volume, idx) in by_volume {
            for chunk in idx.chunks(4096) {
                let keys: Vec<HashKey> = chunk.iter().map(|&i| items[i].key(pool)).collect();
                match self.cache.lookup(volume, &keys) {
                    Ok(hits) => {
                        for ((&i, key), hit) in chunk.iter().zip(&keys).zip(hits) {
                            // A backend that ignored size/mtime must not
                            // smuggle in a stale hash.
                            if let Some(h) = hit.filter(|h| h.key == *key) {
                                items[i].partial = Some(h.partial);
                                items[i].full = h.full;
                            }
                        }
                    }
                    Err(_) => self.stats.cache_errors += 1,
                }
            }
        }
    }

    /// Hashes `items[work]` for `phase`. Results are aligned with `work`;
    /// `Err(())` means the file was skipped (already recorded).
    fn hash_phase(
        &mut self,
        pool: &[Candidate],
        items: &[Item],
        work: &[usize],
        phase: Phase,
    ) -> Option<Vec<Result<CachedHash, ()>>> {
        let full = phase == Phase::FullHash;
        let bytes_total: u64 = work
            .iter()
            .map(|&i| {
                let s = items[i].facts.size;
                if full {
                    s
                } else {
                    hash::partial_windows(s).iter().map(|w| w.1).sum()
                }
            })
            .sum();
        let tracker = self.tracker(phase, work.len() as u64, bytes_total);
        let pending: Mutex<Vec<(VolumeKey, CachedHash)>> = Mutex::new(Vec::new());
        let cache_errors = AtomicU64::new(0);
        let cfg = self.cfg;
        let flush = |force: bool| {
            let batch = {
                let mut p = pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !force && p.len() < cfg.cache_batch.max(1) {
                    return;
                }
                std::mem::take(&mut *p)
            };
            let mut by_volume: BTreeMap<VolumeKey, Vec<CachedHash>> = BTreeMap::new();
            for (v, h) in batch {
                by_volume.entry(v).or_default().push(h);
            }
            for (v, rows) in by_volume {
                if self.cache.upsert(&v, &rows).is_err() {
                    cache_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        };
        let results = self.par_map(work, |&i| {
            let it = &items[i];
            let c = &pool[it.cand];
            let r = self.hash_one(c, it, full, &tracker);
            if let Ok(h) = &r {
                pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((c.volume.clone(), *h));
                flush(false);
            }
            tracker.file_done();
            r
        });
        flush(true);
        (self.progress)(&tracker.snapshot());
        let bytes_read = tracker.bytes.load(Ordering::Relaxed);
        self.stats.cache_errors += cache_errors.load(Ordering::Relaxed);
        self.stats.bytes_read += bytes_read;

        let mut out = Vec::with_capacity(work.len());
        let mut changed: BTreeMap<VolumeKey, Vec<FileRef>> = BTreeMap::new();
        let mut cancelled = false;
        for (&i, r) in work.iter().zip(results) {
            let c = &pool[items[i].cand];
            match r {
                Ok(h) => {
                    if full {
                        self.stats.full_hashed += 1;
                    } else {
                        self.stats.partial_hashed += 1;
                    }
                    out.push(Ok(h));
                }
                Err(HashFail::Cancelled) => {
                    cancelled = true;
                    out.push(Err(()));
                }
                Err(HashFail::Skip(reason)) => {
                    if reason == SkipReason::Changed {
                        changed
                            .entry(c.volume.clone())
                            .or_default()
                            .push(c.file_ref);
                    }
                    self.skip(c, reason);
                    out.push(Err(()));
                }
            }
        }
        for (v, refs) in changed {
            if self.cache.invalidate(&v, &refs).is_err() {
                self.stats.cache_errors += 1;
            }
        }
        if cancelled || self.cancel.is_cancelled() {
            return None;
        }
        Some(out)
    }

    fn hash_one(
        &self,
        c: &Candidate,
        it: &Item,
        full: bool,
        tracker: &Tracker<'_>,
    ) -> Result<CachedHash, HashFail> {
        if self.cancel.is_cancelled() {
            return Err(HashFail::Cancelled);
        }
        let expect = Expect {
            file_ref: c.file_ref,
            state: Some((it.facts.size, it.facts.mtime)),
            allow_wof_and_dedup: self.cfg.allow_wof_and_dedup,
        };
        let (file, facts) = hash::open_checked(&c.path, Access::Read, &expect)?;
        let key = HashKey {
            file_ref: c.file_ref,
            size: facts.size,
            mtime: facts.mtime,
        };
        if full {
            let mut buf = vec![0u8; self.cfg.read_buffer.max(64 * 1024)];
            let on_bytes = |n: u64| {
                tracker.bytes.fetch_add(n, Ordering::Relaxed);
                tracker.tick();
            };
            let h = hash::full_hash(
                &file,
                &facts,
                &mut buf,
                &self.throttle,
                self.cancel,
                &on_bytes,
            )?;
            Ok(CachedHash {
                key,
                partial: it.partial.unwrap_or(0),
                full: Some(h),
            })
        } else {
            let mut buf = Vec::new();
            let p = hash::partial_hash(&file, &facts, &mut buf, &self.throttle, self.cancel)?;
            let read: u64 = hash::partial_windows(facts.size).iter().map(|w| w.1).sum();
            tracker.bytes.fetch_add(read, Ordering::Relaxed);
            Ok(CachedHash {
                key,
                partial: p,
                full: None,
            })
        }
    }

    fn build_report(&mut self, pool: &[Candidate], items: Vec<Item>) -> DuplicateReport {
        let mut groups: Vec<DuplicateGroup> = Vec::new();
        let mut items = items;
        items.sort_by(|a, b| {
            (a.facts.size, a.full)
                .cmp(&(b.facts.size, b.full))
                .then_with(|| pool[a.cand].path.cmp(&pool[b.cand].path))
        });
        for run in items.chunk_by(|a, b| a.facts.size == b.facts.size && a.full == b.full) {
            let (Some(hash), true) = (run[0].full, run.len() >= 2) else {
                continue;
            };
            let files = run
                .iter()
                .map(|it| {
                    let c = &pool[it.cand];
                    DupFile {
                        volume_serial: c.volume.serial,
                        file_ref: c.file_ref,
                        path: c.path.clone(),
                        size: it.facts.size,
                        mtime: it.facts.mtime,
                        links: it.facts.links,
                    }
                })
                .collect();
            let mut g = DuplicateGroup {
                id: 0,
                size: run[0].facts.size,
                hash,
                files,
                keep: KeepSuggestion {
                    index: 0,
                    reason: KeepReason::FirstByPath,
                },
            };
            g.suggest(&self.cfg.keep);
            groups.push(g);
        }
        groups.sort_by(|a, b| {
            b.wasted_bytes()
                .cmp(&a.wasted_bytes())
                .then_with(|| a.files[0].path.cmp(&b.files[0].path))
        });
        for (i, g) in groups.iter_mut().enumerate() {
            g.id = i as u64;
        }
        self.stats.groups = groups.len() as u64;
        self.stats.duplicate_files = groups.iter().map(|g| g.files.len() as u64).sum();
        self.stats.wasted_bytes = groups.iter().map(DuplicateGroup::wasted_bytes).sum();
        DuplicateReport {
            groups,
            stats: ScanStats::default(),
            skipped: Vec::new(),
        }
    }
}

/// Keeps items whose group key is shared by at least two items.
fn regroup<K: Ord + Copy>(mut items: Vec<Item>, key: impl Fn(&Item) -> K) -> Vec<Item> {
    items.sort_by_key(|it| key(it));
    let mut out = Vec::with_capacity(items.len());
    for run in items.chunk_by(|a, b| key(a) == key(b)) {
        if run.len() >= 2 {
            out.extend_from_slice(run);
        }
    }
    out
}

fn keep_unmarked(items: Vec<Item>, drop: &[bool]) -> Vec<Item> {
    items
        .into_iter()
        .zip(drop)
        .filter_map(|(it, &d)| (!d).then_some(it))
        .collect()
}
