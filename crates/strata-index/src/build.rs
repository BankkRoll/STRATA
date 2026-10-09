//! Building an [`Index`] from [`ScanRecord`]s that arrive in any order.
//!
//! # Pipeline
//!
//! 1. **Stage** ([`IndexBuilder::push`]): each record becomes one staged entry
//!    per name link (one for link-less records), in arrival order. A record
//!    seen twice replaces the earlier copy. Parents are not needed yet.
//! 2. **Resolve**: each link's parent reference is looked up by key. A missing
//!    parent, a sequence mismatch (stale reference), a parent that is not a
//!    directory, or a parent that is a traversal-blocking reparse point
//!    (symlink, junction, mount point) makes the entry an orphan.
//! 3. **Break cycles**: entries on a loop of parent references are attached
//!    to "Orphaned entries" with [`EntryFlags::CYCLE_BROKEN`] (O(n) colouring
//!    walk).
//! 4. **Group NTFS metadata**: entries flagged [`EntryFlags::NTFS_METADATA`]
//!    whose parent is not itself metadata are displayed under the virtual
//!    "NTFS metadata" node (`$Extend\$UsnJrnl` stays under `$Extend`).
//! 5. **Lay out** the base region (directories first, then files, each sorted
//!    by key) and build the CSR child lists.
//! 6. **Aggregate** bottom-up in parallel.
//!
//! The same placement rules are applied incrementally by the live-update
//! path; the property tests in `tests/` check the two agree.

use std::time::{Duration, Instant};

use hashbrown::HashMap;
use rayon::prelude::*;
use strata_core::{CloudState, EntryFlags, EpochSecs, FileTime, NameLink, ReparseKind, ScanRecord};

use crate::IndexError;
use crate::ext::ExtTable;
use crate::index::{
    Columns, EntryTimes, Index, IndexOptions, MAX_ENTRIES, NO_REF, NONE, NewEntry, Rows, ref_key,
};
use crate::names::{self, NameStore};
use crate::query::PathCache;
use crate::wtf8;

/// Display name of the virtual NTFS metadata node.
pub const METADATA_NODE_NAME: &str = "NTFS metadata";
/// Display name of the virtual orphans node.
pub const ORPHANS_NODE_NAME: &str = "Orphaned entries";

/// Wall time of each [`IndexBuilder::finish`] phase (staging time is the
/// caller's).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BuildStats {
    /// Root detection and parent resolution (parallel).
    pub resolve: Duration,
    /// Cycle detection.
    pub cycles: Duration,
    /// Ordering, column permutation, names, placement and child lists.
    pub layout: Duration,
    /// Bottom-up aggregation (parallel by tree level).
    pub aggregate: Duration,
    /// The whole `finish`.
    pub total: Duration,
}

/// Flags the index computes itself; values from scanners are ignored.
const INDEX_OWNED: u32 = EntryFlags::ORPHAN.0
    | EntryFlags::CYCLE_BROKEN.0
    | EntryFlags::HARDLINK_SECONDARY.0
    | EntryFlags::VIRTUAL.0
    | EntryFlags::SUSPICIOUS_TIME.0;

// -----------------------------------------------------------------------------
// Record â†’ entry helpers (shared with the live path)
// -----------------------------------------------------------------------------

/// Entry flags for a record: scanner flags, minus index-owned bits, plus
/// derived reparse/cloud fields, `HAS_ADS` and `SUSPICIOUS_TIME`.
pub(crate) fn record_flags(rec: &ScanRecord, now: FileTime) -> EntryFlags {
    let mut f = EntryFlags(rec.flags.0 & !INDEX_OWNED);
    if f.reparse() == ReparseKind::None
        && let Some(rp) = &rec.reparse
    {
        f = f.with_reparse(ReparseKind::from_tag(rp.tag));
    }
    if f.reparse() == ReparseKind::Cloud && f.cloud() == CloudState::None {
        f = f.with_cloud(CloudState::from_attributes(rec.attributes));
    }
    if !rec.ads.is_empty() {
        f |= EntryFlags::HAS_ADS;
    }
    let t = &rec.times;
    if [t.created, t.modified, t.accessed, t.changed]
        .iter()
        .any(|&x| x.0 != 0 && x.is_suspicious(now))
    {
        f |= EntryFlags::SUSPICIOUS_TIME;
    }
    f
}

pub(crate) fn record_times(rec: &ScanRecord) -> EntryTimes {
    EntryTimes {
        created: EpochSecs::from_filetime(rec.times.created),
        modified: EpochSecs::from_filetime(rec.times.modified),
        accessed: EpochSecs::from_filetime(rec.times.accessed),
        changed: EpochSecs::from_filetime(rec.times.changed),
    }
}

/// WTF-8 name of a link, or a synthesized `<record N>` for link-less records.
pub(crate) fn link_name(rec: &ScanRecord, link: Option<&NameLink>) -> Vec<u8> {
    match link {
        Some(l) => wtf8::encode(l.name.units()),
        None => format!("<record {}>", rec.id.record()).into_bytes(),
    }
}

pub(crate) fn new_entry(rec: &ScanRecord, flags: EntryFlags, ext_id: u16) -> NewEntry {
    NewEntry {
        parent: NONE,
        flags: flags.0,
        logical: rec.sizes.total_logical(),
        allocated: rec.sizes.total_allocated(),
        times: record_times(rec),
        file_ref: rec.id.0,
        ext_id,
    }
}

/// Whether a record's own flags make its directory row partial.
pub(crate) fn own_partial(flags: EntryFlags) -> bool {
    flags.contains(EntryFlags::PARTIAL) || flags.contains(EntryFlags::ACCESS_DENIED)
}

// -----------------------------------------------------------------------------
// Builder
// -----------------------------------------------------------------------------

/// Streams [`ScanRecord`]s into a new [`Index`].
///
/// Records may arrive in any order and in any batch size; children may
/// arrive before their parents. See the module docs for the pipeline.
///
/// # Example
///
/// ```
/// use strata_core::*;
/// use strata_index::{IndexBuilder, IndexOptions};
///
/// let root = FileRef::from_parts(5, 5);
/// let dir = FileRef::from_parts(30, 2);
/// let link = |parent, name: &str| NameLink { parent, name: WideName::from_str_lossless(name) };
/// let base = ScanRecord {
///     id: root,
///     links: vec![link(root, "")],
///     attributes: 0,
///     flags: EntryFlags::DIR,
///     times: Times::default(),
///     fn_created: None,
///     sizes: Sizes::default(),
///     reparse: None,
///     ads: vec![],
/// };
/// let mut b = IndexBuilder::new(IndexOptions::default());
/// // The child arrives before its parent directory.
/// b.push(ScanRecord {
///     id: FileRef::from_parts(31, 1),
///     links: vec![link(dir, "model.gguf")],
///     flags: EntryFlags::EMPTY,
///     sizes: Sizes { logical: 7, allocated: 4096, ..Sizes::default() },
///     ..base.clone()
/// })?;
/// b.push(ScanRecord { id: dir, links: vec![link(root, "models")], ..base.clone() })?;
/// b.push(base)?;
/// let index = b.finish()?;
/// let models = index.lookup(dir).unwrap();
/// assert_eq!(index.aggregate(models).unwrap().allocated, 4096);
/// assert_eq!(index.path_string(index.lookup(FileRef::from_parts(31, 1)).unwrap()), r"\models\model.gguf");
/// # Ok::<(), strata_index::IndexError>(())
/// ```
#[derive(Debug)]
pub struct IndexBuilder {
    opts: IndexOptions,
    col: Columns,
    names: Vec<u8>,
    name_off: Vec<u32>,
    link_parent: Vec<u64>,
    rank: Vec<u16>,
    first: HashMap<u64, u32>,
    exts: ExtTable,
    partial: bool,
    blocks: Vec<(String, u64, u64)>,
}

impl IndexBuilder {
    /// A builder with the given options.
    #[must_use]
    pub fn new(opts: IndexOptions) -> Self {
        Self {
            opts,
            col: Columns::new(false),
            names: Vec::new(),
            name_off: Vec::new(),
            link_parent: Vec::new(),
            rank: Vec::new(),
            first: HashMap::new(),
            exts: ExtTable::default(),
            partial: false,
            blocks: Vec::new(),
        }
    }

    /// Pre-allocates for about `entries` entries.
    pub fn reserve(&mut self, entries: usize) {
        let c = &mut self.col;
        c.parent.reserve(entries);
        c.flags.reserve(entries);
        c.owner_app.reserve(entries);
        c.logical.reserve(entries);
        c.allocated.reserve(entries);
        c.mtime.reserve(entries);
        c.ctime.reserve(entries);
        c.atime.reserve(entries);
        c.mftchange.reserve(entries);
        c.file_ref.reserve(entries);
        c.category.reserve(entries);
        c.ext_id.reserve(entries);
        self.name_off.reserve(entries);
        self.link_parent.reserve(entries);
        self.rank.reserve(entries);
        self.first.reserve(entries);
        self.names.reserve(entries * 16);
    }

    /// Number of staged entries so far (including replaced ones).
    #[must_use]
    pub fn staged(&self) -> usize {
        self.col.len()
    }

    /// Marks the scan as partial (cancelled or incomplete): the root and
    /// therefore every ancestor chain reports [`EntryFlags::PARTIAL`].
    pub fn set_partial(&mut self, partial: bool) {
        self.partial = partial;
    }

    /// Adds a virtual leaf block under the root, e.g. "Unaccounted / system
    /// reserved" or "System Restore / Shadow copies" (SPEC Â§5, Â§7.5).
    pub fn add_virtual_block(&mut self, name: &str, logical: u64, allocated: u64) {
        self.blocks.push((name.to_owned(), logical, allocated));
    }

    /// Stages a batch of records.
    ///
    /// # Errors
    ///
    /// See [`IndexBuilder::push`].
    pub fn push_batch(
        &mut self,
        batch: impl IntoIterator<Item = ScanRecord>,
    ) -> Result<(), IndexError> {
        batch.into_iter().try_for_each(|r| self.push(r))
    }

    /// Stages one record. A later record with the same file reference key
    /// replaces an earlier one.
    ///
    /// # Errors
    ///
    /// [`IndexError::TooManyEntries`] when the volume would exceed
    /// [`MAX_ENTRIES`]; [`IndexError::ReservedFileRef`] for the reserved
    /// reference `u64::MAX`.
    pub fn push(&mut self, rec: ScanRecord) -> Result<(), IndexError> {
        if rec.id.0 == NO_REF {
            return Err(IndexError::ReservedFileRef(rec.id.0));
        }
        let n = rec.links.len().max(1);
        let total = self.col.len() as u64 + n as u64;
        if total > u64::from(MAX_ENTRIES) {
            return Err(IndexError::TooManyEntries(total));
        }
        let key = ref_key(rec.id.0);
        if let Some(&first) = self.first.get(&key) {
            self.kill(first);
        }
        let flags = record_flags(&rec, self.opts.now);
        let is_dir = rec.is_dir();
        let first = self.col.len() as u32;
        let links: Vec<Option<&NameLink>> = if rec.links.is_empty() {
            vec![None]
        } else {
            rec.links.iter().map(Some).collect()
        };
        for (rank, link) in links.into_iter().enumerate() {
            let name = link_name(&rec, link);
            let ext = if is_dir {
                0
            } else {
                self.exts.intern_name(&name)
            };
            let mut f = flags;
            f.set(EntryFlags::HARDLINK_SECONDARY, rank > 0);
            self.col.push(&new_entry(&rec, f, ext));
            self.name_off.push(self.names.len() as u32);
            names::write_prefixed(&mut self.names, &name);
            self.link_parent.push(link.map_or(NO_REF, |l| l.parent.0));
            self.rank.push(rank.min(usize::from(u16::MAX)) as u16);
        }
        self.first.insert(key, first);
        Ok(())
    }

    fn kill(&mut self, first: u32) {
        let mut i = first as usize;
        loop {
            self.col.parent[i] = crate::index::DEAD;
            i += 1;
            if i >= self.rank.len() || self.rank[i] == 0 {
                break;
            }
        }
    }

    fn staged_name(&self, i: u32) -> &[u8] {
        let off = self.name_off[i as usize] as usize;
        let (len, hdr) = names::read_len(&self.names, off);
        &self.names[off + hdr..off + hdr + len]
    }

    /// Resolves, lays out and aggregates the staged records.
    ///
    /// # Errors
    ///
    /// [`IndexError::TooManyEntries`] if virtual nodes push the count past
    /// [`MAX_ENTRIES`].
    pub fn finish(self) -> Result<Index, IndexError> {
        self.finish_with_stats().map(|(index, _)| index)
    }

    /// [`IndexBuilder::finish`], also reporting how long each phase took.
    ///
    /// # Errors
    ///
    /// As [`IndexBuilder::finish`].
    pub fn finish_with_stats(self) -> Result<(Index, BuildStats), IndexError> {
        let t0 = Instant::now();
        let mut stats = BuildStats::default();
        let n = self.col.len();
        let dead = |i: usize| self.col.parent[i] == crate::index::DEAD;
        let is_dir = |i: usize| self.col.flags[i] & EntryFlags::DIR.0 != 0;
        let key = |i: usize| ref_key(self.col.file_ref[i]);

        // Root: the self-linked directory with the lowest key.
        let root_staged = (0..n)
            .filter(|&i| !dead(i) && is_dir(i) && self.link_parent[i] == self.col.file_ref[i])
            .min_by_key(|&i| key(i))
            .map(|i| i as u32);

        // Resolve every link's parent to a staged index.
        let resolved: Vec<u32> = (0..n)
            .into_par_iter()
            .map(|i| {
                if dead(i) || Some(i as u32) == root_staged {
                    return NONE;
                }
                let p = self.link_parent[i];
                if p == NO_REF {
                    return NONE;
                }
                let Some(&j) = self.first.get(&ref_key(p)) else {
                    return NONE;
                };
                let f = EntryFlags(self.col.flags[j as usize]);
                let ok = self.col.file_ref[j as usize] == p
                    && f.contains(EntryFlags::DIR)
                    && !f.reparse().blocks_traversal();
                if ok { j } else { NONE }
            })
            .collect();

        stats.resolve = t0.elapsed();
        let t = Instant::now();
        let cycle = find_cycles(&resolved, dead);
        stats.cycles = t.elapsed();
        let t = Instant::now();

        // Base order: directories first, then files, by (key, rank).
        let mut order: Vec<u32> = (0..n as u32).filter(|&i| !dead(i as usize)).collect();
        order.par_sort_unstable_by_key(|&i| {
            let i = i as usize;
            (!is_dir(i), key(i), self.rank[i])
        });
        let base_len = order.len() as u32;
        let base_dirs = order.partition_point(|&i| is_dir(i as usize)) as u32;
        let mut old_to_new = vec![NONE; n];
        for (new, &old) in order.iter().enumerate() {
            old_to_new[old as usize] = new as u32;
        }

        let virtual_count = 2 + u32::from(root_staged.is_none()) + self.blocks.len() as u32;
        let total = u64::from(base_len) + u64::from(virtual_count);
        if total > u64::from(MAX_ENTRIES) {
            return Err(IndexError::TooManyEntries(total));
        }

        let mut col = self.col.gather(&order);
        let names = NameStore::from_base(order.iter().map(|&i| self.staged_name(i)));
        let mut detached = HashMap::new();
        let next_virtual = base_len;
        let root = match root_staged {
            Some(r) => old_to_new[r as usize],
            None => next_virtual,
        };
        let first_group = next_virtual + u32::from(root_staged.is_none());
        let metadata = first_group;
        let orphans = first_group + 1;

        let meta_flag = EntryFlags::NTFS_METADATA.0;
        for (new, &old) in order.iter().enumerate() {
            let old = old as usize;
            let new_u = new as u32;
            if new_u == root {
                col.parent[new] = NONE;
                continue;
            }
            let r = resolved[old];
            let intended = self.link_parent[old];
            let parent = if r == NONE {
                col.flags[new] |= EntryFlags::ORPHAN.0;
                detached.insert(new_u, intended);
                orphans
            } else if cycle[old] {
                col.flags[new] |= EntryFlags::CYCLE_BROKEN.0;
                detached.insert(new_u, intended);
                orphans
            } else if self.col.flags[old] & meta_flag != 0
                && (Some(r) == root_staged || self.col.flags[r as usize] & meta_flag == 0)
            {
                detached.insert(new_u, intended);
                metadata
            } else {
                old_to_new[r as usize]
            };
            col.parent[new] = parent;
        }

        let mut index = Index {
            col,
            names,
            exts: self.exts,
            rows: Rows::default(),
            children: Vec::new(),
            extra: HashMap::new(),
            delta_rows: HashMap::new(),
            free_rows: Vec::new(),
            base_len,
            base_dirs,
            delta_keys: HashMap::new(),
            link_counts: HashMap::new(),
            free: Vec::new(),
            detached,
            pending: HashMap::new(),
            root,
            orphans,
            metadata,
            live: base_len,
            opts: self.opts,
            path_cache: PathCache::default(),
        };

        if root_staged.is_none() {
            let id = index.push_virtual(NONE, b"", true, 0, 0);
            debug_assert_eq!(id, root);
        }
        let m = index.push_virtual(root, METADATA_NODE_NAME.as_bytes(), true, 0, 0);
        let o = index.push_virtual(root, ORPHANS_NODE_NAME.as_bytes(), true, 0, 0);
        debug_assert_eq!((m, o), (metadata, orphans));
        for (name, l, a) in &self.blocks {
            index.push_virtual(root, name.as_bytes(), false, *l, *a);
        }

        index.layout_tree();
        for r in 0..index.base_dirs {
            let own = own_partial(index.col.flags(r));
            index.rows.set_own_partial(r, own);
        }
        if self.partial {
            let r = index.row_of(index.root).expect("root is a directory");
            index.rows.set_own_partial(r, true);
        }
        index.rebuild_link_counts();
        index.rebuild_pending();
        stats.layout = t.elapsed();
        let t = Instant::now();
        index.aggregate_all();
        stats.aggregate = t.elapsed();
        if index.opts.lite {
            index.col.lite = true;
            for v in [
                &mut index.col.mtime,
                &mut index.col.ctime,
                &mut index.col.atime,
                &mut index.col.mftchange,
            ] {
                *v = Vec::new();
            }
        }
        index.shrink_to_fit();
        stats.total = t0.elapsed();
        Ok((index, stats))
    }
}

/// Marks every staged entry that lies on a cycle of `resolved` parent links.
///
/// Each entry is visited once: walk up from an unvisited entry, colouring the
/// path "in progress"; reaching an in-progress entry means the path closed a
/// loop, whose members are the path suffix from that entry.
fn find_cycles(resolved: &[u32], dead: impl Fn(usize) -> bool) -> Vec<bool> {
    const NEW: u8 = 0;
    const ON_PATH: u8 = 1;
    const DONE: u8 = 2;
    let n = resolved.len();
    let mut state = vec![NEW; n];
    let mut cycle = vec![false; n];
    let mut path: Vec<u32> = Vec::new();
    for s in 0..n {
        if state[s] != NEW || dead(s) {
            continue;
        }
        path.clear();
        let mut cur = s as u32;
        loop {
            match state[cur as usize] {
                DONE => break,
                ON_PATH => {
                    let pos = path.iter().rposition(|&x| x == cur).expect("on path");
                    for &m in &path[pos..] {
                        cycle[m as usize] = true;
                    }
                    break;
                }
                _ => {}
            }
            state[cur as usize] = ON_PATH;
            path.push(cur);
            let next = resolved[cur as usize];
            if next == NONE {
                break;
            }
            cur = next;
        }
        for &m in &path {
            state[m as usize] = DONE;
        }
    }
    cycle
}

// -----------------------------------------------------------------------------
// Layout helpers shared by build, compaction and cache load
// -----------------------------------------------------------------------------

impl Index {
    /// Appends a virtual node (delta region) and returns its id. The caller
    /// links it into the tree (or `layout_tree` does, during a build).
    pub(crate) fn push_virtual(
        &mut self,
        parent: u32,
        name: &[u8],
        dir: bool,
        logical: u64,
        allocated: u64,
    ) -> u32 {
        let mut flags = EntryFlags::VIRTUAL;
        if dir {
            flags |= EntryFlags::DIR;
        }
        let e = NewEntry {
            parent,
            flags: flags.0,
            logical,
            allocated,
            times: EntryTimes::default(),
            file_ref: NO_REF,
            ext_id: 0,
        };
        let id = self.col.push(&e);
        self.names.set(id, name);
        if dir {
            let r = self.rows.push_empty();
            self.delta_rows.insert(id, r);
        }
        self.live += 1;
        id
    }

    /// Builds the directory rows (empty aggregates) and child lists from the
    /// `parent` column. Base directories get CSR ranges in row order; delta
    /// directories' children go to `extra`. Rows of delta directories that
    /// already exist (virtual nodes) are kept.
    pub(crate) fn layout_tree(&mut self) {
        let base_dirs = self.base_dirs as usize;
        let delta_rows = self.rows.len();
        let mut rows = Rows::default();
        for _ in 0..base_dirs + delta_rows {
            rows.push_empty();
        }
        // Delta rows were numbered from 0 before the base rows existed.
        let shifted: HashMap<u32, u32> = self
            .delta_rows
            .iter()
            .map(|(&id, &r)| (id, r + base_dirs as u32))
            .collect();
        self.delta_rows = shifted;
        self.rows = rows;

        let mut count = vec![0u32; base_dirs];
        let mut extra: HashMap<u32, Vec<u32>> = HashMap::new();
        for c in 0..self.col.len() as u32 {
            let p = self.col.parent[c as usize];
            if p >= crate::index::DEAD {
                continue;
            }
            if (p as usize) < base_dirs {
                count[p as usize] += 1;
            } else if let Some(r) = self.row_of(p) {
                extra.entry(r).or_default().push(c);
            }
        }
        let mut start = 0u32;
        for (r, &c) in count.iter().enumerate() {
            self.rows.start[r] = start;
            start += c;
        }
        let mut children = vec![0u32; start as usize];
        for c in 0..self.col.len() as u32 {
            let p = self.col.parent[c as usize];
            if (p as usize) < base_dirs {
                let r = p as usize;
                children[(self.rows.start[r] + self.rows.len[r]) as usize] = c;
                self.rows.len[r] += 1;
            }
        }
        self.children = children;
        self.extra = extra;
    }

    /// Link counts of a freshly laid-out index. Every entry with a file
    /// reference is in the base region, sorted by key within the directory and
    /// file partitions, so equal keys are adjacent and a run scan suffices.
    pub(crate) fn rebuild_link_counts(&mut self) {
        let mut counts: HashMap<u64, u32> = HashMap::new();
        let refs = &self.col.file_ref;
        for (lo, hi) in [
            (0, self.base_dirs as usize),
            (self.base_dirs as usize, self.base_len as usize),
        ] {
            let mut i = lo;
            while i < hi {
                let k = ref_key(refs[i]);
                let mut j = i + 1;
                while j < hi && ref_key(refs[j]) == k {
                    j += 1;
                }
                if j - i > 1 {
                    counts.insert(k, (j - i) as u32);
                }
                i = j;
            }
        }
        self.link_counts = counts;
    }

    pub(crate) fn rebuild_pending(&mut self) {
        let mut pending: HashMap<u64, Vec<u32>> = HashMap::new();
        for (&id, &r) in &self.detached {
            if r != NO_REF {
                pending.entry(ref_key(r)).or_default().push(id);
            }
        }
        self.pending = pending;
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.col.shrink_to_fit();
        self.names.shrink_to_fit();
        self.rows.shrink_to_fit();
        self.children.shrink_to_fit();
        self.extra.shrink_to_fit();
        self.delta_rows.shrink_to_fit();
        self.link_counts.shrink_to_fit();
        self.detached.shrink_to_fit();
        self.pending.shrink_to_fit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycles_are_found_and_tails_are_not() {
        // 0 â†’ 1 â†’ 2 â†’ 0 is a loop; 3 â†’ 0 leads into it; 4 â†’ NONE.
        let resolved = vec![1, 2, 0, 0, NONE];
        let c = find_cycles(&resolved, |_| false);
        assert_eq!(c, vec![true, true, true, false, false]);
        // Self loop.
        let c = find_cycles(&[0], |_| false);
        assert_eq!(c, vec![true]);
    }
}
