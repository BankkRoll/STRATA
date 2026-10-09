//! Structural self-check, used by the property tests and available to the
//! app as a diagnostic (e.g. after a cache load in debug builds).

use hashbrown::HashMap;
use strata_core::EntryFlags;

use crate::index::{DEAD, Index, NO_REF, NONE, ref_key};

impl Index {
    /// Verifies every internal invariant: live count, parent/child-list
    /// agreement, lookup coverage, detached/pending bookkeeping, and that
    /// every directory row equals a from-scratch recomputation. O(n).
    ///
    /// # Errors
    ///
    /// A description of the first violated invariant.
    pub fn check_invariants(&self) -> Result<(), String> {
        let n = self.col.len();
        let live = (0..n).filter(|&i| self.col.parent[i] != DEAD).count();
        if live != self.live as usize {
            return Err(format!("live count {} != {}", self.live, live));
        }
        if self.col.parent[self.root as usize] != NONE {
            return Err("root has a parent".into());
        }

        // Child lists ↔ parent pointers.
        let mut seen: HashMap<u32, u32> = HashMap::new();
        for d in 0..n as u32 {
            if self.col.parent[d as usize] == DEAD {
                continue;
            }
            let Some(r) = self.row_of(d) else { continue };
            let (a, b) = self.child_slices(r);
            for &c in a.iter().chain(b) {
                if self.col.parent[c as usize] != d {
                    return Err(format!(
                        "child {c} of {d} has parent {}",
                        self.col.parent[c as usize]
                    ));
                }
                *seen.entry(c).or_insert(0) += 1;
            }
        }
        for e in 0..n as u32 {
            let p = self.col.parent[e as usize];
            if p == DEAD || e == self.root {
                continue;
            }
            if p == NONE {
                return Err(format!("entry {e} is unplaced"));
            }
            if self.row_of(p).is_none() || self.col.parent[p as usize] == DEAD {
                return Err(format!("entry {e} has non-directory or dead parent {p}"));
            }
            if seen.get(&e) != Some(&1) {
                return Err(format!("entry {e} listed {:?} times", seen.get(&e)));
            }
        }

        // Lookup.
        for e in 0..n as u32 {
            let r = self.col.file_ref[e as usize];
            if self.col.parent[e as usize] == DEAD || r == NO_REF {
                continue;
            }
            let mut found = false;
            self.for_each_key(ref_key(r), |x| found |= x == e);
            if !found {
                return Err(format!("entry {e} not found by its key"));
            }
        }

        // Detached / pending.
        for (&e, &r) in &self.detached {
            if self.col.parent[e as usize] == DEAD {
                return Err(format!("dead entry {e} is detached"));
            }
            if r != NO_REF
                && !self
                    .pending
                    .get(&ref_key(r))
                    .is_some_and(|v| v.contains(&e))
            {
                return Err(format!("detached entry {e} missing from pending"));
            }
            let p = self.col.parent[e as usize];
            if p != self.orphans && p != self.metadata {
                return Err(format!("detached entry {e} displayed under {p}"));
            }
        }
        for (k, v) in &self.pending {
            for e in v {
                if self.detached.get(e).map(|&r| ref_key(r)) != Some(*k) {
                    return Err(format!("pending entry {e} not detached on key {k}"));
                }
            }
        }
        let flagged = EntryFlags::ORPHAN.0 | EntryFlags::CYCLE_BROKEN.0;
        for e in 0..n as u32 {
            if self.col.parent[e as usize] != DEAD
                && self.col.flags[e as usize] & flagged != 0
                && !self.detached.contains_key(&e)
            {
                return Err(format!("orphan/cycle entry {e} is not detached"));
            }
        }

        // Aggregates.
        for d in 0..n as u32 {
            if self.col.parent[d as usize] == DEAD || self.row_of(d).is_none() {
                continue;
            }
            let fresh = self.compute_row(d);
            let r = self.row_of(d).expect("checked") as usize;
            let stored = (
                self.rows.sub_l[r],
                self.rows.sub_a[r],
                self.rows.files[r],
                self.rows.dirs[r],
                self.rows.largest_a[r],
                self.rows.largest_l[r],
                self.col.has(d, EntryFlags::PARTIAL),
            );
            let want = (
                fresh.l,
                fresh.a,
                fresh.files,
                fresh.dirs,
                fresh.big_a.0,
                fresh.big_l.0,
                fresh.partial,
            );
            if stored != want {
                return Err(format!(
                    "row of {d}: stored {stored:?}, recomputed {want:?}"
                ));
            }
            if !self.col.lite
                && (self.rows.newest[r], self.rows.oldest[r]) != (fresh.newest, fresh.oldest)
            {
                return Err(format!("times of {d} differ from recomputation"));
            }
        }
        Ok(())
    }
}
