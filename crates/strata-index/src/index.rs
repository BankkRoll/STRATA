//! The [`Index`]: struct-of-arrays entry columns, the directory table, the
//! child lists and the `FileRef` lookup.
//!
//! # Layout
//!
//! Every entry is a `u32` [`EntryId`] into parallel columns. The layout is
//! tuned to stay under 64 bytes per entry (names excluded):
//!
//! | column | bytes | notes |
//! |---|---|---|
//! | `parent` | 4 | `NONE` for the root, `DEAD` for removed slots |
//! | `flags` | 4 | [`EntryFlags`] |
//! | `owner_app` | 4 | classifier output |
//! | `logical`, `allocated` | 4 + 4 | `u32`; values ≥ `u32::MAX` spill to a side map |
//! | `mtime`/`ctime`/`atime`/`mftchange` | 16 | [`EpochSecs`]; absent in lite mode |
//! | `file_ref` | 8 | sorted within the base region (see below) |
//! | `category`, `ext_id` | 2 + 2 | |
//! | child slot | 4 | position in the CSR `children` array |
//! | name sample | 0.25 | one offset per 16 entries ([`crate::names`]) |
//!
//! Directories additionally own a row in a side table (first child range,
//! subtree logical/allocated, file and dir counts, newest/oldest mtime,
//! largest descendant in each size mode): 48 bytes per directory.
//!
//! # Base region and delta region
//!
//! A build (or [`Index::compact`]) lays entries out as *base*: directories
//! first, then files, each group sorted by file-reference key. Consequences:
//!
//! - `FileRef → EntryId` is a binary search over the `file_ref` column, so no
//!   hash map is needed for the millions of base entries.
//! - A base directory's row index equals its `EntryId`.
//! - Children are stored contiguously (CSR) per base directory.
//!
//! Entries created by live updates go to the *delta* region (ids past the
//! base), with small hash maps for their lookup keys, rows and extra
//! children. Removed base slots become tombstones (`parent == DEAD`) and are
//! never reused, because reuse would break the sorted `file_ref` order;
//! removed delta slots go on a free list. [`Index::compact`] rebuilds a
//! dense base from everything live.

use hashbrown::HashMap;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use strata_core::{Category, EntryFlags, EpochSecs, FileRef, FileTime, SizeMode, WideName};

use crate::ext::ExtTable;
use crate::mem::{map_bytes, vec_bytes};
use crate::names::NameStore;
use crate::query::PathCache;
use crate::wtf8;

// -----------------------------------------------------------------------------
// Ids and sentinels
// -----------------------------------------------------------------------------

/// Identifier of one entry (one name link) in an [`Index`].
///
/// Ids are dense indexes into the index columns. They stay stable across live
/// updates (removed ids are tombstoned or recycled, never shifted) and change
/// only on [`Index::compact`] or a rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryId(pub u32);

impl EntryId {
    /// The id as a column index.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Maximum number of entries an index accepts. Ids at the top of the `u32`
/// range are reserved for sentinels and virtual nodes, so builds refuse
/// gracefully (with [`crate::IndexError::TooManyEntries`]) past this point.
pub const MAX_ENTRIES: u32 = u32::MAX - (1 << 16);

/// Bytes of name storage records may use. Name offsets are `u32`; the top
/// 64 KiB is left for virtual node names, which are added infallibly.
pub(crate) const MAX_NAME_BYTES: u64 = u32::MAX as u64 - (1 << 16);

/// Capacity limits of an index. Always the real constants outside tests,
/// which shrink them to exercise the refusal paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    /// Maximum entries (see [`MAX_ENTRIES`]).
    pub entries: u64,
    /// Maximum bytes of length-prefixed record names (see [`MAX_NAME_BYTES`]).
    pub name_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            entries: u64::from(MAX_ENTRIES),
            name_bytes: MAX_NAME_BYTES,
        }
    }
}

/// Upper bound on the name-buffer bytes a record's links need (name plus
/// LEB128 prefix), counting link-less records' synthesized names.
pub(crate) fn name_bytes_needed(rec: &strata_core::ScanRecord) -> u64 {
    if rec.links.is_empty() {
        return 32;
    }
    rec.links
        .iter()
        .map(|l| l.name.units().len() as u64 * 3 + 10)
        .sum()
}

/// No entry (root's parent, empty largest-descendant, ...).
pub(crate) const NONE: u32 = u32::MAX;
/// Parent value marking a removed slot.
pub(crate) const DEAD: u32 = u32::MAX - 1;
/// File reference of virtual nodes, and "no intended parent".
pub(crate) const NO_REF: u64 = u64::MAX;
/// Size column sentinel: the real value lives in `Columns::big`.
pub(crate) const BIG: u32 = u32::MAX;
/// `newest` value meaning "no valid modification time in the subtree".
pub(crate) const NO_NEWEST: u32 = 0;
/// `oldest` value meaning "no valid modification time in the subtree".
pub(crate) const NO_OLDEST: u32 = u32::MAX;

/// Lookup key of a file reference: the record number for real NTFS
/// references (so a reused record with a new sequence number collides with
/// its stale predecessor, which is how staleness is detected), the full value
/// for walker-synthesized ids.
#[inline]
pub(crate) const fn ref_key(r: u64) -> u64 {
    if r & FileRef::SYNTHETIC_BIT != 0 {
        r
    } else {
        r & FileRef::RECORD_MASK
    }
}

// -----------------------------------------------------------------------------
// Options and public value types
// -----------------------------------------------------------------------------

/// Volume identity and USN position, stored in the cache header.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeInfo {
    /// Volume serial number.
    pub serial: u64,
    /// USN journal id at the time of the scan (0 when unknown).
    pub usn_journal_id: u64,
    /// Last USN applied to the index.
    pub last_usn: i64,
    /// Display prefix for paths, e.g. `C:`. The root's path is `prefix\`.
    pub prefix: String,
}

/// Options fixed at build time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexOptions {
    /// Split hardlinked files' bytes evenly across all their paths instead of
    /// attributing them to the primary path. Off by default.
    pub split_hardlinks: bool,
    /// Lite index for low-memory systems: per-entry timestamps are not kept.
    /// Directory aggregates (including newest/oldest mtime) are still
    /// computed at build time; afterwards they only move monotonically.
    pub lite: bool,
    /// Reference "now" for suspicious-timestamp detection.
    pub now: FileTime,
    /// Volume identity.
    pub volume: VolumeInfo,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            split_hardlinks: false,
            lite: false,
            now: FileTime(u64::MAX / 2),
            volume: VolumeInfo::default(),
        }
    }
}

/// Compact timestamps of one entry (second precision since 2000-01-01 UTC).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EntryTimes {
    /// Creation time.
    pub created: EpochSecs,
    /// Last content modification.
    pub modified: EpochSecs,
    /// Last access (unreliable: Windows may disable or coarsen updates).
    pub accessed: EpochSecs,
    /// Last MFT record change (0 when unknown).
    pub changed: EpochSecs,
}

/// Subtree totals of one directory.
///
/// Sizes include the directory's own bytes (index overhead, ADS). Counts
/// exclude the directory itself and virtual nodes. Times exclude virtual
/// entries, unknown (zero) times and entries flagged
/// [`EntryFlags::SUSPICIOUS_TIME`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DirAggregate {
    /// Subtree logical bytes.
    pub logical: u64,
    /// Subtree allocated bytes.
    pub allocated: u64,
    /// Descendant files (every hardlink path counts).
    pub files: u32,
    /// Descendant directories.
    pub dirs: u32,
    /// Newest modification time in the subtree, including the directory.
    pub newest: Option<EpochSecs>,
    /// Oldest modification time in the subtree, including the directory.
    pub oldest: Option<EpochSecs>,
    /// Largest descendant file by allocated contribution.
    pub largest_allocated: Option<EntryId>,
    /// Largest descendant file by logical contribution.
    pub largest_logical: Option<EntryId>,
    /// Some part of the subtree is incomplete (partial scan, access denied).
    pub partial: bool,
}

impl DirAggregate {
    /// Subtree size in `mode`.
    #[must_use]
    pub const fn size(&self, mode: SizeMode) -> u64 {
        match mode {
            SizeMode::Allocated => self.allocated,
            SizeMode::Logical => self.logical,
        }
    }

    /// Largest descendant file in `mode`.
    #[must_use]
    pub const fn largest(&self, mode: SizeMode) -> Option<EntryId> {
        match mode {
            SizeMode::Allocated => self.largest_allocated,
            SizeMode::Logical => self.largest_logical,
        }
    }
}

// -----------------------------------------------------------------------------
// Columns
// -----------------------------------------------------------------------------

/// Per-entry columns.
#[derive(Debug, Clone, Default)]
pub(crate) struct Columns {
    pub(crate) parent: Vec<u32>,
    pub(crate) flags: Vec<u32>,
    pub(crate) owner_app: Vec<u32>,
    pub(crate) logical: Vec<u32>,
    pub(crate) allocated: Vec<u32>,
    /// `[logical, allocated]` for entries with a [`BIG`] size column.
    pub(crate) big: HashMap<u32, [u64; 2]>,
    pub(crate) mtime: Vec<u32>,
    pub(crate) ctime: Vec<u32>,
    pub(crate) atime: Vec<u32>,
    pub(crate) mftchange: Vec<u32>,
    pub(crate) file_ref: Vec<u64>,
    pub(crate) category: Vec<u16>,
    pub(crate) ext_id: Vec<u16>,
    pub(crate) lite: bool,
}

/// Values for one new entry.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NewEntry {
    pub(crate) parent: u32,
    pub(crate) flags: u32,
    pub(crate) logical: u64,
    pub(crate) allocated: u64,
    pub(crate) times: EntryTimes,
    pub(crate) file_ref: u64,
    pub(crate) ext_id: u16,
}

impl Columns {
    pub(crate) fn new(lite: bool) -> Self {
        Self {
            lite,
            ..Self::default()
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.parent.len()
    }

    pub(crate) fn push(&mut self, e: &NewEntry) -> u32 {
        let id = self.parent.len() as u32;
        self.parent.push(e.parent);
        self.flags.push(e.flags);
        self.owner_app.push(0);
        self.logical.push(0);
        self.allocated.push(0);
        if !self.lite {
            self.mtime.push(0);
            self.ctime.push(0);
            self.atime.push(0);
            self.mftchange.push(0);
        }
        self.file_ref.push(e.file_ref);
        self.category.push(0);
        self.ext_id.push(e.ext_id);
        self.write(id, e);
        id
    }

    /// Overwrites slot `id` with `e`, resetting classifier columns.
    pub(crate) fn write(&mut self, id: u32, e: &NewEntry) {
        let i = id as usize;
        self.parent[i] = e.parent;
        self.flags[i] = e.flags;
        self.owner_app[i] = 0;
        self.category[i] = 0;
        self.file_ref[i] = e.file_ref;
        self.ext_id[i] = e.ext_id;
        self.set_sizes(id, e.logical, e.allocated);
        self.set_times(id, e.times);
    }

    pub(crate) fn set_sizes(&mut self, id: u32, logical: u64, allocated: u64) {
        let i = id as usize;
        if logical >= u64::from(BIG) || allocated >= u64::from(BIG) {
            self.logical[i] = if logical >= u64::from(BIG) {
                BIG
            } else {
                logical as u32
            };
            self.allocated[i] = if allocated >= u64::from(BIG) {
                BIG
            } else {
                allocated as u32
            };
            self.big.insert(id, [logical, allocated]);
        } else {
            self.logical[i] = logical as u32;
            self.allocated[i] = allocated as u32;
            if !self.big.is_empty() {
                self.big.remove(&id);
            }
        }
    }

    pub(crate) fn set_times(&mut self, id: u32, t: EntryTimes) {
        if self.lite {
            return;
        }
        let i = id as usize;
        self.mtime[i] = t.modified.0;
        self.ctime[i] = t.created.0;
        self.atime[i] = t.accessed.0;
        self.mftchange[i] = t.changed.0;
    }

    #[inline]
    pub(crate) fn logical(&self, id: u32) -> u64 {
        let v = self.logical[id as usize];
        if v == BIG {
            self.big.get(&id).map_or(u64::from(BIG), |b| b[0])
        } else {
            u64::from(v)
        }
    }

    #[inline]
    pub(crate) fn allocated(&self, id: u32) -> u64 {
        let v = self.allocated[id as usize];
        if v == BIG {
            self.big.get(&id).map_or(u64::from(BIG), |b| b[1])
        } else {
            u64::from(v)
        }
    }

    #[inline]
    pub(crate) fn own(&self, id: u32, mode: SizeMode) -> u64 {
        match mode {
            SizeMode::Allocated => self.allocated(id),
            SizeMode::Logical => self.logical(id),
        }
    }

    #[inline]
    pub(crate) fn flags(&self, id: u32) -> EntryFlags {
        EntryFlags(self.flags[id as usize])
    }

    #[inline]
    pub(crate) fn has(&self, id: u32, f: EntryFlags) -> bool {
        self.flags[id as usize] & f.0 != 0
    }

    #[inline]
    pub(crate) fn set_flag(&mut self, id: u32, f: EntryFlags, on: bool) {
        let v = &mut self.flags[id as usize];
        if on {
            *v |= f.0;
        } else {
            *v &= !f.0;
        }
    }

    #[inline]
    pub(crate) fn mtime(&self, id: u32) -> Option<u32> {
        self.mtime.get(id as usize).copied()
    }

    pub(crate) fn times(&self, id: u32) -> Option<EntryTimes> {
        let i = id as usize;
        if self.lite || i >= self.mtime.len() {
            return None;
        }
        Some(EntryTimes {
            created: EpochSecs(self.ctime[i]),
            modified: EpochSecs(self.mtime[i]),
            accessed: EpochSecs(self.atime[i]),
            changed: EpochSecs(self.mftchange[i]),
        })
    }

    /// Permutes every column so that new slot `i` holds old slot `order[i]`.
    pub(crate) fn gather(&self, order: &[u32]) -> Self {
        fn g<T: Copy + Send + Sync>(v: &[T], order: &[u32]) -> Vec<T> {
            if v.is_empty() {
                return Vec::new();
            }
            order.par_iter().map(|&o| v[o as usize]).collect()
        }
        let mut big = HashMap::new();
        if !self.big.is_empty() {
            for (new, &old) in order.iter().enumerate() {
                if let Some(b) = self.big.get(&old) {
                    big.insert(new as u32, *b);
                }
            }
        }
        Self {
            parent: g(&self.parent, order),
            flags: g(&self.flags, order),
            owner_app: g(&self.owner_app, order),
            logical: g(&self.logical, order),
            allocated: g(&self.allocated, order),
            big,
            mtime: g(&self.mtime, order),
            ctime: g(&self.ctime, order),
            atime: g(&self.atime, order),
            mftchange: g(&self.mftchange, order),
            file_ref: g(&self.file_ref, order),
            category: g(&self.category, order),
            ext_id: g(&self.ext_id, order),
            lite: self.lite,
        }
    }

    pub(crate) fn heap_bytes(&self) -> (u64, u64) {
        let cols = vec_bytes(&self.parent)
            + vec_bytes(&self.flags)
            + vec_bytes(&self.owner_app)
            + vec_bytes(&self.logical)
            + vec_bytes(&self.allocated)
            + vec_bytes(&self.mtime)
            + vec_bytes(&self.ctime)
            + vec_bytes(&self.atime)
            + vec_bytes(&self.mftchange)
            + vec_bytes(&self.file_ref)
            + vec_bytes(&self.category)
            + vec_bytes(&self.ext_id);
        (cols, map_bytes(self.big.capacity(), 20))
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.parent.shrink_to_fit();
        self.flags.shrink_to_fit();
        self.owner_app.shrink_to_fit();
        self.logical.shrink_to_fit();
        self.allocated.shrink_to_fit();
        self.big.shrink_to_fit();
        self.mtime.shrink_to_fit();
        self.ctime.shrink_to_fit();
        self.atime.shrink_to_fit();
        self.mftchange.shrink_to_fit();
        self.file_ref.shrink_to_fit();
        self.category.shrink_to_fit();
        self.ext_id.shrink_to_fit();
    }
}

// -----------------------------------------------------------------------------
// Directory rows
// -----------------------------------------------------------------------------

/// Directory side table, one row per directory, struct-of-arrays.
#[derive(Debug, Clone, Default)]
pub(crate) struct Rows {
    /// Start of the row's CSR range in `Index::children`.
    pub(crate) start: Vec<u32>,
    /// Used length of the CSR range.
    pub(crate) len: Vec<u32>,
    pub(crate) files: Vec<u32>,
    pub(crate) dirs: Vec<u32>,
    pub(crate) newest: Vec<u32>,
    pub(crate) oldest: Vec<u32>,
    pub(crate) largest_a: Vec<u32>,
    pub(crate) largest_l: Vec<u32>,
    pub(crate) sub_l: Vec<u64>,
    pub(crate) sub_a: Vec<u64>,
    /// Bitset: the directory itself is partial (scanner flag, access denied,
    /// or the root of a partial scan).
    pub(crate) own_partial: Vec<u64>,
}

impl Rows {
    pub(crate) fn len(&self) -> usize {
        self.start.len()
    }

    pub(crate) fn push_empty(&mut self) -> u32 {
        let r = self.start.len() as u32;
        self.start.push(0);
        self.len.push(0);
        self.files.push(0);
        self.dirs.push(0);
        self.newest.push(NO_NEWEST);
        self.oldest.push(NO_OLDEST);
        self.largest_a.push(NONE);
        self.largest_l.push(NONE);
        self.sub_l.push(0);
        self.sub_a.push(0);
        if self.own_partial.len() * 64 <= r as usize {
            self.own_partial.push(0);
        }
        r
    }

    pub(crate) fn reset(&mut self, r: u32) {
        let i = r as usize;
        self.start[i] = 0;
        self.len[i] = 0;
        self.files[i] = 0;
        self.dirs[i] = 0;
        self.newest[i] = NO_NEWEST;
        self.oldest[i] = NO_OLDEST;
        self.largest_a[i] = NONE;
        self.largest_l[i] = NONE;
        self.sub_l[i] = 0;
        self.sub_a[i] = 0;
        self.set_own_partial(r, false);
    }

    #[inline]
    pub(crate) fn own_partial(&self, r: u32) -> bool {
        self.own_partial[r as usize / 64] & (1 << (r % 64)) != 0
    }

    #[inline]
    pub(crate) fn set_own_partial(&mut self, r: u32, on: bool) {
        let w = &mut self.own_partial[r as usize / 64];
        if on {
            *w |= 1 << (r % 64);
        } else {
            *w &= !(1 << (r % 64));
        }
    }

    pub(crate) fn heap_bytes(&self) -> u64 {
        vec_bytes(&self.start)
            + vec_bytes(&self.len)
            + vec_bytes(&self.files)
            + vec_bytes(&self.dirs)
            + vec_bytes(&self.newest)
            + vec_bytes(&self.oldest)
            + vec_bytes(&self.largest_a)
            + vec_bytes(&self.largest_l)
            + vec_bytes(&self.sub_l)
            + vec_bytes(&self.sub_a)
            + vec_bytes(&self.own_partial)
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.start.shrink_to_fit();
        self.len.shrink_to_fit();
        self.files.shrink_to_fit();
        self.dirs.shrink_to_fit();
        self.newest.shrink_to_fit();
        self.oldest.shrink_to_fit();
        self.largest_a.shrink_to_fit();
        self.largest_l.shrink_to_fit();
        self.sub_l.shrink_to_fit();
        self.sub_a.shrink_to_fit();
        self.own_partial.shrink_to_fit();
    }
}

// -----------------------------------------------------------------------------
// Index
// -----------------------------------------------------------------------------

/// The in-memory index of one volume.
///
/// Built with [`crate::IndexBuilder`], updated live with
/// [`Index::upsert`]/[`Index::remove`], queried with the methods in
/// [`crate::query`] and [`crate::search`], persisted with
/// [`Index::save`](crate::cache)/[`Index::load`](crate::cache).
///
/// # Example
///
/// ```
/// use strata_core::*;
/// use strata_index::{IndexBuilder, IndexOptions};
///
/// let root = FileRef::from_parts(5, 5);
/// let rec = |id: FileRef, parent: FileRef, name: &str, dir: bool, size: u64| ScanRecord {
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
/// b.push(rec(FileRef::from_parts(40, 1), root, "a.bin", false, 4096))?;
/// b.push(rec(root, root, "", true, 0))?;
/// let index = b.finish()?;
/// let agg = index.aggregate(index.root()).unwrap();
/// assert_eq!(agg.allocated, 4096);
/// assert_eq!(agg.files, 1);
/// # Ok::<(), strata_index::IndexError>(())
/// ```
#[derive(Debug)]
pub struct Index {
    pub(crate) col: Columns,
    pub(crate) names: NameStore,
    pub(crate) exts: ExtTable,
    pub(crate) rows: Rows,
    /// CSR child storage for base directories (ranges in row order).
    pub(crate) children: Vec<u32>,
    /// Children beyond a row's CSR capacity, and all children of delta rows.
    pub(crate) extra: HashMap<u32, Vec<u32>>,
    /// Row of each non-base directory.
    pub(crate) delta_rows: HashMap<u32, u32>,
    pub(crate) free_rows: Vec<u32>,
    /// Entries `0..base_len` are base; `0..base_dirs` are base directories.
    pub(crate) base_len: u32,
    pub(crate) base_dirs: u32,
    /// Lookup key → delta entries carrying that key.
    pub(crate) delta_keys: HashMap<u64, Vec<u32>>,
    /// Link counts of records with more than one live link.
    pub(crate) link_counts: HashMap<u64, u32>,
    /// Recyclable delta slots.
    pub(crate) free: Vec<u32>,
    /// Entries whose display parent is not their real parent (orphans,
    /// broken cycles, grouped NTFS metadata): their intended parent ref.
    pub(crate) detached: HashMap<u32, u64>,
    /// Intended-parent key → detached entries waiting on it.
    pub(crate) pending: HashMap<u64, Vec<u32>>,
    pub(crate) root: u32,
    pub(crate) orphans: u32,
    pub(crate) metadata: u32,
    pub(crate) live: u32,
    pub(crate) opts: IndexOptions,
    pub(crate) path_cache: PathCache,
    pub(crate) limits: Limits,
}

/// Iterator over the children of a directory.
#[derive(Debug, Clone)]
pub struct Children<'a> {
    a: std::slice::Iter<'a, u32>,
    b: std::slice::Iter<'a, u32>,
}

impl Iterator for Children<'_> {
    type Item = EntryId;

    #[inline]
    fn next(&mut self) -> Option<EntryId> {
        self.a.next().or_else(|| self.b.next()).map(|&c| EntryId(c))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.a.len() + self.b.len();
        (n, Some(n))
    }
}

impl ExactSizeIterator for Children<'_> {}

/// Memory used by an index, by category (heap bytes, by capacity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MemoryReport {
    /// Entry slots (live plus tombstones and free slots).
    pub slots: u64,
    /// Live entries.
    pub live: u64,
    /// Directory rows.
    pub dir_rows: u64,
    /// Per-entry columns.
    pub columns: u64,
    /// Directory rows and child lists.
    pub tree: u64,
    /// Hash maps and other side tables.
    pub maps: u64,
    /// Name bookkeeping (sampled offsets, override map).
    pub name_index: u64,
    /// Raw name bytes (WTF-8 plus length prefixes), excluded from the budget.
    pub name_bytes: u64,
}

impl MemoryReport {
    /// Bytes excluding raw name bytes.
    #[must_use]
    pub const fn total_excluding_names(&self) -> u64 {
        self.columns + self.tree + self.maps + self.name_index
    }

    /// Average bytes per live entry, excluding raw name bytes (the budget
    /// is 64).
    #[must_use]
    pub fn bytes_per_entry(&self) -> f64 {
        if self.live == 0 {
            return 0.0;
        }
        self.total_excluding_names() as f64 / self.live as f64
    }
}

impl Index {
    // ---- identity and structure -------------------------------------------

    /// Options the index was built with.
    #[must_use]
    pub fn options(&self) -> &IndexOptions {
        &self.opts
    }

    /// Volume identity and USN position.
    #[must_use]
    pub fn volume(&self) -> &VolumeInfo {
        &self.opts.volume
    }

    /// Records the USN journal position the index reflects.
    pub fn set_usn_position(&mut self, usn_journal_id: u64, last_usn: i64) {
        self.opts.volume.usn_journal_id = usn_journal_id;
        self.opts.volume.last_usn = last_usn;
    }

    /// Changes the reference time used to flag suspicious timestamps in
    /// subsequent live updates.
    pub fn set_now(&mut self, now: FileTime) {
        self.opts.now = now;
    }

    /// Number of id slots (live entries, tombstones and free slots). Every
    /// valid [`EntryId`] is below this.
    #[must_use]
    pub fn slot_count(&self) -> usize {
        self.col.len()
    }

    /// Number of live entries, including virtual nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.live as usize
    }

    /// Whether the index has no live entries (never true after a build: the
    /// root always exists).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// The volume root.
    #[must_use]
    pub fn root(&self) -> EntryId {
        EntryId(self.root)
    }

    /// The virtual "Orphaned entries" directory under the root.
    #[must_use]
    pub fn orphans_node(&self) -> EntryId {
        EntryId(self.orphans)
    }

    /// The virtual "NTFS metadata" directory under the root.
    #[must_use]
    pub fn metadata_node(&self) -> EntryId {
        EntryId(self.metadata)
    }

    /// Whether `id` refers to a live entry.
    #[must_use]
    pub fn is_live(&self, id: EntryId) -> bool {
        self.live_u32(id.0)
    }

    #[inline]
    pub(crate) fn live_u32(&self, id: u32) -> bool {
        (id as usize) < self.col.len() && self.col.parent[id as usize] != DEAD
    }

    /// Display parent (`None` for the root and dead ids). Orphans, broken
    /// cycles and NTFS metadata files report their virtual group node; see
    /// [`Index::intended_parent`] for the on-disk parent.
    #[must_use]
    pub fn parent(&self, id: EntryId) -> Option<EntryId> {
        let p = *self.col.parent.get(id.index())?;
        (p != NONE && p != DEAD).then_some(EntryId(p))
    }

    /// On-disk parent reference of an entry displayed under a virtual group
    /// node (orphan, broken cycle, NTFS metadata). `None` for entries shown
    /// under their real parent, and for records without any name link.
    #[must_use]
    pub fn intended_parent(&self, id: EntryId) -> Option<FileRef> {
        self.detached
            .get(&id.0)
            .copied()
            .filter(|&r| r != NO_REF)
            .map(FileRef)
    }

    /// Flags of an entry. Directory flags include [`EntryFlags::PARTIAL`]
    /// when any part of the subtree is partial.
    #[must_use]
    pub fn flags(&self, id: EntryId) -> EntryFlags {
        self.col.flags(id.0)
    }

    /// Whether the entry is a directory (including virtual group nodes).
    #[must_use]
    pub fn is_dir(&self, id: EntryId) -> bool {
        self.col.has(id.0, EntryFlags::DIR)
    }

    /// File reference, or `None` for virtual nodes.
    #[must_use]
    pub fn file_ref(&self, id: EntryId) -> Option<FileRef> {
        let r = *self.col.file_ref.get(id.index())?;
        (r != NO_REF).then_some(FileRef(r))
    }

    /// Name as raw WTF-8 bytes: lossless, so unpaired UTF-16 surrogates survive.
    #[must_use]
    pub fn name_wtf8(&self, id: EntryId) -> &[u8] {
        self.names.get(id.0)
    }

    /// Name as lossless UTF-16.
    #[must_use]
    pub fn name(&self, id: EntryId) -> WideName {
        WideName::from_units(wtf8::decode(self.names.get(id.0)))
    }

    /// Display name (unpaired surrogates become U+FFFD).
    #[must_use]
    pub fn name_lossy(&self, id: EntryId) -> String {
        wtf8::to_string_lossy(self.names.get(id.0))
    }

    /// The record's own logical size (all streams), whatever its hardlink
    /// status.
    #[must_use]
    pub fn own_logical(&self, id: EntryId) -> u64 {
        self.col.logical(id.0)
    }

    /// The record's own allocated size (data, ADS, directory overhead).
    #[must_use]
    pub fn own_allocated(&self, id: EntryId) -> u64 {
        self.col.allocated(id.0)
    }

    /// The record's own size in `mode`.
    #[must_use]
    pub fn own_size(&self, id: EntryId, mode: SizeMode) -> u64 {
        self.col.own(id.0, mode)
    }

    /// Bytes this entry contributes to its ancestors' totals in `mode`: the
    /// own size, 0 for secondary hardlinks, or an even share in
    /// split-hardlink mode (the remainder goes to the primary path).
    #[must_use]
    pub fn contribution(&self, id: EntryId, mode: SizeMode) -> u64 {
        self.contrib(id.0, mode)
    }

    #[inline]
    pub(crate) fn contrib(&self, id: u32, mode: SizeMode) -> u64 {
        let own = self.col.own(id, mode);
        let secondary = self.col.has(id, EntryFlags::HARDLINK_SECONDARY);
        if !self.opts.split_hardlinks {
            return if secondary { 0 } else { own };
        }
        let n = u64::from(self.link_count_u32(id));
        if n <= 1 {
            return own;
        }
        own / n + if secondary { 0 } else { own % n }
    }

    /// Display size: subtree total for directories, contribution for files.
    #[must_use]
    pub fn size(&self, id: EntryId, mode: SizeMode) -> u64 {
        match self.row_of(id.0) {
            Some(r) => match mode {
                SizeMode::Allocated => self.rows.sub_a[r as usize],
                SizeMode::Logical => self.rows.sub_l[r as usize],
            },
            None => self.contrib(id.0, mode),
        }
    }

    /// Compact timestamps, or `None` in lite mode.
    #[must_use]
    pub fn times(&self, id: EntryId) -> Option<EntryTimes> {
        self.col.times(id.0)
    }

    /// Category assigned by the classifier ([`Category::Unknown`] until then).
    #[must_use]
    pub fn category(&self, id: EntryId) -> Category {
        Category::from_u16(self.col.category[id.index()])
    }

    /// Raw `category` column value.
    #[must_use]
    pub fn category_raw(&self, id: EntryId) -> u16 {
        self.col.category[id.index()]
    }

    /// Sets the classifier category.
    pub fn set_category(&mut self, id: EntryId, category: u16) {
        if let Some(c) = self.col.category.get_mut(id.index()) {
            *c = category;
        }
    }

    /// Owning app id (0 = unknown).
    #[must_use]
    pub fn owner_app(&self, id: EntryId) -> u32 {
        self.col.owner_app[id.index()]
    }

    /// Sets the owning app id.
    pub fn set_owner_app(&mut self, id: EntryId, app: u32) {
        if let Some(a) = self.col.owner_app.get_mut(id.index()) {
            *a = app;
        }
    }

    /// Interned extension id (0 = none).
    #[must_use]
    pub fn ext_id(&self, id: EntryId) -> u16 {
        self.col.ext_id[id.index()]
    }

    /// Lowercased extension of a file entry, if any.
    #[must_use]
    pub fn extension(&self, id: EntryId) -> Option<String> {
        let e = self.exts.name(self.col.ext_id[id.index()]);
        (!e.is_empty()).then(|| wtf8::to_string_lossy(e))
    }

    /// Extension text for an interned id.
    #[must_use]
    pub fn extension_name(&self, ext_id: u16) -> String {
        wtf8::to_string_lossy(self.exts.name(ext_id))
    }

    /// Id of a (case-insensitive) extension, if any entry uses it.
    #[must_use]
    pub fn extension_id(&self, ext: &str) -> Option<u16> {
        self.exts
            .lookup(&crate::fold::fold(ext.trim_start_matches('.').as_bytes()))
    }

    /// Subtree aggregate of a directory.
    #[must_use]
    pub fn aggregate(&self, id: EntryId) -> Option<DirAggregate> {
        if !self.is_live(id) {
            return None;
        }
        let r = self.row_of(id.0)? as usize;
        let opt = |v: u32| (v != NONE).then_some(EntryId(v));
        Some(DirAggregate {
            logical: self.rows.sub_l[r],
            allocated: self.rows.sub_a[r],
            files: self.rows.files[r],
            dirs: self.rows.dirs[r],
            newest: (self.rows.newest[r] != NO_NEWEST).then_some(EpochSecs(self.rows.newest[r])),
            oldest: (self.rows.oldest[r] != NO_OLDEST).then_some(EpochSecs(self.rows.oldest[r])),
            largest_allocated: opt(self.rows.largest_a[r]),
            largest_logical: opt(self.rows.largest_l[r]),
            partial: self.col.has(id.0, EntryFlags::PARTIAL),
        })
    }

    /// Children of a directory, in storage order (see
    /// [`Index::children_sorted`] for display order). Empty for files.
    #[must_use]
    pub fn children(&self, id: EntryId) -> Children<'_> {
        let (a, b) = match self.row_of(id.0) {
            Some(r) if self.is_live(id) => self.child_slices(r),
            _ => (&[][..], &[][..]),
        };
        Children {
            a: a.iter(),
            b: b.iter(),
        }
    }

    /// Number of direct children.
    #[must_use]
    pub fn child_count(&self, id: EntryId) -> usize {
        self.children(id).len()
    }

    /// Primary entry of a file reference (exact sequence match), if indexed.
    #[must_use]
    pub fn lookup(&self, file_ref: FileRef) -> Option<EntryId> {
        let mut best = None;
        self.for_each_key(ref_key(file_ref.0), |id| {
            if self.col.file_ref[id as usize] == file_ref.0
                && (best.is_none() || !self.col.has(id, EntryFlags::HARDLINK_SECONDARY))
            {
                best = Some(EntryId(id));
            }
        });
        best
    }

    /// Every live entry (hardlink path) of a file reference, primary first.
    #[must_use]
    pub fn links(&self, file_ref: FileRef) -> Vec<EntryId> {
        let mut v = self.links_of_key(ref_key(file_ref.0));
        v.retain(|&id| self.col.file_ref[id as usize] == file_ref.0);
        v.into_iter().map(EntryId).collect()
    }

    /// Number of live links of the entry's record (1 for ordinary files).
    #[must_use]
    pub fn link_count(&self, id: EntryId) -> u32 {
        self.link_count_u32(id.0)
    }

    #[inline]
    pub(crate) fn link_count_u32(&self, id: u32) -> u32 {
        if self.link_counts.is_empty() {
            return 1;
        }
        let r = self.col.file_ref[id as usize];
        if r == NO_REF {
            return 1;
        }
        self.link_counts.get(&ref_key(r)).copied().unwrap_or(1)
    }

    /// Heap memory by category.
    #[must_use]
    pub fn memory_report(&self) -> MemoryReport {
        let (cols, big) = self.col.heap_bytes();
        let (name_bytes, name_index) = self.names.heap_bytes();
        let extra: u64 = self.extra.values().map(|v| vec_bytes(v) + 24).sum::<u64>()
            + map_bytes(self.extra.capacity(), 28);
        let tree = self.rows.heap_bytes() + vec_bytes(&self.children) + extra;
        let vec_map = |m: &HashMap<u64, Vec<u32>>| {
            m.values().map(vec_bytes).sum::<u64>() + map_bytes(m.capacity(), 32)
        };
        let maps = big
            + map_bytes(self.delta_rows.capacity(), 8)
            + vec_bytes(&self.free_rows)
            + vec_map(&self.delta_keys)
            + map_bytes(self.link_counts.capacity(), 12)
            + vec_bytes(&self.free)
            + map_bytes(self.detached.capacity(), 12)
            + vec_map(&self.pending)
            + self.exts.heap_bytes();
        MemoryReport {
            slots: self.col.len() as u64,
            live: u64::from(self.live),
            dir_rows: self.rows.len() as u64,
            columns: cols,
            tree,
            maps,
            name_index,
            name_bytes,
        }
    }

    // ---- internal structure helpers ---------------------------------------

    /// Row of a directory entry.
    #[inline]
    pub(crate) fn row_of(&self, id: u32) -> Option<u32> {
        if (id as usize) >= self.col.len() || !self.col.has(id, EntryFlags::DIR) {
            return None;
        }
        if id < self.base_dirs {
            Some(id)
        } else {
            self.delta_rows.get(&id).copied()
        }
    }

    /// CSR capacity of a row.
    #[inline]
    pub(crate) fn row_cap(&self, r: u32) -> u32 {
        if r >= self.base_dirs {
            return 0;
        }
        let end = if r + 1 < self.base_dirs {
            self.rows.start[r as usize + 1]
        } else {
            self.children.len() as u32
        };
        end - self.rows.start[r as usize]
    }

    #[inline]
    pub(crate) fn child_slices(&self, r: u32) -> (&[u32], &[u32]) {
        let s = self.rows.start[r as usize] as usize;
        let l = self.rows.len[r as usize] as usize;
        let a = &self.children[s..s + l];
        let b = self.extra.get(&r).map_or(&[][..], Vec::as_slice);
        (a, b)
    }

    /// Calls `f` for every live entry whose lookup key is `key`.
    pub(crate) fn for_each_key(&self, key: u64, mut f: impl FnMut(u32)) {
        let refs = &self.col.file_ref;
        for (lo, hi) in [(0, self.base_dirs), (self.base_dirs, self.base_len)] {
            let part = &refs[lo as usize..hi as usize];
            let mut i = part.partition_point(|&r| ref_key(r) < key);
            while i < part.len() && ref_key(part[i]) == key {
                let id = lo + i as u32;
                if self.col.parent[id as usize] != DEAD {
                    f(id);
                }
                i += 1;
            }
        }
        if let Some(v) = self.delta_keys.get(&key) {
            for &id in v {
                f(id);
            }
        }
    }

    /// Live entries with lookup key `key`, primary first, then by id.
    pub(crate) fn links_of_key(&self, key: u64) -> Vec<u32> {
        let mut v = Vec::new();
        self.for_each_key(key, |id| v.push(id));
        v.sort_by_key(|&id| (self.col.has(id, EntryFlags::HARDLINK_SECONDARY), id));
        v
    }

    /// The entry a child link with parent reference `p` attaches to, if `p`
    /// names a live, non-virtual directory with a matching sequence number
    /// that may be descended into (reparse points are leaves).
    pub(crate) fn valid_parent(&self, p: u64) -> Option<u32> {
        if p == NO_REF {
            return None;
        }
        let mut primary = None;
        self.for_each_key(ref_key(p), |id| {
            if primary.is_none() || !self.col.has(id, EntryFlags::HARDLINK_SECONDARY) {
                primary = Some(id);
            }
        });
        let q = primary?;
        let f = self.col.flags(q);
        (self.col.file_ref[q as usize] == p
            && f.contains(EntryFlags::DIR)
            && !f.contains(EntryFlags::VIRTUAL)
            && !f.reparse().blocks_traversal())
        .then_some(q)
    }

    /// Whether a directory may hold children found on disk.
    pub(crate) fn can_parent(&self, id: u32) -> bool {
        let f = self.col.flags(id);
        f.contains(EntryFlags::DIR)
            && !f.contains(EntryFlags::VIRTUAL)
            && !f.reparse().blocks_traversal()
    }
}
