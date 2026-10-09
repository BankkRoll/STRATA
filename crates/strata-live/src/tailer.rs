//! The USN tailer: read, coalesce, fetch, apply.
//!
//! One [`Tailer::step`]:
//! 1. Reads the journal from the current position. The read blocks; its
//!    timeout is the next deadline (tick flush or periodic save), and with
//!    nothing pending there is no deadline at all, so an idle volume costs
//!    one blocked call and no CPU.
//! 2. Parses the buffer (`USN_RECORD_V2/V3/V4`) and coalesces records by
//!    file reference ([`crate::reason`] explains which bits matter and why
//!    close records must trigger a refresh).
//! 3. When the tick deadline (first record + [`TailerConfig::tick`]) passes,
//!    flushes: deleted files become removes without a fetch; everything else
//!    is fetched from the [`RecordSource`] in batches and applied with
//!    [`Index::apply`] in short lock holds. Directories whose name set
//!    changed are refreshed too, because NTFS journals no record for a
//!    parent whose index allocation grew or shrank. Each tick emits one
//!    merged [`ChangeSet`].
//!
//! Work per tick is bounded ([`TailerConfig::max_refresh_per_tick`] and
//! [`TailerConfig::tick_budget`]); a larger backlog carries over and the
//! status reports [`LiveStatus::CatchingUp`], so the UI stays responsive
//! during bursts. Reading pauses while the backlog exceeds
//! [`TailerConfig::max_pending`], bounding memory.
//!
//! The applied position only advances when the dirty set is empty, so it
//! never covers a record whose change is not in the index yet.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use strata_core::FileRef;
use strata_index::{ChangeSet, Index, Update};
use strata_ntfs::{UsnRecord, parse_usn_buffer};

use crate::cache::{CacheFile, CachePolicy};
use crate::coalesce::{Coalescer, Noted, Plan};
use crate::merge::ChangeMerger;
use crate::reason;
use crate::source::{Clock, Fetched, JournalSource, RecordSource, SourceError};
use crate::status::{Halt, JournalPosition, RescanReason, StaleReason, check_position};

// -----------------------------------------------------------------------------
// Configuration and reports
// -----------------------------------------------------------------------------

/// Tailer tuning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailerConfig {
    /// Debounce window: changes are applied this long after the first
    /// unapplied record arrived.
    pub tick: Duration,
    /// References per [`RecordSource::fetch`] call.
    pub fetch_batch: usize,
    /// Updates per index lock hold.
    pub apply_batch: usize,
    /// Most references refreshed or removed in one tick.
    pub max_refresh_per_tick: usize,
    /// Wall-time budget of one tick's flush; the rest carries over.
    pub tick_budget: Duration,
    /// Reading pauses while this many references are pending.
    pub max_pending: usize,
    /// Refresh directories whose name set changed (create, delete, rename,
    /// hardlink), so their index allocation stays accurate.
    pub refresh_parents: bool,
    /// Ticks a rename's old half waits for its new half before the file is
    /// refreshed anyway.
    pub rename_grace_ticks: u64,
    /// Cache save policy (used when a cache file is attached).
    pub cache: CachePolicy,
}

impl Default for TailerConfig {
    fn default() -> Self {
        Self {
            tick: Duration::from_millis(250),
            fetch_batch: 1024,
            apply_batch: 2048,
            max_refresh_per_tick: 16_384,
            tick_budget: Duration::from_millis(150),
            max_pending: 262_144,
            refresh_parents: true,
            rename_grace_ticks: 1,
            cache: CachePolicy::default(),
        }
    }
}

/// Whether the index reflects the journal head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveStatus {
    /// Changes appear within one tick.
    Live,
    /// Replaying a backlog ("Catching up… N changes").
    CatchingUp {
        /// Files waiting to be refreshed or removed.
        pending: usize,
        /// Journal bytes not read yet, relative to the head when catch-up
        /// started.
        unread_bytes: u64,
    },
}

/// What one tick did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickReport {
    /// Merged change set for the UI.
    pub changes: ChangeSet,
    /// Status after the tick.
    pub status: LiveStatus,
    /// References fetched.
    pub fetched: usize,
    /// Upserts applied.
    pub upserts: usize,
    /// Removes applied (deletes plus fetched-missing).
    pub removes: usize,
    /// References the source neither returned nor reported missing.
    pub unavailable: usize,
    /// Updates dropped because they would remove or replace the root.
    pub skipped: usize,
    /// Wall time of the flush.
    pub elapsed: Duration,
    /// Applied position after the tick, when it advanced.
    pub applied: Option<JournalPosition>,
}

/// Something the app should know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEvent {
    /// A tick was applied.
    Tick(TickReport),
    /// The status changed.
    Status(LiveStatus),
    /// The cache was saved at this position.
    Saved(JournalPosition),
    /// Saving the cache failed (tailing continues).
    SaveFailed(String),
}

/// Counters since the tailer started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TailStats {
    /// Journal reads.
    pub reads: u64,
    /// Records parsed.
    pub records: u64,
    /// Records whose bits change nothing the index stores.
    pub ignored: u64,
    /// Renames whose old and new halves were paired.
    pub paired_renames: u64,
    /// Flushes that did work.
    pub ticks: u64,
    /// References fetched.
    pub fetched: u64,
    /// Updates applied.
    pub updates: u64,
}

// -----------------------------------------------------------------------------
// Index access
// -----------------------------------------------------------------------------

/// How the tailer reaches the index. Each call is one short lock hold.
pub trait IndexAccess {
    /// Runs `f` with exclusive access. Returns `false` if the index is
    /// unavailable (a poisoned lock).
    fn with_index(&mut self, f: &mut dyn FnMut(&mut Index)) -> bool;
}

impl IndexAccess for Index {
    fn with_index(&mut self, f: &mut dyn FnMut(&mut Index)) -> bool {
        f(self);
        true
    }
}

impl IndexAccess for &Mutex<Index> {
    fn with_index(&mut self, f: &mut dyn FnMut(&mut Index)) -> bool {
        match self.lock() {
            Ok(mut g) => {
                f(&mut g);
                true
            }
            Err(_) => false,
        }
    }
}

impl IndexAccess for &RwLock<Index> {
    fn with_index(&mut self, f: &mut dyn FnMut(&mut Index)) -> bool {
        match self.write() {
            Ok(mut g) => {
                f(&mut g);
                true
            }
            Err(_) => false,
        }
    }
}

impl IndexAccess for Arc<Mutex<Index>> {
    fn with_index(&mut self, f: &mut dyn FnMut(&mut Index)) -> bool {
        (&**self).with_index(f)
    }
}

impl IndexAccess for Arc<RwLock<Index>> {
    fn with_index(&mut self, f: &mut dyn FnMut(&mut Index)) -> bool {
        (&**self).with_index(f)
    }
}

// -----------------------------------------------------------------------------
// Tailer
// -----------------------------------------------------------------------------

/// Tails one volume's journal into an [`Index`].
///
/// # Example
///
/// ```no_run
/// # use strata_live::*;
/// # fn demo(journal: &mut dyn JournalSource, records: &mut dyn RecordSource,
/// #         index: std::sync::Arc<std::sync::RwLock<strata_index::Index>>,
/// #         stop: &std::sync::atomic::AtomicBool) -> Result<(), Halt> {
/// let pos = index_position(&index.read().unwrap());
/// let mut tailer = Tailer::start(TailerConfig::default(), journal, pos)?
///     .with_cache(CacheFile::new("volume.idx"));
/// let mut access = index.clone();
/// let halt = tailer.run(journal, records, &mut access, &SystemClock, stop, &mut |event| {
///     // Forward `LiveEvent::Tick(report)` change sets to the UI.
///     let _ = event;
/// });
/// # let _ = halt; Ok(()) }
/// ```
#[derive(Debug)]
pub struct Tailer {
    cfg: TailerConfig,
    journal_id: u64,
    /// Next USN to read.
    read_usn: i64,
    /// Every change before this USN is in the index.
    applied_usn: i64,
    /// Journal head when catch-up started.
    head: i64,
    coalescer: Coalescer,
    deadline: Option<Instant>,
    cache: Option<CacheFile>,
    unsaved: bool,
    last_save: Option<Instant>,
    last_progress: Option<Instant>,
    status: LiveStatus,
    stats: TailStats,
}

impl Tailer {
    /// Validates `pos` against the journal and prepares to replay from it.
    ///
    /// After a full scan, `pos` is the journal id and next USN queried
    /// *before* the scan started (changes during the scan are replayed,
    /// which is harmless). After loading a cache it is
    /// [`crate::index_position`] of the loaded index.
    ///
    /// # Errors
    ///
    /// [`Halt::JournalDisabled`], [`Halt::NeedsRescan`] (wrap, id change)
    /// or a [`Halt::Stale`] if the query fails.
    pub fn start(
        cfg: TailerConfig,
        journal: &mut dyn JournalSource,
        pos: JournalPosition,
    ) -> Result<Self, Halt> {
        let mut t = Self {
            cfg,
            journal_id: pos.journal_id,
            read_usn: pos.usn,
            applied_usn: pos.usn,
            head: pos.usn,
            coalescer: Coalescer::default(),
            deadline: None,
            cache: None,
            unsaved: false,
            last_save: None,
            last_progress: None,
            status: LiveStatus::Live,
            stats: TailStats::default(),
        };
        t.resume(journal)?;
        Ok(t)
    }

    /// Attaches the cache file saved by the [`CachePolicy`].
    #[must_use]
    pub fn with_cache(mut self, cache: CacheFile) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Re-validates the journal and restarts from the applied position: after
    /// a [`Halt::Stale`] once the source is back, and after resume from
    /// sleep. Unapplied records are dropped and read again.
    ///
    /// # Errors
    ///
    /// As [`Tailer::start`].
    pub fn resume(&mut self, journal: &mut dyn JournalSource) -> Result<(), Halt> {
        let info = journal.query().map_err(|e| self.halt_for(e, journal))?;
        let info = check_position(self.position(), info)?;
        self.read_usn = self.applied_usn;
        self.head = info.next_usn;
        self.coalescer.clear();
        self.deadline = None;
        self.status = self.compute_status();
        Ok(())
    }

    /// The applied position (what a cache saved now would record).
    #[must_use]
    pub fn position(&self) -> JournalPosition {
        JournalPosition {
            journal_id: self.journal_id,
            usn: self.applied_usn,
        }
    }

    /// The next USN to read.
    #[must_use]
    pub fn read_position(&self) -> i64 {
        self.read_usn
    }

    /// Current status.
    #[must_use]
    pub fn status(&self) -> LiveStatus {
        self.status
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> TailStats {
        self.stats
    }

    /// Files waiting to be refreshed or removed.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.coalescer.len()
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &TailerConfig {
        &self.cfg
    }

    /// Runs until stopped or halted. Stopping requires `stop` to be set *and*
    /// a blocked [`JournalSource::read`] to return (the app cancels it, which
    /// yields [`SourceError::Cancelled`]). Saves the cache on a clean or
    /// stale stop when the policy says so.
    pub fn run(
        &mut self,
        journal: &mut dyn JournalSource,
        records: &mut dyn RecordSource,
        index: &mut dyn IndexAccess,
        clock: &dyn Clock,
        stop: &AtomicBool,
        events: &mut dyn FnMut(LiveEvent),
    ) -> Halt {
        let halt = loop {
            if stop.load(Ordering::Acquire) {
                break Halt::Stopped;
            }
            if let Err(h) = self.step(journal, records, index, clock, events) {
                break h;
            }
        };
        if self.cfg.cache.save_on_stop && matches!(halt, Halt::Stopped | Halt::Stale(_)) {
            self.save(index, clock.now(), events);
        }
        halt
    }

    /// One read and, when due, one flush and one periodic save.
    ///
    /// # Errors
    ///
    /// A [`Halt`]; the caller stops tailing.
    pub fn step(
        &mut self,
        journal: &mut dyn JournalSource,
        records: &mut dyn RecordSource,
        index: &mut dyn IndexAccess,
        clock: &dyn Clock,
        events: &mut dyn FnMut(LiveEvent),
    ) -> Result<(), Halt> {
        let now = clock.now();
        self.last_save.get_or_insert(now);
        if self.coalescer.len() < self.cfg.max_pending {
            let wait = self.wait(now);
            let buf = journal
                .read(self.journal_id, self.read_usn, wait)
                .map_err(|e| self.halt_for(e, journal))?;
            self.stats.reads += 1;
            let had_work = !self.coalescer.is_empty();
            self.ingest(&buf)?;
            if !had_work && !self.coalescer.is_empty() {
                self.deadline = Some(clock.now() + self.cfg.tick);
            } else if self.coalescer.is_empty() && self.applied_usn != self.read_usn {
                // Only irrelevant records arrived: the position advances
                // without touching the index.
                self.applied_usn = self.read_usn;
                self.unsaved = true;
            }
        }
        let now = clock.now();
        if self.deadline.is_some_and(|d| now >= d) {
            self.flush(records, index, clock, events)?;
        }
        if self.save_due(clock.now()) {
            self.save(index, clock.now(), events);
        }
        self.publish_status(clock.now(), events);
        Ok(())
    }

    /// Saves the cache now (for example before the system suspends).
    pub fn save_now(
        &mut self,
        index: &mut dyn IndexAccess,
        clock: &dyn Clock,
        events: &mut dyn FnMut(LiveEvent),
    ) {
        self.unsaved = true;
        self.save(index, clock.now(), events);
    }

    // -------------------------------------------------------------------------
    // Reading
    // -------------------------------------------------------------------------

    fn wait(&self, now: Instant) -> Option<Duration> {
        let mut deadline = self.deadline;
        if self.cache.is_some() && self.unsaved {
            let save_at = self.last_save.unwrap_or(now) + self.cfg.cache.interval;
            deadline = Some(deadline.map_or(save_at, |d| d.min(save_at)));
        }
        deadline.map(|d| d.saturating_duration_since(now))
    }

    fn ingest(&mut self, buf: &[u8]) -> Result<(), Halt> {
        let malformed = |e: strata_ntfs::usn::UsnError| {
            Halt::NeedsRescan(RescanReason::MalformedJournal(e.to_string()))
        };
        let (next, records) = parse_usn_buffer(buf).map_err(malformed)?;
        for rec in records {
            let rec = rec.map_err(malformed)?;
            let (file, parent, bits) = match &rec {
                UsnRecord::Change(c) => (c.file, c.parent, c.reason),
                UsnRecord::Range(r) => (r.file, r.parent, r.reason),
            };
            let (Some(file), Some(parent)) = (file.as_file_ref(), parent.as_file_ref()) else {
                return Err(Halt::NeedsRescan(RescanReason::UnsupportedFileIds));
            };
            self.stats.records += 1;
            match self.coalescer.note(file, bits) {
                Noted::Ignored => self.stats.ignored += 1,
                Noted::PairedRename => self.stats.paired_renames += 1,
                Noted::Scheduled => {}
            }
            if self.cfg.refresh_parents && reason::is_namespace(bits) && parent != file {
                self.coalescer.touch(parent);
            }
        }
        self.read_usn = self.read_usn.max(next);
        Ok(())
    }

    fn halt_for(&self, e: SourceError, journal: &mut dyn JournalSource) -> Halt {
        match e {
            SourceError::JournalInactive => Halt::JournalDisabled,
            SourceError::JournalReset => match journal.query() {
                Ok(None) => Halt::JournalDisabled,
                found => Halt::NeedsRescan(RescanReason::JournalIdChanged {
                    expected: self.journal_id,
                    found: found.ok().flatten().map(|i| i.journal_id),
                }),
            },
            SourceError::UsnPurged => {
                let first = journal
                    .query()
                    .ok()
                    .flatten()
                    .map_or(0, |i| i.oldest_readable());
                Halt::NeedsRescan(RescanReason::JournalWrapped {
                    saved_usn: self.applied_usn,
                    first_usn: first,
                })
            }
            SourceError::VolumeGone => Halt::Stale(StaleReason::VolumeGone),
            SourceError::Disconnected => Halt::Stale(StaleReason::Disconnected),
            SourceError::Cancelled => Halt::Stopped,
            SourceError::Io(s) => Halt::Stale(StaleReason::Io(s)),
        }
    }

    // -------------------------------------------------------------------------
    // Flushing
    // -------------------------------------------------------------------------

    fn flush(
        &mut self,
        records: &mut dyn RecordSource,
        index: &mut dyn IndexAccess,
        clock: &dyn Clock,
        events: &mut dyn FnMut(LiveEvent),
    ) -> Result<(), Halt> {
        let started = clock.now();
        let mut merger = ChangeMerger::default();
        let mut report = TickReport {
            changes: ChangeSet::default(),
            status: self.status,
            fetched: 0,
            upserts: 0,
            removes: 0,
            unavailable: 0,
            skipped: 0,
            elapsed: Duration::ZERO,
            applied: None,
        };
        let mut processed = 0;
        let mut did_work = false;
        while processed < self.cfg.max_refresh_per_tick
            && clock.now().saturating_duration_since(started) < self.cfg.tick_budget
        {
            let room = self
                .cfg
                .fetch_batch
                .min(self.cfg.max_refresh_per_tick - processed)
                .max(1);
            let plan = self.coalescer.take(room, self.cfg.rename_grace_ticks);
            if plan.is_empty() {
                break;
            }
            did_work = true;
            processed += plan.len();
            let fetched = if plan.refresh.is_empty() {
                Fetched::default()
            } else {
                records
                    .fetch(&plan.refresh)
                    .map_err(|e| self.halt_for_fetch(e))?
            };
            report.fetched += plan.refresh.len();
            self.stats.fetched += plan.refresh.len() as u64;
            self.apply(plan, fetched, index, &mut merger, &mut report)?;
        }
        self.coalescer.advance_epoch();

        let now = clock.now();
        if self.coalescer.is_empty() {
            self.deadline = None;
            if self.applied_usn != self.read_usn {
                self.applied_usn = self.read_usn;
                let pos = self.position();
                if !index.with_index(&mut |idx| idx.set_usn_position(pos.journal_id, pos.usn)) {
                    return Err(poisoned());
                }
                report.applied = Some(pos);
                self.unsaved = true;
            }
        } else if self.coalescer.ready() > 0 {
            // Backlog: flush again right after the next (non-blocking) read.
            self.deadline = Some(now);
        } else {
            // Only renames waiting for their partner.
            self.deadline = Some(now + self.cfg.tick);
        }
        if did_work {
            self.stats.ticks += 1;
            // `publish_status` owns transitions, so it can announce them.
            report.status = self.compute_status();
            report.changes = merger.finish();
            report.elapsed = now.saturating_duration_since(started);
            events(LiveEvent::Tick(report));
        }
        Ok(())
    }

    fn halt_for_fetch(&self, e: SourceError) -> Halt {
        match e {
            SourceError::Cancelled => Halt::Stopped,
            SourceError::VolumeGone => Halt::Stale(StaleReason::VolumeGone),
            SourceError::Disconnected => Halt::Stale(StaleReason::Disconnected),
            other => Halt::Stale(StaleReason::Io(other.to_string())),
        }
    }

    /// Turns a plan and its fetch result into index updates and applies them
    /// in lock-sized chunks.
    fn apply(
        &mut self,
        plan: Plan,
        fetched: Fetched,
        index: &mut dyn IndexAccess,
        merger: &mut ChangeMerger,
        report: &mut TickReport,
    ) -> Result<(), Halt> {
        let Plan { remove, refresh } = plan;
        let Fetched { records, missing } = fetched;

        let mut answered: std::collections::HashSet<FileRef> = missing.iter().copied().collect();
        answered.extend(records.iter().map(|r| r.id));
        report.unavailable += refresh.iter().filter(|r| !answered.contains(r)).count();

        let mut updates: Vec<Update> =
            Vec::with_capacity(remove.len() + missing.len() + records.len());
        updates.extend(remove.into_iter().chain(missing).map(Update::Remove));
        updates.extend(records.into_iter().map(Update::Upsert));

        let fetched_now: std::collections::HashSet<FileRef> = refresh.into_iter().collect();
        let refresh_parents = self.cfg.refresh_parents;
        let mut parents: Vec<FileRef> = Vec::new();
        let mut chunks = updates.into_iter().peekable();
        while chunks.peek().is_some() {
            let chunk: Vec<Update> = chunks.by_ref().take(self.cfg.apply_batch.max(1)).collect();
            let mut chunk = Some(chunk);
            let mut result = None;
            let ok = index.with_index(&mut |idx| {
                let Some(chunk) = chunk.take() else { return };
                let (kept, skipped) = guard_root(idx, chunk);
                if refresh_parents {
                    changed_parents(idx, &kept, &mut parents);
                }
                let counts = count(&kept);
                result = Some((idx.apply(kept), counts, skipped));
            });
            if !ok {
                return Err(poisoned());
            }
            let Some((applied, (upserts, removes), skipped)) = result else {
                return Err(poisoned());
            };
            let cs = applied
                .map_err(|e| Halt::NeedsRescan(RescanReason::IndexRejected(e.to_string())))?;
            merger.push(cs);
            report.upserts += upserts;
            report.removes += removes;
            report.skipped += skipped;
            self.stats.updates += (upserts + removes) as u64;
        }
        // The fetch of a directory in this batch already saw its children's
        // changes, so it needs no second refresh.
        for p in parents {
            if !fetched_now.contains(&p) {
                self.coalescer.touch(p);
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Status and saving
    // -------------------------------------------------------------------------

    fn compute_status(&self) -> LiveStatus {
        let pending = self.coalescer.len();
        if self.read_usn < self.head || pending > self.cfg.max_refresh_per_tick {
            LiveStatus::CatchingUp {
                pending,
                unread_bytes: u64::try_from(self.head - self.read_usn).unwrap_or(0),
            }
        } else {
            LiveStatus::Live
        }
    }

    /// Announces state changes at once and progress counts at most once per
    /// tick, so a burst does not flood the UI.
    fn publish_status(&mut self, now: Instant, events: &mut dyn FnMut(LiveEvent)) {
        let s = self.compute_status();
        if s == self.status {
            return;
        }
        let same_state = matches!(
            (s, self.status),
            (LiveStatus::CatchingUp { .. }, LiveStatus::CatchingUp { .. })
        );
        if same_state
            && self
                .last_progress
                .is_some_and(|t| now.saturating_duration_since(t) < self.cfg.tick)
        {
            return;
        }
        self.status = s;
        self.last_progress = Some(now);
        events(LiveEvent::Status(s));
    }

    fn save_due(&self, now: Instant) -> bool {
        self.cache.is_some()
            && self.unsaved
            && self.coalescer.is_empty()
            && self
                .last_save
                .is_none_or(|t| now.saturating_duration_since(t) >= self.cfg.cache.interval)
    }

    fn save(
        &mut self,
        index: &mut dyn IndexAccess,
        now: Instant,
        events: &mut dyn FnMut(LiveEvent),
    ) {
        let Some(cache) = &self.cache else { return };
        if !self.unsaved {
            return;
        }
        let pos = self.position();
        let mut bytes = None;
        let ok = index.with_index(&mut |idx| {
            idx.set_usn_position(pos.journal_id, pos.usn);
            bytes = Some(idx.to_bytes());
        });
        let Some(bytes) = bytes.filter(|_| ok) else {
            events(LiveEvent::SaveFailed("index unavailable".into()));
            return;
        };
        match cache.write(&bytes) {
            Ok(()) => {
                self.unsaved = false;
                self.last_save = Some(now);
                events(LiveEvent::Saved(pos));
            }
            Err(e) => {
                // Retry at the next interval rather than on every step.
                self.last_save = Some(now);
                events(LiveEvent::SaveFailed(e.to_string()));
            }
        }
    }
}

fn poisoned() -> Halt {
    Halt::NeedsRescan(RescanReason::IndexRejected("index lock poisoned".into()))
}

fn count(updates: &[Update]) -> (usize, usize) {
    let removes = updates
        .iter()
        .filter(|u| matches!(u, Update::Remove(_)))
        .count();
    (updates.len() - removes, removes)
}

/// Drops updates the index would reject: removing the root, replacing it
/// with another record, or the reserved reference.
fn guard_root(idx: &Index, updates: Vec<Update>) -> (Vec<Update>, usize) {
    let root = idx.file_ref(idx.root());
    let before = updates.len();
    let kept: Vec<Update> = updates
        .into_iter()
        .filter(|u| match u {
            Update::Remove(r) => Some(*r) != root,
            Update::Upsert(rec) => {
                rec.id.0 != u64::MAX
                    && root.is_none_or(|root| {
                        rec.id.record() != root.record() || (rec.id == root && rec.is_dir())
                    })
            }
        })
        .collect();
    let skipped = before - kept.len();
    (kept, skipped)
}

/// On-disk parent of an entry (its display parent unless it is detached).
fn parent_ref(idx: &Index, e: strata_index::EntryId) -> Option<FileRef> {
    idx.intended_parent(e)
        .or_else(|| idx.parent(e).and_then(|p| idx.file_ref(p)))
}

/// Collects directories whose name set the updates change: parents of
/// removed links and of links added, removed or renamed by an upsert.
fn changed_parents(idx: &Index, updates: &[Update], out: &mut Vec<FileRef>) {
    for u in updates {
        match u {
            Update::Remove(r) => {
                out.extend(idx.links(*r).into_iter().filter_map(|e| parent_ref(idx, e)));
            }
            Update::Upsert(rec) => {
                let old: Vec<(FileRef, Vec<u16>)> = idx
                    .links(rec.id)
                    .into_iter()
                    .filter_map(|e| Some((parent_ref(idx, e)?, idx.name(e).units().to_vec())))
                    .collect();
                for l in &rec.links {
                    if !old
                        .iter()
                        .any(|(p, n)| *p == l.parent && n.as_slice() == l.name.units())
                    {
                        out.push(l.parent);
                    }
                }
                for (p, n) in &old {
                    if !rec
                        .links
                        .iter()
                        .any(|l| l.parent == *p && l.name.units() == n.as_slice())
                    {
                        out.push(*p);
                    }
                }
            }
        }
    }
    out.retain(|p| p.0 != u64::MAX);
}
