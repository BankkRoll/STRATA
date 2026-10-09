//! Read queries (SPEC §9.2): sorted child pages, paths, top-N, filters,
//! extension and category breakdowns.
//!
//! Whole-volume scans (`scope` = root) run over the columns in parallel; scans
//! under a subtree walk its child lists.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::sync::Mutex;

use hashbrown::HashMap;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use strata_core::{CloudState, EntryFlags, EpochSecs, SizeMode, WideName};

use crate::fold;
use crate::index::{DEAD, EntryId, Index};
use crate::wtf8;

// -----------------------------------------------------------------------------
// Path cache
// -----------------------------------------------------------------------------

/// Paths cached by [`PathCache`].
const PATH_CACHE_CAP: usize = 1024;

/// Recently built paths (UTF-16, lossless), cleared whenever a live update
/// renames or moves anything.
///
/// PERF: a two-generation approximation of LRU: hits in the old generation are
/// promoted, and when the young generation fills up it becomes the old one.
/// Every operation is O(1), unlike exact LRU eviction by scanning.
#[derive(Debug, Default)]
pub(crate) struct PathCache {
    inner: Mutex<PathCacheInner>,
}

#[derive(Debug, Default)]
struct PathCacheInner {
    young: HashMap<u32, Box<[u16]>>,
    old: HashMap<u32, Box<[u16]>>,
}

impl PathCacheInner {
    fn insert(&mut self, id: u32, path: Box<[u16]>) {
        if self.young.len() >= PATH_CACHE_CAP / 2 {
            self.old = std::mem::take(&mut self.young);
        }
        self.young.insert(id, path);
    }
}

impl PathCache {
    fn get(&self, id: u32) -> Option<Box<[u16]>> {
        let mut g = self.inner.lock().ok()?;
        if let Some(p) = g.young.get(&id) {
            return Some(p.clone());
        }
        let p = g.old.remove(&id)?;
        g.insert(id, p.clone());
        Some(p)
    }

    fn put(&self, id: u32, path: &[u16]) {
        if let Ok(mut g) = self.inner.lock() {
            g.insert(id, path.into());
        }
    }

    pub(crate) fn clear(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.young.clear();
            g.old.clear();
        }
    }
}

// -----------------------------------------------------------------------------
// Value types
// -----------------------------------------------------------------------------

/// Column to sort children by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortKey {
    /// Display size (subtree total for directories) in the query's mode.
    Size,
    /// Item count: descendant files + directories (0 for files).
    Count,
    /// Name: natural order, case-insensitive.
    Name,
    /// Modification time (directories: newest in subtree).
    Modified,
    /// Category id, then size.
    Category,
}

/// A page of children in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildQuery {
    /// Sort column.
    pub key: SortKey,
    /// Largest/newest/last first.
    pub descending: bool,
    /// Size mode for [`SortKey::Size`] and tie-breaks.
    pub mode: SizeMode,
    /// Directories before files regardless of the key.
    pub dirs_first: bool,
    /// Rows to skip.
    pub offset: usize,
    /// Maximum rows to return.
    pub limit: usize,
}

impl Default for ChildQuery {
    fn default() -> Self {
        Self {
            key: SortKey::Size,
            descending: true,
            mode: SizeMode::Allocated,
            dirs_first: false,
            offset: 0,
            limit: usize::MAX,
        }
    }
}

/// Which entries a query considers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// Files and directories.
    #[default]
    Any,
    /// Files only.
    Files,
    /// Directories only.
    Dirs,
}

/// Combinable entry filter (all set conditions must hold).
///
/// Virtual nodes are excluded unless [`Filter::include_virtual`] is set.
///
/// # Example
///
/// ```
/// use strata_index::{EntryKind, Filter};
/// use strata_core::SizeMode;
/// let f = Filter {
///     kind: EntryKind::Files,
///     size: Some((1 << 30, u64::MAX)),
///     size_mode: SizeMode::Allocated,
///     ..Filter::default()
/// };
/// assert!(f.size.is_some());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Filter {
    /// Files, directories or both.
    pub kind: EntryKind,
    /// Extension ids (any of); see [`Index::extension_id`].
    pub exts: Vec<u16>,
    /// Category ids (any of).
    pub categories: Vec<u16>,
    /// Owning app ids (any of).
    pub owner_apps: Vec<u32>,
    /// Inclusive modification-time range (directories: newest in subtree).
    pub modified: Option<(EpochSecs, EpochSecs)>,
    /// Inclusive display-size range in [`Filter::size_mode`].
    pub size: Option<(u64, u64)>,
    /// Size mode for [`Filter::size`].
    pub size_mode: SizeMode,
    /// Every one of these flags must be set.
    pub flags_all: EntryFlags,
    /// At least one of these flags must be set (ignored when empty).
    pub flags_any: EntryFlags,
    /// None of these flags may be set.
    pub flags_none: EntryFlags,
    /// Cloud states (any of).
    pub cloud: Vec<CloudState>,
    /// Include virtual nodes.
    pub include_virtual: bool,
}

impl Filter {
    /// Whether entry `id` passes the filter.
    #[must_use]
    pub fn matches(&self, index: &Index, id: EntryId) -> bool {
        self.matches_u32(index, id.0)
    }

    pub(crate) fn matches_u32(&self, index: &Index, id: u32) -> bool {
        let col = &index.col;
        let i = id as usize;
        let f = col.flags[i];
        let is_dir = f & EntryFlags::DIR.0 != 0;
        if !self.include_virtual && f & EntryFlags::VIRTUAL.0 != 0 {
            return false;
        }
        match self.kind {
            EntryKind::Files if is_dir => return false,
            EntryKind::Dirs if !is_dir => return false,
            _ => {}
        }
        if f & self.flags_all.0 != self.flags_all.0 || f & self.flags_none.0 != 0 {
            return false;
        }
        if self.flags_any.0 != 0 && f & self.flags_any.0 == 0 {
            return false;
        }
        if !self.exts.is_empty() && !self.exts.contains(&col.ext_id[i]) {
            return false;
        }
        if !self.categories.is_empty() && !self.categories.contains(&col.category[i]) {
            return false;
        }
        if !self.owner_apps.is_empty() && !self.owner_apps.contains(&col.owner_app[i]) {
            return false;
        }
        if !self.cloud.is_empty() && !self.cloud.contains(&EntryFlags(f).cloud()) {
            return false;
        }
        if let Some((lo, hi)) = self.size {
            let s = index.size(EntryId(id), self.size_mode);
            if s < lo || s > hi {
                return false;
            }
        }
        if let Some((lo, hi)) = self.modified {
            match index.modified_u32(id) {
                Some(m) if m >= lo.0 && m <= hi.0 => {}
                _ => return false,
            }
        }
        true
    }
}

/// Files and bytes per extension or category.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Breakdown {
    /// Extension id or category id.
    pub id: u32,
    /// Lowercased extension (empty for "no extension") or category label.
    pub label: String,
    /// Number of files.
    pub files: u64,
    /// Bytes contributed in the query's mode.
    pub bytes: u64,
}

// -----------------------------------------------------------------------------
// Queries
// -----------------------------------------------------------------------------

impl Index {
    /// Modification time used by sorting and filters: the entry's own for
    /// files, the subtree's newest for directories. `None` when unknown.
    pub(crate) fn modified_u32(&self, id: u32) -> Option<u32> {
        if let Some(r) = self.row_of(id) {
            let v = self.rows.newest[r as usize];
            return (v != crate::index::NO_NEWEST).then_some(v);
        }
        self.valid_mtime(id)
    }

    /// Full path of an entry (lossless UTF-16): `prefix\a\b`. The root is
    /// `prefix\`. Recent results are cached.
    #[must_use]
    pub fn path(&self, id: EntryId) -> WideName {
        WideName::from_units(self.path_units(id.0))
    }

    /// Display path (unpaired surrogates become U+FFFD).
    #[must_use]
    pub fn path_string(&self, id: EntryId) -> String {
        String::from_utf16_lossy(&self.path_units(id.0))
    }

    fn path_units(&self, id: u32) -> Box<[u16]> {
        if !self.live_u32(id) {
            return Box::default();
        }
        if let Some(p) = self.path_cache.get(id) {
            return p;
        }
        let mut chain = Vec::new();
        let mut cur = id;
        let mut base: Option<Box<[u16]>> = None;
        while cur != self.root && chain.len() <= self.col.len() {
            if cur != id
                && let Some(p) = self.path_cache.get(cur)
            {
                base = Some(p);
                break;
            }
            chain.push(cur);
            let p = self.col.parent[cur as usize];
            if p >= DEAD {
                break;
            }
            cur = p;
        }
        let mut out: Vec<u16> = match base {
            Some(b) => b.into_vec(),
            None => {
                let mut v: Vec<u16> = self.opts.volume.prefix.encode_utf16().collect();
                v.push(u16::from(b'\\'));
                v
            }
        };
        for (k, &c) in chain.iter().rev().enumerate() {
            if k > 0 || out.last() != Some(&u16::from(b'\\')) {
                out.push(u16::from(b'\\'));
            }
            out.extend(wtf8::decode(self.names.get(c)));
        }
        self.path_cache.put(id, &out);
        out.into_boxed_slice()
    }

    /// One page of a directory's children in display order. Ties break by
    /// natural name order, then id, so pages are stable.
    #[must_use]
    pub fn children_sorted(&self, dir: EntryId, q: &ChildQuery) -> Vec<EntryId> {
        let kids: Vec<u32> = self.children(dir).map(|e| e.0).collect();
        let end = q.offset.saturating_add(q.limit).min(kids.len());
        if q.offset >= end {
            return Vec::new();
        }
        // PERF: sort keys are computed once per child (folded names packed in one
        // buffer, numeric keys in a vector), not once per comparison.
        let mut folded = Vec::with_capacity(kids.len() * 16);
        let mut spans = Vec::with_capacity(kids.len());
        let mut tmp = Vec::new();
        for &k in &kids {
            fold::fold_into(self.names.get(k), &mut tmp);
            spans.push((folded.len(), tmp.len()));
            folded.extend_from_slice(&tmp);
        }
        let name = |i: u32| {
            let (s, l) = spans[i as usize];
            &folded[s..s + l]
        };
        let keys: Vec<(u64, u64)> = kids
            .iter()
            .map(|&k| {
                let size = || self.size(EntryId(k), q.mode);
                match q.key {
                    SortKey::Size => (size(), 0),
                    SortKey::Count => (self.item_count(k), 0),
                    SortKey::Modified => (self.modified_u32(k).map_or(0, |m| u64::from(m) + 1), 0),
                    SortKey::Category => {
                        (u64::from(self.col.category[k as usize]), u64::MAX - size())
                    }
                    SortKey::Name => (0, 0),
                }
            })
            .collect();
        let is_dir: Vec<bool> = kids
            .iter()
            .map(|&k| q.dirs_first && self.col.has(k, EntryFlags::DIR))
            .collect();
        let cmp = |a: &u32, b: &u32| -> Ordering {
            let (a, b) = (*a, *b);
            let primary = if q.key == SortKey::Name {
                fold::natural_cmp(name(a), name(b))
            } else {
                keys[a as usize].cmp(&keys[b as usize])
            };
            let primary = if q.descending {
                primary.reverse()
            } else {
                primary
            };
            is_dir[b as usize]
                .cmp(&is_dir[a as usize])
                .then(primary)
                .then_with(|| fold::natural_cmp(name(a), name(b)))
                .then_with(|| kids[a as usize].cmp(&kids[b as usize]))
        };
        let mut order: Vec<u32> = (0..kids.len() as u32).collect();
        if end < order.len() / 4 {
            order.select_nth_unstable_by(end, cmp);
            order.truncate(end);
        }
        order.par_sort_unstable_by(cmp);
        order[q.offset..end]
            .iter()
            .map(|&i| EntryId(kids[i as usize]))
            .collect()
    }

    fn item_count(&self, id: u32) -> u64 {
        self.row_of(id).map_or(0, |r| {
            u64::from(self.rows.files[r as usize]) + u64::from(self.rows.dirs[r as usize])
        })
    }

    /// Calls `f` for every live entry in the subtree of `scope` (inclusive).
    pub fn for_each_in_subtree(&self, scope: EntryId, mut f: impl FnMut(EntryId)) {
        if !self.is_live(scope) {
            return;
        }
        let mut stack = vec![scope.0];
        while let Some(d) = stack.pop() {
            f(EntryId(d));
            if let Some(r) = self.row_of(d) {
                let (a, b) = self.child_slices(r);
                stack.extend(a.iter().chain(b).copied());
            }
        }
    }

    /// Live ids in `scope` matching `pred`, in parallel for the whole volume.
    fn collect_matching(
        &self,
        scope: Option<EntryId>,
        pred: impl Fn(u32) -> bool + Sync,
    ) -> Vec<u32> {
        match scope {
            Some(s) if s.0 != self.root => {
                let mut v = Vec::new();
                self.for_each_in_subtree(s, |e| {
                    if pred(e.0) {
                        v.push(e.0);
                    }
                });
                v
            }
            _ => (0..self.col.len() as u32)
                .into_par_iter()
                .filter(|&i| self.col.parent[i as usize] != DEAD && pred(i))
                .collect(),
        }
    }

    /// Entries in `scope` (whole volume when `None`) passing `filter`, in id
    /// order, at most `limit`.
    #[must_use]
    pub fn filter_entries(
        &self,
        scope: Option<EntryId>,
        filter: &Filter,
        limit: usize,
    ) -> Vec<EntryId> {
        let mut v = self.collect_matching(scope, |i| filter.matches_u32(self, i));
        v.sort_unstable();
        v.truncate(limit);
        v.into_iter().map(EntryId).collect()
    }

    /// The `n` largest entries of `kind` in `scope` by display size
    /// (contribution for files, subtree total for directories), largest
    /// first. Virtual nodes are excluded; `filter` narrows further.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # fn demo(index: &strata_index::Index) {
    /// use strata_index::EntryKind;
    /// use strata_core::SizeMode;
    /// for (id, bytes) in index.top_n(None, 50, EntryKind::Files, SizeMode::Allocated, None) {
    ///     println!("{bytes:>14} {}", index.path_string(id));
    /// }
    /// # }
    /// ```
    #[must_use]
    pub fn top_n(
        &self,
        scope: Option<EntryId>,
        n: usize,
        kind: EntryKind,
        mode: SizeMode,
        filter: Option<&Filter>,
    ) -> Vec<(EntryId, u64)> {
        if n == 0 {
            return Vec::new();
        }
        let want = |i: u32| -> bool {
            let f = self.col.flags[i as usize];
            if f & EntryFlags::VIRTUAL.0 != 0 {
                return false;
            }
            let dir = f & EntryFlags::DIR.0 != 0;
            let kind_ok = match kind {
                EntryKind::Any => true,
                EntryKind::Files => !dir,
                EntryKind::Dirs => dir,
            };
            kind_ok && filter.is_none_or(|flt| flt.matches_u32(self, i))
        };
        type Heap = BinaryHeap<Reverse<(u64, Reverse<u32>)>>;
        let push = |h: &mut Heap, i: u32| {
            let s = self.size(EntryId(i), mode);
            if h.len() < n {
                h.push(Reverse((s, Reverse(i))));
            } else if let Some(Reverse(min)) = h.peek()
                && (s, Reverse(i)) > *min
            {
                h.pop();
                h.push(Reverse((s, Reverse(i))));
            }
        };
        let heap: Heap = match scope {
            Some(s) if s.0 != self.root => {
                let mut h = Heap::new();
                self.for_each_in_subtree(s, |e| {
                    if want(e.0) {
                        push(&mut h, e.0);
                    }
                });
                h
            }
            _ => (0..self.col.len() as u32)
                .into_par_iter()
                .fold(Heap::new, |mut h, i| {
                    if self.col.parent[i as usize] != DEAD && want(i) {
                        push(&mut h, i);
                    }
                    h
                })
                .reduce(Heap::new, |mut a, b| {
                    for Reverse((_, Reverse(i))) in b {
                        push(&mut a, i);
                    }
                    a
                }),
        };
        let mut v: Vec<(u64, u32)> = heap
            .into_iter()
            .map(|Reverse((s, Reverse(i)))| (s, i))
            .collect();
        v.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        v.into_iter().map(|(s, i)| (EntryId(i), s)).collect()
    }

    /// Files and bytes per extension under `scope` (whole volume when `None`),
    /// largest first.
    #[must_use]
    pub fn extension_breakdown(&self, scope: Option<EntryId>, mode: SizeMode) -> Vec<Breakdown> {
        let slots = self.exts.names.len().max(1) + 1;
        let totals = self.tally(scope, mode, slots, |i| {
            let e = self.col.ext_id[i as usize] as usize;
            e.min(slots - 1)
        });
        let mut v: Vec<Breakdown> = totals
            .into_iter()
            .enumerate()
            .filter(|(_, (files, _))| *files > 0)
            .map(|(id, (files, bytes))| Breakdown {
                id: id as u32,
                label: wtf8::to_string_lossy(self.exts.name(id as u16)),
                files,
                bytes,
            })
            .collect();
        v.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.id.cmp(&b.id)));
        v
    }

    /// Files and bytes per category under `scope`, largest first.
    #[must_use]
    pub fn category_breakdown(&self, scope: Option<EntryId>, mode: SizeMode) -> Vec<Breakdown> {
        const SLOTS: usize = 1 << 16;
        let totals = self.tally(scope, mode, SLOTS, |i| {
            self.col.category[i as usize] as usize
        });
        let mut v: Vec<Breakdown> = totals
            .into_iter()
            .enumerate()
            .filter(|(_, (files, _))| *files > 0)
            .map(|(id, (files, bytes))| Breakdown {
                id: id as u32,
                label: strata_core::Category::from_u16(id as u16)
                    .label()
                    .to_owned(),
                files,
                bytes,
            })
            .collect();
        v.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.id.cmp(&b.id)));
        v
    }

    /// Sums (files, contributed bytes) of non-virtual files into `slots`
    /// buckets chosen by `bucket`.
    fn tally(
        &self,
        scope: Option<EntryId>,
        mode: SizeMode,
        slots: usize,
        bucket: impl Fn(u32) -> usize + Sync,
    ) -> Vec<(u64, u64)> {
        let is_file = |i: u32| {
            let f = self.col.flags[i as usize];
            f & (EntryFlags::DIR.0 | EntryFlags::VIRTUAL.0) == 0
        };
        let add = |mut acc: Vec<(u64, u64)>, i: u32| {
            let b = &mut acc[bucket(i)];
            b.0 += 1;
            b.1 += self.contrib(i, mode);
            acc
        };
        match scope {
            Some(s) if s.0 != self.root => {
                let mut acc = vec![(0u64, 0u64); slots];
                self.for_each_in_subtree(s, |e| {
                    if is_file(e.0) {
                        acc = add(std::mem::take(&mut acc), e.0);
                    }
                });
                acc
            }
            _ => (0..self.col.len() as u32)
                .into_par_iter()
                .with_min_len(1 << 14)
                .fold(
                    || vec![(0u64, 0u64); slots],
                    |acc, i| {
                        if self.col.parent[i as usize] != DEAD && is_file(i) {
                            add(acc, i)
                        } else {
                            acc
                        }
                    },
                )
                .reduce(
                    || vec![(0u64, 0u64); slots],
                    |mut a, b| {
                        for (x, y) in a.iter_mut().zip(b) {
                            x.0 += y.0;
                            x.1 += y.1;
                        }
                        a
                    },
                ),
        }
    }
}
