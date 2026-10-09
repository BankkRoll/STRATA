//! Directory aggregates: the bottom-up build pass and the
//! incremental O(depth) propagation used by live updates.
//!
//! # Incremental strategy
//!
//! A change to one entry is described as the entry's [`Summary`] before and
//! after (the summary is what the entry contributes to its parent). Walking up
//! the ancestor chain, each directory row is patched:
//!
//! - **Sums** (logical, allocated, file and dir counts) take the exact delta.
//! - **Newest/oldest mtime, partial**: a new value that improves the extreme
//!   is applied directly. If the old value *was* the extreme and the new one
//!   is worse, the row is recomputed from its own data and its children's
//!   summaries (O(fanout); children are already up to date because the walk
//!   is bottom-up).
//! - **Largest descendant**: the row keeps the id. If the current largest is
//!   the changed subtree's previous largest and it shrank or left, the row is
//!   recomputed; otherwise the better of the current and the new candidate
//!   wins. Ties break toward the lower id so the result is unique.
//!
//! Walking stops at the first ancestor whose summary did not change. Only
//! entries on the ancestor chain are ever touched, and recomputation happens
//! only where an extreme was actually lost, so the common cases (file grows,
//! file is written, file is created) are pure O(depth).
//!
//! In lite mode there are no per-entry times, so recomputation keeps the
//! directory's existing newest/oldest values: time aggregates reflect the
//! last full build plus monotonic improvements.

use rayon::prelude::*;
use strata_core::{EntryFlags, SizeMode};

use crate::index::{Index, NO_NEWEST, NO_OLDEST, NONE};

/// What one entry contributes to its parent's row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) l: u64,
    pub(crate) a: u64,
    pub(crate) files: u32,
    pub(crate) dirs: u32,
    pub(crate) newest: u32,
    pub(crate) oldest: u32,
    /// Largest file by allocated contribution: `(id, bytes)`.
    pub(crate) big_a: (u32, u64),
    /// Largest file by logical contribution.
    pub(crate) big_l: (u32, u64),
    pub(crate) partial: bool,
}

impl Summary {
    pub(crate) const EMPTY: Self = Self {
        l: 0,
        a: 0,
        files: 0,
        dirs: 0,
        newest: NO_NEWEST,
        oldest: NO_OLDEST,
        big_a: (NONE, 0),
        big_l: (NONE, 0),
        partial: false,
    };

    #[inline]
    fn add(&mut self, s: &Self) {
        self.l = self.l.wrapping_add(s.l);
        self.a = self.a.wrapping_add(s.a);
        self.files = self.files.wrapping_add(s.files);
        self.dirs = self.dirs.wrapping_add(s.dirs);
        self.newest = self.newest.max(s.newest);
        self.oldest = self.oldest.min(s.oldest);
        if better(s.big_a, self.big_a) {
            self.big_a = s.big_a;
        }
        if better(s.big_l, self.big_l) {
            self.big_l = s.big_l;
        }
        self.partial |= s.partial;
    }
}

/// Whether candidate `c` beats `cur` for "largest": bigger, or equal size and
/// lower id. `NONE` never wins.
#[inline]
pub(crate) fn better(c: (u32, u64), cur: (u32, u64)) -> bool {
    c.0 != NONE && (cur.0 == NONE || c.1 > cur.1 || (c.1 == cur.1 && c.0 < cur.0))
}

/// An entry whose own contribution changed in the current operation, with its
/// contributions before the change (allocated, logical).
pub(crate) type Changed = (u32, u64, u64);

impl Index {
    /// Modification time usable for newest/oldest aggregation.
    #[inline]
    pub(crate) fn valid_mtime(&self, id: u32) -> Option<u32> {
        let m = self.col.mtime(id)?;
        let bad = EntryFlags::VIRTUAL.0 | EntryFlags::SUSPICIOUS_TIME.0;
        (m != 0 && self.col.flags[id as usize] & bad == 0).then_some(m)
    }

    /// What entry `id` contributes to its parent.
    pub(crate) fn summary(&self, id: u32) -> Summary {
        let f = self.col.flags(id);
        let partial = f.contains(EntryFlags::PARTIAL) || f.contains(EntryFlags::ACCESS_DENIED);
        let virt = f.contains(EntryFlags::VIRTUAL);
        if let Some(r) = self.row_of(id) {
            let r = r as usize;
            let la = self.rows.largest_a[r];
            let ll = self.rows.largest_l[r];
            Summary {
                l: self.rows.sub_l[r],
                a: self.rows.sub_a[r],
                files: self.rows.files[r],
                dirs: self.rows.dirs[r] + u32::from(!virt),
                newest: self.rows.newest[r],
                oldest: self.rows.oldest[r],
                big_a: (la, self.size_or_zero(la, SizeMode::Allocated)),
                big_l: (ll, self.size_or_zero(ll, SizeMode::Logical)),
                // The effective flag already folds in the directory's own
                // ACCESS_DENIED (via `own_partial`); reading ACCESS_DENIED here
                // would see a new value before the row has absorbed it.
                partial: f.contains(EntryFlags::PARTIAL),
            }
        } else {
            let a = self.contrib(id, SizeMode::Allocated);
            let l = self.contrib(id, SizeMode::Logical);
            let m = self.valid_mtime(id);
            Summary {
                l,
                a,
                files: u32::from(!virt),
                dirs: 0,
                newest: m.unwrap_or(NO_NEWEST),
                oldest: m.unwrap_or(NO_OLDEST),
                big_a: if virt { (NONE, 0) } else { (id, a) },
                big_l: if virt { (NONE, 0) } else { (id, l) },
                partial,
            }
        }
    }

    #[inline]
    fn size_or_zero(&self, id: u32, mode: SizeMode) -> u64 {
        if id == NONE {
            0
        } else {
            self.contrib(id, mode)
        }
    }

    /// A directory's own contribution to its row (not its children).
    pub(crate) fn own_summary(&self, id: u32) -> Summary {
        let r = self.row_of(id).expect("own_summary on a directory");
        let m = self.valid_mtime(id);
        Summary {
            l: self.contrib(id, SizeMode::Logical),
            a: self.contrib(id, SizeMode::Allocated),
            newest: m.unwrap_or(NO_NEWEST),
            oldest: m.unwrap_or(NO_OLDEST),
            partial: self.rows.own_partial(r),
            ..Summary::EMPTY
        }
    }

    /// Row of `id` computed from scratch: own data plus children summaries.
    pub(crate) fn compute_row(&self, id: u32) -> Summary {
        let mut acc = self.own_summary(id);
        let r = self.row_of(id).expect("compute_row on a directory");
        let (a, b) = self.child_slices(r);
        for &c in a.iter().chain(b) {
            acc.add(&self.summary(c));
        }
        acc
    }

    /// Stores a computed row. `keep_times` preserves the stored newest/oldest
    /// (lite mode after the build).
    pub(crate) fn write_row(&mut self, id: u32, s: &Summary, keep_times: bool) {
        let r = self.row_of(id).expect("write_row on a directory") as usize;
        self.rows.sub_l[r] = s.l;
        self.rows.sub_a[r] = s.a;
        self.rows.files[r] = s.files;
        self.rows.dirs[r] = s.dirs;
        if !keep_times {
            self.rows.newest[r] = s.newest;
            self.rows.oldest[r] = s.oldest;
        }
        self.rows.largest_a[r] = s.big_a.0;
        self.rows.largest_l[r] = s.big_l.0;
        self.col.set_flag(id, EntryFlags::PARTIAL, s.partial);
    }

    pub(crate) fn recompute_row(&mut self, id: u32) {
        let s = self.compute_row(id);
        self.write_row(id, &s, self.col.lite);
    }

    /// Computes every directory row bottom-up, one tree level at a time, each
    /// level in parallel.
    pub(crate) fn aggregate_all(&mut self) {
        let mut levels: Vec<Vec<u32>> = vec![vec![self.root]];
        loop {
            let next: Vec<u32> = {
                let last = levels.last().expect("at least the root level");
                last.par_iter()
                    .flat_map_iter(|&d| {
                        let r = self.row_of(d).expect("level entries are directories");
                        let (a, b) = self.child_slices(r);
                        a.iter()
                            .chain(b)
                            .copied()
                            .filter(|&c| self.row_of(c).is_some())
                            .collect::<Vec<_>>()
                    })
                    .collect()
            };
            if next.is_empty() {
                break;
            }
            levels.push(next);
        }
        for level in levels.iter().rev() {
            let rows: Vec<Summary> = level.par_iter().map(|&d| self.compute_row(d)).collect();
            for (&d, s) in level.iter().zip(&rows) {
                self.write_row(d, s, self.col.lite);
            }
        }
    }

    /// Summary of directory `id` with largest-descendant sizes as they were
    /// before the entries in `changed` changed.
    fn summary_before(&self, id: u32, changed: &[Changed]) -> Summary {
        let mut s = self.summary(id);
        for &(c, a, l) in changed {
            if s.big_a.0 == c {
                s.big_a.1 = a;
            }
            if s.big_l.0 == c {
                s.big_l.1 = l;
            }
        }
        s
    }

    /// Applies "a child's summary went from `old` to `new`" to the row of
    /// `id`. Returns whether the row must be recomputed from its children.
    fn apply_at(&mut self, id: u32, old: &Summary, new: &Summary, changed: &[Changed]) -> bool {
        let r = self.row_of(id).expect("apply_at on a directory") as usize;
        let rows = &mut self.rows;
        rows.sub_l[r] = rows.sub_l[r].wrapping_sub(old.l).wrapping_add(new.l);
        rows.sub_a[r] = rows.sub_a[r].wrapping_sub(old.a).wrapping_add(new.a);
        rows.files[r] = rows.files[r]
            .wrapping_sub(old.files)
            .wrapping_add(new.files);
        rows.dirs[r] = rows.dirs[r].wrapping_sub(old.dirs).wrapping_add(new.dirs);

        let mut recompute = false;
        if !self.col.lite {
            if old.newest != NO_NEWEST && old.newest >= rows.newest[r] && new.newest < old.newest {
                recompute = true;
            } else {
                rows.newest[r] = rows.newest[r].max(new.newest);
            }
            if old.oldest != NO_OLDEST && old.oldest <= rows.oldest[r] && new.oldest > old.oldest {
                recompute = true;
            } else {
                rows.oldest[r] = rows.oldest[r].min(new.oldest);
            }
        } else {
            rows.newest[r] = rows.newest[r].max(new.newest);
            rows.oldest[r] = rows.oldest[r].min(new.oldest);
        }

        let partial_now = self.col.has(id, EntryFlags::PARTIAL);
        if old.partial && !new.partial && partial_now {
            recompute = true;
        } else if new.partial {
            self.col.set_flag(id, EntryFlags::PARTIAL, true);
        }

        for mode in [SizeMode::Allocated, SizeMode::Logical] {
            let (cur, o, n) = match mode {
                SizeMode::Allocated => (self.rows.largest_a[r], old.big_a, new.big_a),
                SizeMode::Logical => (self.rows.largest_l[r], old.big_l, new.big_l),
            };
            let before = |x: u32| {
                changed
                    .iter()
                    .find(|c| c.0 == x)
                    .map(|c| match mode {
                        SizeMode::Allocated => c.1,
                        SizeMode::Logical => c.2,
                    })
                    .unwrap_or_else(|| self.contrib(x, mode))
            };
            let set = if cur != NONE && cur == o.0 {
                let cur_old = (cur, before(cur));
                if n.0 == cur {
                    if self.contrib(cur, mode) < cur_old.1 {
                        recompute = true;
                    }
                    None
                } else if better(n, cur_old) {
                    Some(n.0)
                } else {
                    recompute = true;
                    None
                }
            } else {
                let cur_now = (cur, self.size_or_zero(cur, mode));
                better(n, cur_now).then_some(n.0)
            };
            if let Some(v) = set {
                match mode {
                    SizeMode::Allocated => self.rows.largest_a[r] = v,
                    SizeMode::Logical => self.rows.largest_l[r] = v,
                }
            }
        }
        recompute
    }

    /// Propagates "the summary of a child of `dir` (or of `dir`'s own data)
    /// went from `old` to `new`" up the ancestor chain. `touched` collects
    /// every directory whose row changed.
    pub(crate) fn propagate(
        &mut self,
        dir: u32,
        mut old: Summary,
        mut new: Summary,
        changed: &[Changed],
        touched: &mut Vec<u32>,
    ) {
        let mut d = dir;
        loop {
            if self.row_of(d).is_none() || old == new {
                return;
            }
            let before = self.summary_before(d, changed);
            if self.apply_at(d, &old, &new, changed) {
                self.recompute_row(d);
            }
            touched.push(d);
            let after = self.summary(d);
            let p = self.col.parent[d as usize];
            if before == after || p == NONE || p >= crate::index::DEAD {
                return;
            }
            old = before;
            new = after;
            d = p;
        }
    }
}
