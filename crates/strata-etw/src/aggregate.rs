//! Rolling activity windows, hourly rollups and last writers.
//!
//! Every attributed event adds [`Counts`] for one (process, directory) at the
//! event's own timestamp (never the time it was processed), into:
//! - **minute buckets** for the last hour, serving the "now" and "last hour"
//!   windows;
//! - **hour buckets** for the last 48 hours, serving "today" (from a
//!   caller-supplied local midnight) and the attribution evidence;
//! - **pending rollups** keyed by UTC hour, drained by [`Aggregator::drain`]
//!   as [`ActivitySample`]s for [`strata_store::Store::record_activity`]
//!   (which sums repeated flushes into the same hour, so draining often is
//!   harmless);
//! - **pending last writers** per path hash, drained as [`LastWrite`]s for
//!   [`strata_store::Store::set_last_writers`].
//!
//! Retention of persisted data is the store's job; the in-memory windows only
//! keep what the views need.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};
use strata_store::{ActivitySample, LastWrite, Timestamp};

use crate::paths::Dir;
use crate::processes::Proc;

/// Activity counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Counts {
    /// Bytes written (scaled up while sampling).
    pub bytes_written: u64,
    /// Files created.
    pub files_created: u64,
    /// Files deleted.
    pub files_deleted: u64,
}

impl Counts {
    /// Component-wise saturating sum.
    pub fn add(&mut self, o: Self) {
        self.bytes_written = self.bytes_written.saturating_add(o.bytes_written);
        self.files_created = self.files_created.saturating_add(o.files_created);
        self.files_deleted = self.files_deleted.saturating_add(o.files_deleted);
    }

    /// A single write of `bytes`.
    #[must_use]
    pub const fn write(bytes: u64) -> Self {
        Self {
            bytes_written: bytes,
            files_created: 0,
            files_deleted: 0,
        }
    }

    /// One created file.
    #[must_use]
    pub const fn created() -> Self {
        Self {
            bytes_written: 0,
            files_created: 1,
            files_deleted: 0,
        }
    }

    /// One deleted file.
    #[must_use]
    pub const fn deleted() -> Self {
        Self {
            bytes_written: 0,
            files_created: 0,
            files_deleted: 1,
        }
    }

    /// Activity score used for shares: bytes plus 4 KiB per created or
    /// deleted file, so metadata-only churn still counts.
    #[must_use]
    pub const fn score(&self) -> u64 {
        self.bytes_written
            .saturating_add((self.files_created + self.files_deleted).saturating_mul(4096))
    }

    /// Whether every counter is zero.
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.bytes_written == 0 && self.files_created == 0 && self.files_deleted == 0
    }
}

/// A view window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Window {
    /// The current and the previous minute (60-120 s of activity).
    Now,
    /// The last 60 minutes.
    LastHour,
    /// Every hour starting at or after this instant (rounded down to the
    /// hour), up to 48 hours back. "Today" passes local midnight.
    Since(Timestamp),
}

/// Totals for one process image in one directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirTotal {
    /// Full image path.
    pub image: String,
    /// Directory (DOS path).
    pub dir: String,
    /// Counters.
    pub counts: Counts,
}

/// Totals for one process image over a window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriterSummary {
    /// Full image path.
    pub image: String,
    /// Counters summed over all directories.
    pub counts: Counts,
    /// Distinct directories touched.
    pub dirs: u32,
}

/// What [`Aggregator::drain`] hands to the store.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ActivityBatch {
    /// Hourly rollup deltas for `Store::record_activity`.
    pub samples: Vec<ActivitySample>,
    /// Last writers for `Store::set_last_writers`.
    pub last_writes: Vec<LastWrite>,
}

impl ActivityBatch {
    /// Whether there is nothing to persist.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty() && self.last_writes.is_empty()
    }
}

type Key = (Proc, Dir);

#[derive(Debug)]
struct Bucket {
    start: i64,
    data: HashMap<Key, Counts>,
}

#[derive(Debug)]
struct Ring {
    width: i64,
    keep: i64,
    buckets: VecDeque<Bucket>,
}

impl Ring {
    const fn new(width: i64, keep: i64) -> Self {
        Self {
            width,
            keep,
            buckets: VecDeque::new(),
        }
    }

    fn add(&mut self, at: i64, key: &Key, c: Counts) {
        let start = at.div_euclid(self.width) * self.width;
        if let Some(newest) = self.buckets.back()
            && start < newest.start - self.width * (self.keep - 1)
        {
            return;
        }
        // NOTE: events arrive almost in order; search from the newest bucket.
        let pos = self.buckets.iter().rposition(|b| b.start <= start);
        let bucket = match pos {
            Some(i) if self.buckets[i].start == start => &mut self.buckets[i],
            Some(i) => {
                self.buckets.insert(
                    i + 1,
                    Bucket {
                        start,
                        data: HashMap::new(),
                    },
                );
                &mut self.buckets[i + 1]
            }
            None => {
                self.buckets.push_front(Bucket {
                    start,
                    data: HashMap::new(),
                });
                &mut self.buckets[0]
            }
        };
        bucket.data.entry(key.clone()).or_default().add(c);
        self.trim();
    }

    fn trim(&mut self) {
        if let Some(newest) = self.buckets.back().map(|b| b.start) {
            let oldest = newest - self.width * (self.keep - 1);
            while self.buckets.front().is_some_and(|b| b.start < oldest) {
                self.buckets.pop_front();
            }
        }
    }

    fn expire(&mut self, now: i64) {
        let oldest = now.div_euclid(self.width) * self.width - self.width * (self.keep - 1);
        while self.buckets.front().is_some_and(|b| b.start < oldest) {
            self.buckets.pop_front();
        }
    }

    fn since(&self, from: i64) -> impl Iterator<Item = &Bucket> {
        self.buckets.iter().filter(move |b| b.start >= from)
    }
}

/// In-memory activity state.
#[derive(Debug)]
pub struct Aggregator {
    minutes: Ring,
    hours: Ring,
    pending: HashMap<(i64, Key), Counts>,
    last: HashMap<u64, (Proc, Timestamp)>,
}

impl Default for Aggregator {
    fn default() -> Self {
        Self {
            minutes: Ring::new(60, 61),
            hours: Ring::new(3600, 49),
            pending: HashMap::new(),
            last: HashMap::new(),
        }
    }
}

impl Aggregator {
    /// Adds counts for `proc` in `dir` at `at`.
    pub fn add(&mut self, proc: &Proc, dir: &Dir, at: Timestamp, c: Counts) {
        if c.is_zero() {
            return;
        }
        let key = (proc.clone(), dir.clone());
        self.minutes.add(at.0, &key, c);
        self.hours.add(at.0, &key, c);
        self.pending
            .entry((at.hour_start().0, key))
            .or_default()
            .add(c);
    }

    /// Records `proc` as the latest writer of the path with `path_hash`.
    pub fn note_writer(&mut self, path_hash: u64, proc: &Proc, at: Timestamp) {
        match self.last.get_mut(&path_hash) {
            Some(e) if e.1 > at => {}
            Some(e) => *e = (proc.clone(), at),
            None => {
                self.last.insert(path_hash, (proc.clone(), at));
            }
        }
    }

    /// Moves a pending last-writer entry after a rename.
    pub fn rename_writer(&mut self, old_hash: u64, new_hash: u64) {
        if let Some(e) = self.last.remove(&old_hash) {
            match self.last.get(&new_hash) {
                Some(n) if n.1 > e.1 => {}
                _ => {
                    self.last.insert(new_hash, e);
                }
            }
        }
    }

    /// Takes everything pending for the store.
    pub fn drain(&mut self) -> ActivityBatch {
        let mut samples: Vec<ActivitySample> = self
            .pending
            .drain()
            .map(|((hour, (p, d)), c)| ActivitySample {
                at: Timestamp(hour),
                image: p.0.image.to_string(),
                dir_hash: d.0.hash,
                bytes_written: c.bytes_written,
                files_created: c.files_created,
                files_deleted: c.files_deleted,
            })
            .collect();
        samples.sort_by(|a, b| (a.at, &a.image, a.dir_hash).cmp(&(b.at, &b.image, b.dir_hash)));
        let mut last_writes: Vec<LastWrite> = self
            .last
            .drain()
            .map(|(h, (p, at))| LastWrite {
                path_hash: h,
                image: p.0.image.to_string(),
                pid: Some(p.0.pid),
                at,
            })
            .collect();
        last_writes.sort_by_key(|w| (w.at, w.path_hash));
        ActivityBatch {
            samples,
            last_writes,
        }
    }

    /// Drops window buckets that fell out of range at `now`.
    pub fn expire(&mut self, now: Timestamp) {
        self.minutes.expire(now.0);
        self.hours.expire(now.0);
    }

    fn window_buckets(&self, w: Window, now: Timestamp) -> Vec<&Bucket> {
        match w {
            Window::Now => self.minutes.since(now.0.div_euclid(60) * 60 - 60).collect(),
            Window::LastHour => self
                .minutes
                .since(now.0.div_euclid(60) * 60 - 59 * 60)
                .collect(),
            Window::Since(t) => self.hours.since(t.hour_start().0).collect(),
        }
    }

    fn window_totals(&self, w: Window, now: Timestamp) -> HashMap<Key, Counts> {
        let mut out: HashMap<Key, Counts> = HashMap::new();
        for b in self.window_buckets(w, now) {
            for (k, c) in &b.data {
                out.entry(k.clone()).or_default().add(*c);
            }
        }
        out
    }

    /// Per-process, per-directory totals over `w`, largest score first.
    #[must_use]
    pub fn dir_totals(&self, w: Window, now: Timestamp, limit: usize) -> Vec<DirTotal> {
        let mut v: Vec<DirTotal> = self
            .window_totals(w, now)
            .into_iter()
            .map(|((p, d), counts)| DirTotal {
                image: p.0.image.to_string(),
                dir: d.0.path.to_string(),
                counts,
            })
            .collect();
        v.sort_by(|a, b| {
            b.counts
                .score()
                .cmp(&a.counts.score())
                .then_with(|| (&a.image, &a.dir).cmp(&(&b.image, &b.dir)))
        });
        v.truncate(limit);
        v
    }

    /// Top writers by image over `w` (all instances of an image summed, as the
    /// store does), largest bytes written first.
    #[must_use]
    pub fn top_writers(&self, w: Window, now: Timestamp, limit: usize) -> Vec<WriterSummary> {
        let mut by_image: HashMap<String, (Counts, u32)> = HashMap::new();
        for ((p, _), c) in self.window_totals(w, now) {
            let e = by_image.entry(p.0.image.to_string()).or_default();
            e.0.add(c);
            e.1 += 1;
        }
        let mut v: Vec<WriterSummary> = by_image
            .into_iter()
            .map(|(image, (counts, dirs))| WriterSummary {
                image,
                counts,
                dirs,
            })
            .collect();
        v.sort_by(|a, b| {
            b.counts
                .bytes_written
                .cmp(&a.counts.bytes_written)
                .then_with(|| b.counts.score().cmp(&a.counts.score()))
                .then_with(|| a.image.cmp(&b.image))
        });
        v.truncate(limit);
        v
    }

    /// Per (directory, image) activity over the hour buckets since `since`,
    /// with the number of distinct active hours: the input of
    /// [`crate::evidence::evidence`].
    #[must_use]
    pub fn hourly_profile(&self, since: Timestamp) -> Vec<crate::evidence::DirActivity> {
        let mut acc: HashMap<(Dir, String), (Counts, u32)> = HashMap::new();
        for b in self.hours.since(since.hour_start().0) {
            let mut seen: HashMap<(Dir, String), Counts> = HashMap::new();
            for ((p, d), c) in &b.data {
                seen.entry((d.clone(), p.0.image.to_string()))
                    .or_default()
                    .add(*c);
            }
            for (k, c) in seen {
                let e = acc.entry(k).or_default();
                e.0.add(c);
                e.1 += 1;
            }
        }
        acc.into_iter()
            .map(
                |((d, image), (counts, hours))| crate::evidence::DirActivity {
                    dir: d.0.path.to_string(),
                    image,
                    counts,
                    active_hours: hours,
                },
            )
            .collect()
    }

    /// Forgets everything, including data not yet drained.
    pub fn clear(&mut self) {
        self.minutes.buckets.clear();
        self.hours.buckets.clear();
        self.pending.clear();
        self.last.clear();
    }

    /// Pending (undrained) rollup rows.
    #[must_use]
    pub fn pending_rows(&self) -> usize {
        self.pending.len()
    }
}
