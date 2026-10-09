//! Compaction: rebuild a dense base layout from everything live.
//!
//! Live updates leave tombstones in the base region, append new entries to
//! the delta region and append renamed names to the buffer. Compaction drops
//! all of that, re-sorts entries (directories first, then files, by key),
//! rebuilds the CSR child lists and the name layout, and recomputes the
//! aggregates. Newest/oldest times are carried over, so lite mode (which has no
//! per-entry times to recompute them from) keeps them.

use hashbrown::HashMap;
use rayon::prelude::*;
use strata_core::EntryFlags;

use crate::index::{DEAD, EntryId, Index, NO_REF, NONE, Rows, ref_key};
use crate::names::NameStore;

/// Old-id → new-id mapping returned by [`Index::compact`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdRemap {
    map: Vec<u32>,
}

impl IdRemap {
    /// New id of `old`, or `None` if it was not live.
    #[must_use]
    pub fn get(&self, old: EntryId) -> Option<EntryId> {
        self.map
            .get(old.index())
            .copied()
            .filter(|&v| v != NONE)
            .map(EntryId)
    }
}

/// Row values carried across compaction.
#[derive(Debug, Clone, Copy)]
struct RowData {
    newest: u32,
    oldest: u32,
    own_partial: bool,
}

impl Index {
    /// Rebuilds a dense layout. Every [`EntryId`] changes; the returned map
    /// translates old ids. Aggregates, flags and placement are preserved.
    pub fn compact(&mut self) -> IdRemap {
        let n = self.col.len();
        let is_dir = |i: u32| self.col.has(i, EntryFlags::DIR);
        let mut base: Vec<u32> = (0..n as u32)
            .filter(|&i| self.live_u32(i) && self.col.file_ref[i as usize] != NO_REF)
            .collect();
        base.par_sort_unstable_by_key(|&i| {
            (
                !is_dir(i),
                ref_key(self.col.file_ref[i as usize]),
                self.col.has(i, EntryFlags::HARDLINK_SECONDARY),
                i,
            )
        });
        let virtuals: Vec<u32> = (0..n as u32)
            .filter(|&i| self.live_u32(i) && self.col.file_ref[i as usize] == NO_REF)
            .collect();
        let base_len = base.len() as u32;
        let base_dirs = base.partition_point(|&i| is_dir(i)) as u32;
        let order: Vec<u32> = base.iter().chain(&virtuals).copied().collect();
        let mut map = vec![NONE; n];
        for (new, &old) in order.iter().enumerate() {
            map[old as usize] = new as u32;
        }
        let remap = |v: u32| if v >= DEAD { v } else { map[v as usize] };

        let saved: Vec<Option<RowData>> = order
            .iter()
            .map(|&old| {
                self.row_of(old).map(|r| {
                    let i = r as usize;
                    RowData {
                        newest: self.rows.newest[i],
                        oldest: self.rows.oldest[i],
                        own_partial: self.rows.own_partial(r),
                    }
                })
            })
            .collect();

        let mut col = self.col.gather(&order);
        for p in &mut col.parent {
            *p = remap(*p);
        }
        let mut names = NameStore::from_base(base.iter().map(|&i| self.names.get(i)));
        for (k, &old) in virtuals.iter().enumerate() {
            names.set(base_len + k as u32, self.names.get(old));
        }
        let detached: HashMap<u32, u64> = self
            .detached
            .iter()
            .filter(|(id, _)| map.get(**id as usize).is_some_and(|&v| v != NONE))
            .map(|(&id, &r)| (map[id as usize], r))
            .collect();

        self.root = remap(self.root);
        self.orphans = remap(self.orphans);
        self.metadata = remap(self.metadata);
        self.col = col;
        self.names = names;
        self.detached = detached;
        self.base_len = base_len;
        self.base_dirs = base_dirs;
        self.delta_keys = HashMap::new();
        self.free = Vec::new();
        self.free_rows = Vec::new();
        self.rows = Rows::default();
        self.delta_rows = HashMap::new();
        for k in 0..virtuals.len() as u32 {
            let id = base_len + k;
            if self.col.has(id, EntryFlags::DIR) {
                let r = self.rows.push_empty();
                self.delta_rows.insert(id, r);
            }
        }
        self.layout_tree();
        for (new, data) in saved.iter().enumerate() {
            let Some(d) = data else { continue };
            let r = self
                .row_of(new as u32)
                .expect("saved rows belong to directories") as usize;
            self.rows.newest[r] = d.newest;
            self.rows.oldest[r] = d.oldest;
            self.rows.set_own_partial(r as u32, d.own_partial);
        }
        // Recompute rather than copy: largest-descendant ties break by id, and
        // ids just changed. Lite mode keeps the carried-over times.
        self.aggregate_all();
        self.rebuild_pending();
        self.path_cache.clear();
        self.shrink_to_fit();
        IdRemap { map }
    }
}
