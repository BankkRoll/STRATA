//! Incremental live updates, driven by the USN journal tailer.
//!
//! [`Index::upsert`] creates or updates a record (sizes, attributes, times,
//! renames, moves, hardlink changes); [`Index::remove`] deletes one. Each
//! returns a [`ChangeSet`]: the entries created, updated and removed, plus the
//! new aggregate of every directory whose totals changed. Batches go through
//! [`Index::apply`] and produce one merged change set.
//!
//! # Placement
//!
//! Where an entry is displayed is a pure function of the current record set
//! (the same rules as the build, see [`crate::build`]): under its parent if
//! the parent reference is valid, under "Orphaned entries" if not or if the
//! entry sits on a parent-reference cycle, or under "NTFS metadata" for
//! grouped metadata files. Entries displayed away from their real parent are
//! *detached*: their intended parent reference is kept in a side map and
//! indexed by key in `pending`, so creating, removing or changing the record
//! they wait on re-evaluates exactly them (and cascades from there). A move
//! that would close a loop is detected by walking the intended-parent chain
//! up from the new parent (O(depth)); every member of the loop is parked
//! with [`EntryFlags::CYCLE_BROKEN`], as a fresh build would do.
//!
//! # Cost
//!
//! A size/attribute/time change is one O(depth) propagation (see
//! [`crate::agg`]). A rename within the same directory touches no list. A
//! move unlinks from the old parent's child list (a contiguous scan of that
//! list) and propagates up both ancestor chains.
//!
//! # Id stability
//!
//! Ids never shift. Removed base slots are tombstoned (they hold the sorted
//! `file_ref` order the lookup binary-searches); removed delta slots go on a
//! free list and are reused by later creates. [`Index::compact`] rebuilds a
//! dense layout and returns the id remapping; call it when
//! [`Index::tombstones`] grows large (e.g. above a quarter of the slots).

use std::collections::VecDeque;

use hashbrown::HashSet;
use serde::{Deserialize, Serialize};
use strata_core::{EntryFlags, FileRef, NameLink, ScanRecord};

use crate::IndexError;
use crate::agg::Summary;
use crate::build::{link_name, new_entry, own_partial, record_flags};
use crate::index::{DEAD, DirAggregate, EntryId, Index, NO_REF, NONE, NewEntry, ref_key};

/// One live update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Update {
    /// Create or replace a record (all its links).
    Upsert(ScanRecord),
    /// Delete a record by exact reference. A stale reference (sequence
    /// mismatch) is ignored.
    Remove(FileRef),
}

/// What a batch of live updates changed. Ids are sorted and unique.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSet {
    /// New entries.
    pub created: Vec<EntryId>,
    /// Existing entries whose name, parent, flags, sizes or times changed.
    pub updated: Vec<EntryId>,
    /// Entries that no longer exist.
    pub removed: Vec<EntryId>,
    /// Every directory whose aggregate changed, with its new aggregate.
    pub aggregates: Vec<(EntryId, DirAggregate)>,
}

impl ChangeSet {
    /// Whether nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.created.is_empty()
            && self.updated.is_empty()
            && self.removed.is_empty()
            && self.aggregates.is_empty()
    }
}

/// Where an entry belongs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Placement {
    Root,
    Under(u32),
    Orphan,
    Meta,
    /// On a cycle; the other members found on the walk.
    Cycle(Vec<u32>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Park {
    Orphan,
    Cycle,
    Meta,
}

/// Accumulated effects of one `apply`.
#[derive(Debug, Default)]
pub(crate) struct Cx {
    created: Vec<u32>,
    updated: Vec<u32>,
    removed: Vec<u32>,
    pub(crate) touched: Vec<u32>,
    queue: VecDeque<u32>,
}

impl Index {
    // -------------------------------------------------------------------------
    // Public API
    // -------------------------------------------------------------------------

    /// Creates or updates one record.
    ///
    /// # Errors
    ///
    /// [`IndexError::TooManyEntries`] when the new links would not fit,
    /// [`IndexError::ReservedFileRef`] for `u64::MAX`, and
    /// [`IndexError::RootRemoval`] if the update would replace the root
    /// record with a different one. The index is unchanged on error.
    ///
    /// # Example
    ///
    /// ```
    /// use strata_core::*;
    /// use strata_index::{IndexBuilder, IndexOptions};
    ///
    /// let root = FileRef::from_parts(5, 5);
    /// let rec = |id, parent, name: &str, size: u64, dir: bool| ScanRecord {
    ///     id,
    ///     links: vec![NameLink { parent, name: WideName::from_str_lossless(name) }],
    ///     attributes: 0,
    ///     flags: if dir { EntryFlags::DIR } else { EntryFlags::EMPTY },
    ///     times: Times::default(),
    ///     fn_created: None,
    ///     sizes: Sizes { logical: size, allocated: size, ..Sizes::default() },
    ///     reparse: None,
    ///     ads: vec![],
    /// };
    /// let mut b = IndexBuilder::new(IndexOptions::default());
    /// b.push(rec(root, root, "", 0, true))?;
    /// let mut index = b.finish()?;
    ///
    /// let file = FileRef::from_parts(64, 1);
    /// let cs = index.upsert(rec(file, root, "new.log", 100, false))?;
    /// assert_eq!(cs.created.len(), 1);
    /// assert_eq!(index.aggregate(index.root()).unwrap().allocated, 100);
    ///
    /// let cs = index.upsert(rec(file, root, "new.log", 300, false))?;
    /// let (dir, agg) = cs.aggregates[0];
    /// assert_eq!((dir, agg.allocated), (index.root(), 300));
    /// # Ok::<(), strata_index::IndexError>(())
    /// ```
    pub fn upsert(&mut self, rec: ScanRecord) -> Result<ChangeSet, IndexError> {
        self.apply([Update::Upsert(rec)])
    }

    /// Removes a record (every link) by exact reference.
    ///
    /// # Errors
    ///
    /// [`IndexError::RootRemoval`] for the volume root.
    pub fn remove(&mut self, file_ref: FileRef) -> Result<ChangeSet, IndexError> {
        self.apply([Update::Remove(file_ref)])
    }

    /// Applies a batch of updates in order and returns the merged change set.
    /// On error, updates before the failing one stay applied.
    ///
    /// # Errors
    ///
    /// The first error from [`Index::upsert`] or [`Index::remove`].
    pub fn apply(
        &mut self,
        updates: impl IntoIterator<Item = Update>,
    ) -> Result<ChangeSet, IndexError> {
        let mut cx = Cx::default();
        let mut result = Ok(());
        for u in updates {
            let r = match u {
                Update::Upsert(rec) => self.upsert_inner(rec, &mut cx),
                Update::Remove(r) => self.remove_inner(r, &mut cx),
            };
            self.drain(&mut cx);
            if let Err(e) = r {
                result = Err(e);
                break;
            }
        }
        let cs = self.finish_changes(cx);
        result.map(|()| cs)
    }

    /// Adds a virtual leaf block under the root (e.g. "Unaccounted / system
    /// reserved", "System Restore / Shadow copies").
    pub fn add_virtual_block(
        &mut self,
        name: &str,
        logical: u64,
        allocated: u64,
    ) -> (EntryId, ChangeSet) {
        let mut cx = Cx::default();
        let id = self.push_virtual(NONE, name.as_bytes(), false, logical, allocated);
        self.link_child(self.root, id);
        self.col.parent[id as usize] = self.root;
        let s = self.summary(id);
        self.propagate(self.root, Summary::EMPTY, s, &[], &mut cx.touched);
        cx.created.push(id);
        (EntryId(id), self.finish_changes(cx))
    }

    /// Changes the sizes of a virtual block.
    ///
    /// # Errors
    ///
    /// [`IndexError::NotVirtualBlock`] if `id` is not a live virtual leaf.
    pub fn set_virtual_block(
        &mut self,
        id: EntryId,
        logical: u64,
        allocated: u64,
    ) -> Result<ChangeSet, IndexError> {
        self.check_block(id)?;
        let mut cx = Cx::default();
        let old = self.summary(id.0);
        self.col.set_sizes(id.0, logical, allocated);
        let new = self.summary(id.0);
        let p = self.col.parent[id.index()];
        self.propagate(p, old, new, &[], &mut cx.touched);
        cx.updated.push(id.0);
        Ok(self.finish_changes(cx))
    }

    /// Removes a virtual block.
    ///
    /// # Errors
    ///
    /// [`IndexError::NotVirtualBlock`] if `id` is not a live virtual leaf.
    pub fn remove_virtual_block(&mut self, id: EntryId) -> Result<ChangeSet, IndexError> {
        self.check_block(id)?;
        let mut cx = Cx::default();
        self.unplace(id.0, &mut cx);
        self.free_entry(id.0, &mut cx);
        Ok(self.finish_changes(cx))
    }

    fn check_block(&self, id: EntryId) -> Result<(), IndexError> {
        let ok = self.is_live(id)
            && self.col.has(id.0, EntryFlags::VIRTUAL)
            && !self.col.has(id.0, EntryFlags::DIR);
        if ok {
            Ok(())
        } else {
            Err(IndexError::NotVirtualBlock(id.0))
        }
    }

    /// Removed base slots awaiting [`Index::compact`].
    #[must_use]
    pub fn tombstones(&self) -> usize {
        (0..self.base_len as usize)
            .filter(|&i| self.col.parent[i] == DEAD)
            .count()
    }

    // -------------------------------------------------------------------------
    // Upsert / remove
    // -------------------------------------------------------------------------

    fn upsert_inner(&mut self, rec: ScanRecord, cx: &mut Cx) -> Result<(), IndexError> {
        if rec.id.0 == NO_REF {
            return Err(IndexError::ReservedFileRef(rec.id.0));
        }
        let key = ref_key(rec.id.0);
        let existing = self.links_of_key(key);
        // Checked before any change, as an upper bound for every path below
        // (creates, renames, re-links), so a full buffer leaves the index
        // untouched. Updates that keep every name write no name bytes.
        let same_names = existing.len() == rec.links.len().max(1)
            && existing.iter().enumerate().all(|(rank, &e)| {
                self.names.get(e) == link_name(&rec, rec.links.get(rank)).as_slice()
            });
        if !same_names {
            let bytes = self.names.buf.len() as u64 + crate::index::name_bytes_needed(&rec);
            if bytes > self.limits.name_bytes {
                return Err(IndexError::NameStoreFull(bytes));
            }
        }
        if existing.is_empty() {
            return self.create(&rec, cx);
        }
        let first = existing[0];
        let same_record = self.col.file_ref[first as usize] == rec.id.0
            && self.col.has(first, EntryFlags::DIR) == rec.is_dir();
        if existing.contains(&self.root) {
            if !same_record {
                return Err(IndexError::RootRemoval);
            }
            self.update_root(&rec, cx);
            return Ok(());
        }
        if !same_record {
            self.check_capacity(rec.links.len().max(1))?;
            for e in existing {
                self.remove_entry(e, cx);
            }
            self.link_counts.remove(&key);
            return self.create(&rec, cx);
        }
        self.update(&existing, &rec, cx)
    }

    fn remove_inner(&mut self, r: FileRef, cx: &mut Cx) -> Result<(), IndexError> {
        let key = ref_key(r.0);
        let existing: Vec<u32> = self
            .links_of_key(key)
            .into_iter()
            .filter(|&e| self.col.file_ref[e as usize] == r.0)
            .collect();
        if existing.contains(&self.root) {
            return Err(IndexError::RootRemoval);
        }
        for e in existing {
            self.remove_entry(e, cx);
        }
        self.link_counts.remove(&key);
        Ok(())
    }

    fn check_capacity(&self, n: usize) -> Result<(), IndexError> {
        let fresh = n.saturating_sub(self.free.len()) as u64;
        let total = self.col.len() as u64 + fresh;
        if total > self.limits.entries {
            return Err(IndexError::TooManyEntries(total));
        }
        Ok(())
    }

    fn create(&mut self, rec: &ScanRecord, cx: &mut Cx) -> Result<(), IndexError> {
        let links: Vec<Option<&NameLink>> = if rec.links.is_empty() {
            vec![None]
        } else {
            rec.links.iter().map(Some).collect()
        };
        self.check_capacity(links.len())?;
        let key = ref_key(rec.id.0);
        let flags = record_flags(rec, self.opts.now);
        let is_dir = rec.is_dir();
        if links.len() > 1 {
            self.link_counts.insert(key, links.len() as u32);
        }
        for (rank, link) in links.into_iter().enumerate() {
            let name = link_name(rec, link);
            let ext = if is_dir {
                0
            } else {
                self.exts.intern_name(&name)
            };
            let mut f = flags;
            f.set(EntryFlags::HARDLINK_SECONDARY, rank > 0);
            let id = self.alloc_slot(&new_entry(rec, f, ext));
            self.names.set(id, &name);
            self.delta_keys.entry(key).or_default().push(id);
            if is_dir {
                let r = self.alloc_row();
                self.delta_rows.insert(id, r);
                self.rows.set_own_partial(r, own_partial(flags));
                self.col.set_flag(id, EntryFlags::PARTIAL, false);
                let s = self.compute_row(id);
                self.write_row(id, &s, false);
            }
            self.set_detached(id, link.map_or(NO_REF, |l| l.parent.0));
            self.live += 1;
            cx.created.push(id);
            cx.queue.push_back(id);
        }
        self.queue_pending(key, cx);
        Ok(())
    }

    /// The root record changed: only its own data can change (it never moves).
    fn update_root(&mut self, rec: &ScanRecord, cx: &mut Cx) {
        let e = self.root;
        let old = self.own_summary(e);
        self.write_record_fields(e, rec, 0);
        let new = self.own_summary(e);
        self.propagate(e, old, new, &[], &mut cx.touched);
        cx.updated.push(e);
    }

    fn update(
        &mut self,
        existing: &[u32],
        rec: &ScanRecord,
        cx: &mut Cx,
    ) -> Result<(), IndexError> {
        let key = ref_key(rec.id.0);
        let new_links: Vec<(u64, Vec<u8>)> = if rec.links.is_empty() {
            vec![(NO_REF, link_name(rec, None))]
        } else {
            rec.links
                .iter()
                .map(|l| (l.parent.0, link_name(rec, Some(l))))
                .collect()
        };
        let old_links: Vec<(u32, u64, Vec<u8>)> = existing
            .iter()
            .map(|&e| (e, self.intended(e), self.names.get(e).to_vec()))
            .collect();

        let unchanged = old_links.len() == new_links.len()
            && old_links
                .iter()
                .zip(&new_links)
                .all(|(o, n)| o.1 == n.0 && o.2 == n.1);
        if unchanged {
            for (rank, &(e, ..)) in old_links.iter().enumerate() {
                self.update_in_place(e, rec, rank, cx);
            }
            return Ok(());
        }

        let extra = new_links.len().saturating_sub(old_links.len());
        self.check_capacity(extra)?;

        // Match new links to existing entries: exact (parent, name) first,
        // then leftovers pairwise in order (renames and moves keep their id).
        let mut assign: Vec<Option<u32>> = vec![None; new_links.len()];
        let mut used = vec![false; old_links.len()];
        for (j, (p, n)) in new_links.iter().enumerate() {
            if let Some(i) = old_links
                .iter()
                .enumerate()
                .position(|(i, o)| !used[i] && o.1 == *p && o.2 == *n)
            {
                used[i] = true;
                assign[j] = Some(old_links[i].0);
            }
        }
        for slot in assign.iter_mut().filter(|s| s.is_none()) {
            if let Some(i) = used.iter().position(|u| !u) {
                used[i] = true;
                *slot = Some(old_links[i].0);
            }
        }

        for &(e, ..) in &old_links {
            self.unplace(e, cx);
        }
        for (i, &(e, ..)) in old_links.iter().enumerate() {
            if !used[i] {
                self.free_or_orphan_children(e, cx);
                self.free_entry(e, cx);
            }
        }
        if new_links.len() > 1 {
            self.link_counts.insert(key, new_links.len() as u32);
        } else {
            self.link_counts.remove(&key);
        }

        let is_dir = rec.is_dir();
        for (rank, ((parent, name), slot)) in new_links.iter().zip(&assign).enumerate() {
            match *slot {
                Some(e) => {
                    let was_parent = self.can_parent(e);
                    let was_meta = self.col.has(e, EntryFlags::NTFS_METADATA);
                    if self.names.get(e) != name.as_slice() {
                        self.names.set(e, name);
                        if !is_dir {
                            self.col.ext_id[e as usize] = self.exts.intern_name(name);
                        }
                    }
                    if is_dir {
                        let old = self.own_summary(e);
                        self.write_record_fields(e, rec, rank);
                        let new = self.own_summary(e);
                        self.apply_own_change(e, old, new, cx);
                    } else {
                        self.write_record_fields(e, rec, rank);
                    }
                    self.after_flag_change(e, was_parent, was_meta, cx);
                    self.set_detached(e, *parent);
                    cx.updated.push(e);
                    cx.queue.push_back(e);
                }
                None => {
                    let flags = record_flags(rec, self.opts.now);
                    let ext = if is_dir {
                        0
                    } else {
                        self.exts.intern_name(name)
                    };
                    let mut f = flags;
                    f.set(EntryFlags::HARDLINK_SECONDARY, rank > 0);
                    let id = self.alloc_slot(&new_entry(rec, f, ext));
                    self.names.set(id, name);
                    self.delta_keys.entry(key).or_default().push(id);
                    if is_dir {
                        let r = self.alloc_row();
                        self.delta_rows.insert(id, r);
                        self.rows.set_own_partial(r, own_partial(flags));
                        self.col.set_flag(id, EntryFlags::PARTIAL, false);
                        let s = self.compute_row(id);
                        self.write_row(id, &s, false);
                    }
                    self.set_detached(id, *parent);
                    self.live += 1;
                    cx.created.push(id);
                    cx.queue.push_back(id);
                }
            }
        }
        self.queue_pending(key, cx);
        Ok(())
    }

    /// Size/attribute/time change of one link with unchanged name and parent.
    fn update_in_place(&mut self, e: u32, rec: &ScanRecord, rank: usize, cx: &mut Cx) {
        let was_parent = self.can_parent(e);
        let was_meta = self.col.has(e, EntryFlags::NTFS_METADATA);
        if self.row_of(e).is_some() {
            let old = self.own_summary(e);
            self.write_record_fields(e, rec, rank);
            let new = self.own_summary(e);
            self.apply_own_change(e, old, new, cx);
        } else {
            let old = self.summary(e);
            let changed = [(
                e,
                self.contrib(e, strata_core::SizeMode::Allocated),
                self.contrib(e, strata_core::SizeMode::Logical),
            )];
            self.write_record_fields(e, rec, rank);
            let new = self.summary(e);
            let p = self.col.parent[e as usize];
            if p != NONE {
                self.propagate(p, old, new, &changed, &mut cx.touched);
            }
        }
        self.after_flag_change(e, was_parent, was_meta, cx);
        cx.updated.push(e);
    }

    /// Applies a directory's own-data change to its row and ancestors.
    fn apply_own_change(&mut self, e: u32, old: Summary, new: Summary, cx: &mut Cx) {
        if self.col.parent[e as usize] == NONE && e != self.root {
            // Unplaced: no ancestors yet, just keep the row consistent.
            if old != new {
                self.recompute_row(e);
            }
        } else {
            self.propagate(e, old, new, &[], &mut cx.touched);
        }
    }

    /// Re-evaluates dependants when a directory's ability to hold children or
    /// its metadata status changed.
    fn after_flag_change(&mut self, e: u32, was_parent: bool, was_meta: bool, cx: &mut Cx) {
        let is_parent = self.can_parent(e);
        let is_meta = self.col.has(e, EntryFlags::NTFS_METADATA);
        if was_meta != is_meta {
            cx.queue.push_back(e);
        }
        if was_parent == is_parent && was_meta == is_meta {
            return;
        }
        if let Some(r) = self.row_of(e) {
            let (a, b) = self.child_slices(r);
            let kids: Vec<u32> = a.iter().chain(b).copied().collect();
            let fr = self.col.file_ref[e as usize];
            for c in kids {
                if !self.detached.contains_key(&c) {
                    self.set_detached(c, fr);
                }
                cx.queue.push_back(c);
            }
        }
        let fr = self.col.file_ref[e as usize];
        if fr != NO_REF {
            self.queue_pending(ref_key(fr), cx);
        }
    }

    /// Overwrites the record-derived columns of `e`, keeping index-owned flag
    /// bits (orphan/cycle placement, effective directory PARTIAL).
    fn write_record_fields(&mut self, e: u32, rec: &ScanRecord, rank: usize) {
        let mut f = record_flags(rec, self.opts.now);
        f.set(EntryFlags::HARDLINK_SECONDARY, rank > 0);
        let cur = self.col.flags(e);
        let keep = EntryFlags::ORPHAN.0 | EntryFlags::CYCLE_BROKEN.0;
        let mut v = (f.0 & !keep) | (cur.0 & keep);
        if let Some(r) = self.row_of(e) {
            self.rows.set_own_partial(r, own_partial(f));
            v = (v & !EntryFlags::PARTIAL.0) | (cur.0 & EntryFlags::PARTIAL.0);
        }
        self.col.flags[e as usize] = v;
        self.col
            .set_sizes(e, rec.sizes.total_logical(), rec.sizes.total_allocated());
        self.col.set_times(e, crate::build::record_times(rec));
    }

    // -------------------------------------------------------------------------
    // Removal
    // -------------------------------------------------------------------------

    /// Removes one entry: detaches it, re-places its children (they become
    /// orphans), frees the slot.
    fn remove_entry(&mut self, e: u32, cx: &mut Cx) {
        self.unplace(e, cx);
        self.free_or_orphan_children(e, cx);
        self.free_entry(e, cx);
    }

    /// Makes `e` unresolvable, then re-places its children.
    fn free_or_orphan_children(&mut self, e: u32, cx: &mut Cx) {
        self.unregister_key(e);
        let Some(r) = self.row_of(e) else { return };
        let (a, b) = self.child_slices(r);
        let kids: Vec<u32> = a.iter().chain(b).copied().collect();
        if kids.is_empty() {
            return;
        }
        let fr = self.col.file_ref[e as usize];
        // Children must leave before the row disappears. `e` is unplaced, so
        // their departure propagates no further than its row, and it no
        // longer resolves by key, so they cannot re-attach to it.
        for c in kids {
            if !self.detached.contains_key(&c) {
                self.set_detached(c, fr);
            }
            self.place(c, cx);
        }
    }

    /// Removes `e` from the key lookup so it no longer resolves.
    fn unregister_key(&mut self, e: u32) {
        let fr = self.col.file_ref[e as usize];
        if fr == NO_REF {
            return;
        }
        let key = ref_key(fr);
        if e >= self.base_len
            && let Some(v) = self.delta_keys.get_mut(&key)
        {
            v.retain(|&x| x != e);
            if v.is_empty() {
                self.delta_keys.remove(&key);
            }
        }
        // Base entries leave the lookup by being marked dead in `free_entry`;
        // mark the key unresolvable now by flagging the slot dead early.
        if e < self.base_len {
            self.col.parent[e as usize] = DEAD;
        }
    }

    /// Releases an unplaced, childless entry's slot.
    fn free_entry(&mut self, e: u32, cx: &mut Cx) {
        let fr = self.col.file_ref[e as usize];
        if self.col.parent[e as usize] != DEAD {
            self.unregister_key(e);
        }
        self.clear_detached(e);
        if let Some(r) = self.row_of(e) {
            self.extra.remove(&r);
            if e >= self.base_dirs {
                self.delta_rows.remove(&e);
                self.rows.reset(r);
                self.free_rows.push(r);
            } else {
                let i = r as usize;
                self.rows.len[i] = 0;
            }
        }
        self.names.remove(e);
        self.col.parent[e as usize] = DEAD;
        self.col.flags[e as usize] = 0;
        if e >= self.base_len {
            self.col.file_ref[e as usize] = NO_REF;
            self.free.push(e);
        }
        self.live -= 1;
        if cx.created.contains(&e) {
            cx.created.retain(|&x| x != e);
        } else {
            cx.removed.push(e);
        }
        if fr != NO_REF {
            self.queue_pending(ref_key(fr), cx);
        }
    }

    // -------------------------------------------------------------------------
    // Placement
    // -------------------------------------------------------------------------

    fn drain(&mut self, cx: &mut Cx) {
        // Each evaluation yields the canonical placement, so re-evaluating
        // converges; the bound only guards against a logic error looping.
        let mut budget = 16 * (self.live as usize + cx.queue.len()) + 1024;
        while let Some(e) = cx.queue.pop_front() {
            self.place(e, cx);
            budget -= 1;
            if budget == 0 {
                debug_assert!(false, "placement did not converge");
                break;
            }
        }
    }

    /// The intended (on-disk) parent reference of `e`.
    pub(crate) fn intended(&self, e: u32) -> u64 {
        if let Some(&r) = self.detached.get(&e) {
            return r;
        }
        if e == self.root {
            return self.col.file_ref[e as usize];
        }
        let p = self.col.parent[e as usize];
        if p >= DEAD {
            return NO_REF;
        }
        self.col.file_ref[p as usize]
    }

    fn evaluate(&self, e: u32) -> Placement {
        if e == self.root {
            return Placement::Root;
        }
        let Some(q) = self.valid_parent(self.intended(e)) else {
            return Placement::Orphan;
        };
        let mut path = Vec::new();
        let mut seen: HashSet<u32> = HashSet::new();
        let mut cur = q;
        loop {
            if cur == e {
                return Placement::Cycle(path);
            }
            if cur == self.root || path.len() > self.col.len() {
                break;
            }
            path.push(cur);
            let next = match self.detached.get(&cur) {
                Some(&ip) => {
                    if !seen.insert(cur) {
                        break;
                    }
                    match self.valid_parent(ip) {
                        Some(n) => n,
                        None => break,
                    }
                }
                None => self.col.parent[cur as usize],
            };
            if next >= DEAD {
                break;
            }
            cur = next;
        }
        let meta = EntryFlags::NTFS_METADATA;
        if self.col.has(e, meta) && !(self.col.has(q, meta) && q != self.root) {
            return Placement::Meta;
        }
        Placement::Under(q)
    }

    fn place(&mut self, e: u32, cx: &mut Cx) {
        if !self.live_u32(e) || e == self.root {
            return;
        }
        match self.evaluate(e) {
            Placement::Root => {}
            Placement::Under(q) => self.attach_under(e, q, cx),
            Placement::Orphan => self.park(e, Park::Orphan, cx),
            Placement::Meta => self.park(e, Park::Meta, cx),
            Placement::Cycle(members) => {
                // Record every member's intended parent before any of them
                // moves, then park them all.
                let mut all = members;
                all.push(e);
                let intended: Vec<(u32, u64)> =
                    all.iter().map(|&m| (m, self.intended(m))).collect();
                for &(m, r) in &intended {
                    self.set_detached(m, r);
                }
                for &(m, _) in &intended {
                    self.park(m, Park::Cycle, cx);
                }
            }
        }
    }

    fn park(&mut self, m: u32, kind: Park, cx: &mut Cx) {
        let intended = self.intended(m);
        let group = if kind == Park::Meta {
            self.metadata
        } else {
            self.orphans
        };
        let want = match kind {
            Park::Orphan => EntryFlags::ORPHAN.0,
            Park::Cycle => EntryFlags::CYCLE_BROKEN.0,
            Park::Meta => 0,
        };
        let mask = EntryFlags::ORPHAN.0 | EntryFlags::CYCLE_BROKEN.0;
        let have = self.col.flags[m as usize] & mask;
        self.set_detached(m, intended);
        if self.col.parent[m as usize] == group && have == want {
            return;
        }
        self.unplace(m, cx);
        let f = &mut self.col.flags[m as usize];
        *f = (*f & !mask) | want;
        self.link_under(m, group, cx);
    }

    fn attach_under(&mut self, e: u32, q: u32, cx: &mut Cx) {
        let mask = EntryFlags::ORPHAN.0 | EntryFlags::CYCLE_BROKEN.0;
        if self.col.parent[e as usize] == q
            && !self.detached.contains_key(&e)
            && self.col.flags[e as usize] & mask == 0
        {
            return;
        }
        self.unplace(e, cx);
        self.clear_detached(e);
        self.col.flags[e as usize] &= !mask;
        self.link_under(e, q, cx);
    }

    fn link_under(&mut self, e: u32, p: u32, cx: &mut Cx) {
        self.link_child(p, e);
        self.col.parent[e as usize] = p;
        let s = self.summary(e);
        self.propagate(p, Summary::EMPTY, s, &[], &mut cx.touched);
        cx.updated.push(e);
    }

    /// Detaches `e` (with its subtree) from its display parent, if any.
    pub(crate) fn unplace(&mut self, e: u32, cx: &mut Cx) {
        let p = self.col.parent[e as usize];
        if p >= DEAD || e == self.root {
            return;
        }
        let s = self.summary(e);
        self.unlink_child(p, e);
        self.col.parent[e as usize] = NONE;
        self.propagate(p, s, Summary::EMPTY, &[], &mut cx.touched);
    }

    /// Queues every detached entry whose evaluation can depend on the record
    /// with lookup key `key`: those waiting on it directly, and transitively
    /// those waiting on them (an intended-parent chain through detached
    /// entries is exactly what a cycle walk follows). Evaluation depends only
    /// on intended parents and validity, never on current placement, so this
    /// is called when a record is created, removed, relinked or changes its
    /// ability to parent, not when an entry merely moves between groups.
    fn queue_pending(&self, key: u64, cx: &mut Cx) {
        let mut keys = vec![key];
        let mut seen: HashSet<u64> = HashSet::new();
        while let Some(k) = keys.pop() {
            if !seen.insert(k) {
                continue;
            }
            let Some(v) = self.pending.get(&k) else {
                continue;
            };
            for &e in v {
                cx.queue.push_back(e);
                let fr = self.col.file_ref[e as usize];
                if fr != NO_REF {
                    keys.push(ref_key(fr));
                }
            }
        }
    }

    fn set_detached(&mut self, e: u32, r: u64) {
        if let Some(old) = self.detached.insert(e, r) {
            if old == r {
                return;
            }
            self.unpend(e, old);
        }
        if r != NO_REF {
            self.pending.entry(ref_key(r)).or_default().push(e);
        }
    }

    fn clear_detached(&mut self, e: u32) {
        if let Some(old) = self.detached.remove(&e) {
            self.unpend(e, old);
        }
    }

    fn unpend(&mut self, e: u32, r: u64) {
        if r == NO_REF {
            return;
        }
        let k = ref_key(r);
        if let Some(v) = self.pending.get_mut(&k) {
            v.retain(|&x| x != e);
            if v.is_empty() {
                self.pending.remove(&k);
            }
        }
    }

    // -------------------------------------------------------------------------
    // Slots, rows, child lists
    // -------------------------------------------------------------------------

    fn alloc_slot(&mut self, e: &NewEntry) -> u32 {
        match self.free.pop() {
            Some(id) => {
                self.col.write(id, e);
                id
            }
            None => self.col.push(e),
        }
    }

    fn alloc_row(&mut self) -> u32 {
        match self.free_rows.pop() {
            Some(r) => {
                self.rows.reset(r);
                r
            }
            None => self.rows.push_empty(),
        }
    }

    pub(crate) fn link_child(&mut self, p: u32, c: u32) {
        let r = self.row_of(p).expect("parent is a directory");
        let i = r as usize;
        if self.rows.len[i] < self.row_cap(r) {
            let at = (self.rows.start[i] + self.rows.len[i]) as usize;
            self.children[at] = c;
            self.rows.len[i] += 1;
        } else {
            self.extra.entry(r).or_default().push(c);
        }
        self.path_cache.clear();
    }

    pub(crate) fn unlink_child(&mut self, p: u32, c: u32) {
        let r = self.row_of(p).expect("parent is a directory");
        let i = r as usize;
        let s = self.rows.start[i] as usize;
        let l = self.rows.len[i] as usize;
        if let Some(pos) = self.children[s..s + l].iter().position(|&x| x == c) {
            self.children.swap(s + pos, s + l - 1);
            self.rows.len[i] -= 1;
        } else if let Some(v) = self.extra.get_mut(&r)
            && let Some(pos) = v.iter().position(|&x| x == c)
        {
            v.swap_remove(pos);
            if v.is_empty() {
                self.extra.remove(&r);
            }
        }
        self.path_cache.clear();
    }

    fn finish_changes(&mut self, cx: Cx) -> ChangeSet {
        let Cx {
            mut created,
            mut updated,
            mut removed,
            mut touched,
            ..
        } = cx;
        let live = |v: &mut Vec<u32>, idx: &Index| {
            v.sort_unstable();
            v.dedup();
            v.retain(|&e| idx.live_u32(e));
        };
        live(&mut created, self);
        let created_set: HashSet<u32> = created.iter().copied().collect();
        live(&mut updated, self);
        updated.retain(|e| !created_set.contains(e));
        removed.sort_unstable();
        removed.dedup();
        live(&mut touched, self);
        let aggregates = touched
            .iter()
            .filter_map(|&d| self.aggregate(EntryId(d)).map(|a| (EntryId(d), a)))
            .collect();
        if !created.is_empty() || !removed.is_empty() || !updated.is_empty() {
            self.path_cache.clear();
        }
        ChangeSet {
            created: created.into_iter().map(EntryId).collect(),
            updated: updated.into_iter().map(EntryId).collect(),
            removed: removed.into_iter().map(EntryId).collect(),
            aggregates,
        }
    }
}
