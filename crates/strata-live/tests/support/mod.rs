//! Test support: an in-memory volume model that journals its changes the
//! way NTFS does, fake journal and record sources backed by it, a manual
//! clock, and a canonical (id-independent) form of an index.

#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::time::{Duration, Instant};

use strata_core::{
    AdsInfo, EntryFlags, FileRef, FileTime, NameLink, Reparse, ScanRecord, SizeMode, Sizes, Times,
    WideName,
};
use strata_index::{EntryId, Index, IndexBuilder, IndexOptions};
use strata_live::{
    Clock, Fetched, Halt, IndexAccess, JournalInfo, JournalSource, LiveEvent, RecordSource,
    SourceError, TailStats, Tailer, TailerConfig, index_position,
};
use strata_ntfs::usn::{self, FileId128, UsnChange, UsnExtent, UsnRange, encode};

pub const ROOT_REC: u64 = 5;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x800;
const FILE_ATTRIBUTE_ENCRYPTED: u32 = 0x4000;
const REPARSE_TAGS: [u32; 3] = [0xA000_000C, 0xA000_0003, 0x8000_0013];

/// A "now" in 2025; model times stay well before it.
pub fn now() -> FileTime {
    FileTime::from_unix_secs(1_760_000_000)
}

pub fn opts() -> IndexOptions {
    IndexOptions {
        now: now(),
        ..IndexOptions::default()
    }
}

// -----------------------------------------------------------------------------
// Volume model
// -----------------------------------------------------------------------------

/// Which record versions the model's journal writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Versions {
    V2,
    V3,
    Mixed,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub seq: u16,
    pub dir: bool,
    /// (parent record, name); the first link is the primary one.
    pub links: Vec<(u64, String)>,
    pub logical: u64,
    pub ads: Vec<(String, u64)>,
    pub reparse: Option<u32>,
    pub hidden: bool,
    pub compressed: bool,
    pub encrypted: bool,
    pub mtime: i64,
}

#[derive(Debug)]
pub struct Model {
    pub nodes: BTreeMap<u64, Node>,
    last_seq: HashMap<u64, u16>,
    free: Vec<u64>,
    next_record: u64,
    /// (usn, encoded record), ascending.
    pub journal: Vec<(i64, Vec<u8>)>,
    pub first_usn: i64,
    pub next_usn: i64,
    pub journal_id: u64,
    pub disabled: bool,
    /// Accumulated reason bits of files with an open handle.
    pub open: BTreeMap<u64, u32>,
    pub versions: Versions,
    pub ranges: bool,
    emitted: u64,
    clock: i64,
    names: u64,
    /// Fetch accounting.
    pub fetch_calls: u64,
    pub fetched_refs: u64,
    pub fetch_log: Vec<FileRef>,
    /// Failure injected into the next source call.
    pub fail_read: Option<SourceError>,
    pub fail_fetch: Option<SourceError>,
    child_cache: RefCell<Option<(i64, HashMap<u64, u64>)>>,
}

impl Model {
    pub fn new(versions: Versions) -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            ROOT_REC,
            Node {
                seq: 5,
                dir: true,
                links: vec![(ROOT_REC, String::new())],
                logical: 0,
                ads: vec![],
                reparse: None,
                hidden: false,
                compressed: false,
                encrypted: false,
                mtime: 1_600_000_000,
            },
        );
        Self {
            nodes,
            last_seq: HashMap::new(),
            free: Vec::new(),
            next_record: 64,
            journal: Vec::new(),
            first_usn: 0,
            next_usn: 0,
            journal_id: 0x1D0_0000_0000_0001,
            disabled: false,
            open: BTreeMap::new(),
            versions,
            ranges: false,
            emitted: 0,
            clock: 1_650_000_000,
            names: 0,
            fetch_calls: 0,
            fetched_refs: 0,
            fetch_log: Vec::new(),
            fail_read: None,
            fail_fetch: None,
            child_cache: RefCell::new(None),
        }
    }

    pub fn file_ref(&self, rec: u64) -> FileRef {
        FileRef::from_parts(rec, self.nodes[&rec].seq)
    }

    pub fn info(&self) -> Option<JournalInfo> {
        (!self.disabled).then_some(JournalInfo {
            journal_id: self.journal_id,
            first_usn: self.first_usn,
            next_usn: self.next_usn,
            lowest_valid_usn: self.first_usn,
            max_usn: i64::MAX,
        })
    }

    /// Purges journal records before `usn` (the journal wrapped).
    pub fn purge_before(&mut self, usn: i64) {
        self.journal.retain(|(u, _)| *u >= usn);
        self.first_usn = usn.min(self.next_usn);
    }

    /// Deletes and recreates the journal.
    pub fn recreate_journal(&mut self) {
        self.journal.clear();
        self.journal_id += 1;
        self.first_usn = self.next_usn;
    }

    // -------------------------------------------------------------------------
    // Records
    // -------------------------------------------------------------------------

    fn child_counts(&self) -> HashMap<u64, u64> {
        let mut m: HashMap<u64, u64> = HashMap::new();
        for (&rec, n) in &self.nodes {
            for (p, _) in &n.links {
                if *p != rec {
                    *m.entry(*p).or_default() += 1;
                }
            }
        }
        m
    }

    fn children_of(&self, dir: u64) -> u64 {
        // Every mutation journals at least one record, so the journal head
        // versions the cached counts.
        let mut cache = self.child_cache.borrow_mut();
        if cache.as_ref().is_none_or(|(v, _)| *v != self.next_usn) {
            *cache = Some((self.next_usn, self.child_counts()));
        }
        cache
            .as_ref()
            .and_then(|(_, m)| m.get(&dir).copied())
            .unwrap_or(0)
    }

    fn to_record(&self, rec: u64, children: u64) -> ScanRecord {
        let n = &self.nodes[&rec];
        let mut attributes = FILE_ATTRIBUTE_ARCHIVE;
        if n.dir {
            attributes = FILE_ATTRIBUTE_DIRECTORY;
        }
        if n.hidden {
            attributes |= FILE_ATTRIBUTE_HIDDEN;
        }
        if n.compressed {
            attributes |= FILE_ATTRIBUTE_COMPRESSED;
        }
        if n.encrypted {
            attributes |= FILE_ATTRIBUTE_ENCRYPTED;
        }
        if n.reparse.is_some() {
            attributes |= FILE_ATTRIBUTE_REPARSE_POINT;
        }
        let mut flags = EntryFlags::from_win32_attributes(attributes);
        if n.dir {
            flags |= EntryFlags::DIR;
        }
        let allocated = if n.compressed {
            (n.logical / 2).next_multiple_of(4096)
        } else {
            n.logical.next_multiple_of(4096)
        };
        let t = FileTime::from_unix_secs(n.mtime);
        ScanRecord {
            id: FileRef::from_parts(rec, n.seq),
            links: n
                .links
                .iter()
                .map(|(p, name)| NameLink {
                    parent: self.file_ref(*p),
                    name: WideName::from_str_lossless(name),
                })
                .collect(),
            attributes,
            flags,
            times: Times {
                created: FileTime::from_unix_secs(1_600_000_000),
                modified: t,
                accessed: t,
                changed: t,
            },
            fn_created: None,
            sizes: Sizes {
                logical: n.logical,
                allocated,
                ads_logical: n.ads.iter().map(|a| a.1).sum(),
                ads_allocated: n.ads.iter().map(|a| a.1.next_multiple_of(4096)).sum(),
                // Grows and shrinks with the name count, like an $I30 index,
                // and is never journaled for the directory itself.
                dir_overhead: if n.dir { 4096 * (children / 3) } else { 0 },
                attr_overhead: 0,
            },
            reparse: n.reparse.map(|tag| Reparse { tag, target: None }),
            ads: n
                .ads
                .iter()
                .map(|(name, size)| AdsInfo {
                    name: WideName::from_str_lossless(name),
                    logical: *size,
                    allocated: size.next_multiple_of(4096),
                })
                .collect(),
        }
    }

    /// Current record of `r`, or `None` if it no longer exists.
    pub fn record(&self, r: FileRef) -> Option<ScanRecord> {
        let n = self.nodes.get(&r.record())?;
        (n.seq == r.sequence()).then(|| {
            let kids = if n.dir {
                self.children_of(r.record())
            } else {
                0
            };
            self.to_record(r.record(), kids)
        })
    }

    /// Every record, as a full scan would emit them.
    pub fn records(&self) -> Vec<ScanRecord> {
        let counts = self.child_counts();
        self.nodes
            .keys()
            .map(|&rec| self.to_record(rec, counts.get(&rec).copied().unwrap_or(0)))
            .collect()
    }

    pub fn build_index(&self) -> Index {
        let mut b = IndexBuilder::new(opts());
        b.push_batch(self.records()).expect("push");
        b.finish().expect("finish")
    }

    // -------------------------------------------------------------------------
    // Journal writing
    // -------------------------------------------------------------------------

    fn emit_for(&mut self, file: FileRef, parent: FileRef, name: &str, reason: u32) {
        self.emitted += 1;
        let major = match self.versions {
            Versions::V2 => 2,
            Versions::V3 => 3,
            Versions::Mixed => 2 + (self.emitted % 2) as u16,
        };
        let c = UsnChange {
            major_version: major,
            file: FileId128(u128::from(file.0)),
            parent: FileId128(u128::from(parent.0)),
            usn: self.next_usn,
            timestamp: FileTime::from_unix_secs(self.clock),
            reason,
            source_info: 0,
            security_id: 0,
            attributes: 0,
            name: WideName::from_str_lossless(name),
        };
        let bytes = encode::change(&c);
        self.push_raw(bytes);
    }

    fn push_raw(&mut self, bytes: Vec<u8>) {
        let usn = self.next_usn;
        self.next_usn += bytes.len() as i64;
        self.journal.push((usn, bytes));
    }

    fn emit(&mut self, rec: u64, link: usize, reason: u32) {
        let n = &self.nodes[&rec];
        let (p, name) = n.links[link.min(n.links.len() - 1)].clone();
        let file = self.file_ref(rec);
        let parent = self.file_ref(p);
        self.emit_for(file, parent, &name, reason);
    }

    fn emit_range(&mut self, rec: u64) {
        let file = self.file_ref(rec);
        let parent = self.file_ref(self.nodes[&rec].links[0].0);
        let r = UsnRange {
            file: FileId128(u128::from(file.0)),
            parent: FileId128(u128::from(parent.0)),
            usn: self.next_usn,
            reason: usn::USN_REASON_DATA_OVERWRITE,
            source_info: 0,
            remaining_extents: 0,
            extents: vec![UsnExtent {
                offset: 0,
                length: 4096,
            }],
        };
        self.push_raw(encode::range(&r));
    }

    /// Adds `bit` to the file's open reasons, journaling it if new, and
    /// closes the handle unless `keep_open`.
    fn change(&mut self, rec: u64, bit: u32, keep_open: bool) {
        let cur = self.open.get(&rec).copied().unwrap_or(0);
        let next = cur | bit;
        if next != cur {
            self.emit(rec, 0, next);
        }
        if keep_open {
            self.open.insert(rec, next);
        } else {
            self.open.remove(&rec);
            self.emit(rec, 0, next | usn::USN_REASON_CLOSE);
        }
    }

    /// Namespace changes are always journaled, whatever is already set.
    fn namespace(&mut self, rec: u64, link: usize, bit: u32, keep_open: bool) {
        let next = self.open.get(&rec).copied().unwrap_or(0) | bit;
        self.emit(rec, link, next);
        if keep_open {
            self.open.insert(rec, next);
        } else {
            self.open.remove(&rec);
            self.emit(rec, link, next | usn::USN_REASON_CLOSE);
        }
    }

    pub fn close(&mut self, rec: u64) {
        if let Some(bits) = self.open.remove(&rec) {
            self.emit(rec, 0, bits | usn::USN_REASON_CLOSE);
        }
    }

    pub fn close_all(&mut self) {
        let open: Vec<u64> = self.open.keys().copied().collect();
        for rec in open {
            self.close(rec);
        }
    }

    // -------------------------------------------------------------------------
    // Pickers
    // -------------------------------------------------------------------------

    fn tick_clock(&mut self) -> i64 {
        self.clock += 7;
        self.clock
    }

    fn name(&mut self, prefix: &str) -> String {
        self.names += 1;
        format!("{prefix}{}", self.names)
    }

    fn pick(&self, k: u16, pred: impl Fn(u64, &Node) -> bool) -> Option<u64> {
        let v: Vec<u64> = self
            .nodes
            .iter()
            .filter(|(r, n)| **r != ROOT_REC && pred(**r, n))
            .map(|(r, _)| *r)
            .collect();
        (!v.is_empty()).then(|| v[usize::from(k) % v.len()])
    }

    /// A directory that can hold new names (not a reparse point).
    fn pick_dir(&self, k: u16) -> u64 {
        let v: Vec<u64> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.dir && n.reparse.is_none())
            .map(|(r, _)| *r)
            .collect();
        v[usize::from(k) % v.len()]
    }

    fn is_ancestor_or_self(&self, a: u64, mut d: u64) -> bool {
        loop {
            if d == a {
                return true;
            }
            if d == ROOT_REC {
                return false;
            }
            d = self.nodes[&d].links[0].0;
        }
    }

    fn alloc(&mut self) -> (u64, u16) {
        if let Some(rec) = self.free.pop() {
            let seq = self
                .last_seq
                .get(&rec)
                .copied()
                .unwrap_or(1)
                .wrapping_add(1)
                .max(1);
            (rec, seq)
        } else {
            let rec = self.next_record;
            self.next_record += 1;
            (rec, 1)
        }
    }

    // -------------------------------------------------------------------------
    // Operations (each journals exactly what NTFS would)
    // -------------------------------------------------------------------------

    pub fn create(&mut self, dir_k: u16, is_dir: bool, size: u32, keep_open: bool) -> u64 {
        let parent = self.pick_dir(dir_k);
        self.create_in(parent, is_dir, size, keep_open)
    }

    pub fn create_in(&mut self, parent: u64, is_dir: bool, size: u32, keep_open: bool) -> u64 {
        let (rec, seq) = self.alloc();
        let name = self.name(if is_dir { "d" } else { "f" });
        let mtime = self.tick_clock();
        self.nodes.insert(
            rec,
            Node {
                seq,
                dir: is_dir,
                links: vec![(parent, name)],
                logical: if is_dir { 0 } else { u64::from(size) },
                ads: vec![],
                reparse: None,
                hidden: false,
                compressed: false,
                encrypted: false,
                mtime,
            },
        );
        self.namespace(rec, 0, usn::USN_REASON_FILE_CREATE, keep_open);
        if !is_dir && size > 0 {
            self.change(rec, usn::USN_REASON_DATA_EXTEND, keep_open);
        }
        rec
    }

    pub fn delete_rec(&mut self, rec: u64) {
        let kids: Vec<u64> = self
            .nodes
            .iter()
            .filter(|(r, n)| **r != rec && n.links.iter().any(|(p, _)| *p == rec))
            .map(|(r, _)| *r)
            .collect();
        for k in kids {
            // A child hardlinked elsewhere only loses its names here.
            while let Some(n) = self.nodes.get(&k) {
                let Some(i) = n.links.iter().position(|(p, _)| *p == rec) else {
                    break;
                };
                if n.links.len() > 1 {
                    self.unlink(k, i);
                } else {
                    self.delete_rec(k);
                }
            }
        }
        let bits = self.open.remove(&rec).unwrap_or(0);
        self.emit(
            rec,
            0,
            bits | usn::USN_REASON_FILE_DELETE | usn::USN_REASON_CLOSE,
        );
        let n = self.nodes.remove(&rec).expect("node");
        self.last_seq.insert(rec, n.seq);
        self.free.push(rec);
    }

    fn unlink(&mut self, rec: u64, i: usize) {
        let bits = self.open.remove(&rec).unwrap_or(0) | usn::USN_REASON_HARD_LINK_CHANGE;
        self.emit(rec, i, bits);
        self.emit(rec, i, bits | usn::USN_REASON_CLOSE);
        self.nodes.get_mut(&rec).expect("node").links.remove(i);
    }

    pub fn delete(&mut self, k: u16) {
        let Some(rec) = self.pick(k, |_, _| true) else {
            return;
        };
        if self.nodes[&rec].links.len() > 1 {
            self.unlink(rec, usize::from(k) % self.nodes[&rec].links.len());
        } else {
            self.delete_rec(rec);
        }
    }

    /// Renames or moves one link: an old-name record, then the new-name
    /// record (and a close). Where the halves split across reads is up to
    /// the reader's buffer size.
    pub fn rename(&mut self, k: u16, dir_k: u16, same_dir: bool, keep_open: bool) {
        let Some(rec) = self.pick(k, |_, _| true) else {
            return;
        };
        let li = usize::from(k) % self.nodes[&rec].links.len();
        let target = if same_dir {
            self.nodes[&rec].links[li].0
        } else {
            self.pick_dir(dir_k)
        };
        self.rename_link(rec, li, target, keep_open);
    }

    pub fn rename_link(&mut self, rec: u64, li: usize, target: u64, keep_open: bool) {
        if self.nodes[&rec].dir && self.is_ancestor_or_self(rec, target) {
            return;
        }
        let bits = self.open.get(&rec).copied().unwrap_or(0);
        self.emit(rec, li, bits | usn::USN_REASON_RENAME_OLD_NAME);
        let name = self.name("r");
        self.nodes.get_mut(&rec).expect("node").links[li] = (target, name);
        self.namespace(rec, li, usn::USN_REASON_RENAME_NEW_NAME, keep_open);
    }

    pub fn write(&mut self, k: u16, size: u32, keep_open: bool, range: bool) {
        let Some(rec) = self.pick(k, |_, n| !n.dir) else {
            return;
        };
        self.write_rec(rec, size, keep_open, range);
    }

    pub fn write_rec(&mut self, rec: u64, size: u32, keep_open: bool, range: bool) {
        let old = self.nodes[&rec].logical;
        let new = u64::from(size);
        let bit = match new.cmp(&old) {
            std::cmp::Ordering::Greater => usn::USN_REASON_DATA_EXTEND,
            std::cmp::Ordering::Less => usn::USN_REASON_DATA_TRUNCATION,
            std::cmp::Ordering::Equal => usn::USN_REASON_DATA_OVERWRITE,
        };
        let mtime = self.tick_clock();
        let n = self.nodes.get_mut(&rec).expect("node");
        n.logical = new;
        n.mtime = mtime;
        if range && self.ranges {
            self.emit_range(rec);
        }
        self.change(rec, bit, keep_open);
    }

    pub fn link_add(&mut self, k: u16, dir_k: u16) {
        let Some(rec) = self.pick(k, |_, n| !n.dir && n.links.len() < 5) else {
            return;
        };
        let dir = self.pick_dir(dir_k);
        let name = self.name("h");
        self.nodes
            .get_mut(&rec)
            .expect("node")
            .links
            .push((dir, name));
        let li = self.nodes[&rec].links.len() - 1;
        self.namespace(rec, li, usn::USN_REASON_HARD_LINK_CHANGE, false);
    }

    pub fn link_remove(&mut self, k: u16) {
        let Some(rec) = self.pick(k, |_, n| n.links.len() > 1) else {
            return;
        };
        let i = usize::from(k) % self.nodes[&rec].links.len();
        self.unlink(rec, i);
    }

    pub fn ads(&mut self, k: u16, stream: u8, size: u32) {
        let Some(rec) = self.pick(k, |_, _| true) else {
            return;
        };
        let name = format!("s{}", stream % 3);
        let n = self.nodes.get_mut(&rec).expect("node");
        let pos = n.ads.iter().position(|a| a.0 == name);
        let bit = match (pos, size) {
            (Some(i), 0) => {
                n.ads.remove(i);
                usn::USN_REASON_STREAM_CHANGE
            }
            (Some(i), s) => {
                n.ads[i].1 = u64::from(s);
                usn::USN_REASON_NAMED_DATA_EXTEND
            }
            (None, 0) => return,
            (None, s) => {
                n.ads.push((name, u64::from(s)));
                usn::USN_REASON_STREAM_CHANGE | usn::USN_REASON_NAMED_DATA_EXTEND
            }
        };
        self.change(rec, bit, false);
    }

    pub fn reparse(&mut self, k: u16) {
        let Some(rec) = self.pick(k, |_, _| true) else {
            return;
        };
        if self.nodes[&rec].dir && self.children_of(rec) > 0 {
            return;
        }
        let n = self.nodes.get_mut(&rec).expect("node");
        n.reparse = match n.reparse {
            Some(_) => None,
            None => Some(REPARSE_TAGS[usize::from(k) % REPARSE_TAGS.len()]),
        };
        self.change(rec, usn::USN_REASON_REPARSE_POINT_CHANGE, false);
    }

    pub fn basic(&mut self, k: u16) {
        let Some(rec) = self.pick(k, |_, _| true) else {
            return;
        };
        let mtime = self.tick_clock();
        let n = self.nodes.get_mut(&rec).expect("node");
        n.hidden = !n.hidden;
        n.mtime = mtime;
        self.change(rec, usn::USN_REASON_BASIC_INFO_CHANGE, false);
    }

    pub fn compress(&mut self, k: u16) {
        let Some(rec) = self.pick(k, |_, n| !n.encrypted) else {
            return;
        };
        let n = self.nodes.get_mut(&rec).expect("node");
        n.compressed = !n.compressed;
        self.change(rec, usn::USN_REASON_COMPRESSION_CHANGE, false);
    }

    pub fn encrypt(&mut self, k: u16) {
        let Some(rec) = self.pick(k, |_, n| !n.dir && !n.compressed) else {
            return;
        };
        let n = self.nodes.get_mut(&rec).expect("node");
        n.encrypted = !n.encrypted;
        self.change(rec, usn::USN_REASON_ENCRYPTION_CHANGE, false);
    }

    /// A security descriptor change: journaled, but nothing the index stores.
    pub fn security(&mut self, k: u16) {
        if let Some(rec) = self.pick(k, |_, _| true) {
            self.change(rec, usn::USN_REASON_SECURITY_CHANGE, false);
        }
    }

    /// Appends raw record bytes to the journal.
    pub fn push_bytes(&mut self, bytes: Vec<u8>) {
        self.push_raw(bytes);
    }

    /// Journals a record for `file` with `reason`, named under the root.
    pub fn emit_raw(&mut self, file: FileRef, reason: u32) {
        let root = self.file_ref(ROOT_REC);
        self.emit_for(file, root, "raw", reason);
    }

    /// A late, duplicate close record for a file without an open handle.
    pub fn dup_close(&mut self, k: u16) {
        if let Some(rec) = self.pick(k, |r, _| !self.open.contains_key(&r)) {
            self.emit(rec, 0, usn::USN_REASON_DATA_EXTEND | usn::USN_REASON_CLOSE);
        }
    }

    /// A delete of a reference the index never saw.
    pub fn ghost_delete(&mut self, k: u16) {
        let file = FileRef::from_parts(1_000_000 + u64::from(k), 3);
        let root = self.file_ref(ROOT_REC);
        self.emit_for(
            file,
            root,
            "ghost.tmp",
            usn::USN_REASON_FILE_CREATE | usn::USN_REASON_FILE_DELETE | usn::USN_REASON_CLOSE,
        );
    }

    pub fn close_pick(&mut self, k: u16) {
        let open: Vec<u64> = self.open.keys().copied().collect();
        if !open.is_empty() {
            self.close(open[usize::from(k) % open.len()]);
        }
    }
}

// -----------------------------------------------------------------------------
// Fake sources
// -----------------------------------------------------------------------------

pub type Shared = Rc<RefCell<Model>>;

/// Journal reader over the model, splitting reads at `max_bytes`.
pub struct FakeJournal {
    pub model: Shared,
    pub max_bytes: Rc<Cell<usize>>,
    /// Records at or after this USN are not written yet.
    pub hide_from: Option<i64>,
    pub reads: u64,
    pub waits: Vec<Option<Duration>>,
}

impl FakeJournal {
    pub fn new(model: Shared) -> Self {
        Self {
            model,
            max_bytes: Rc::new(Cell::new(1 << 16)),
            hide_from: None,
            reads: 0,
            waits: Vec::new(),
        }
    }
}

impl JournalSource for FakeJournal {
    fn query(&mut self) -> Result<Option<JournalInfo>, SourceError> {
        Ok(self.model.borrow().info())
    }

    fn read(
        &mut self,
        journal_id: u64,
        from_usn: i64,
        wait: Option<Duration>,
    ) -> Result<Vec<u8>, SourceError> {
        self.reads += 1;
        self.waits.push(wait);
        let mut m = self.model.borrow_mut();
        if let Some(e) = m.fail_read.take() {
            return Err(e);
        }
        if m.disabled {
            return Err(SourceError::JournalInactive);
        }
        if journal_id != m.journal_id {
            return Err(SourceError::JournalReset);
        }
        if from_usn < m.first_usn {
            return Err(SourceError::UsnPurged);
        }
        let start = m.journal.partition_point(|(u, _)| *u < from_usn);
        let end = self.hide_from.map_or(m.journal.len(), |h| {
            m.journal.partition_point(|(u, _)| *u < h)
        });
        let max = self.max_bytes.get();
        let mut out: Vec<Vec<u8>> = Vec::new();
        let mut bytes = 0;
        let mut i = start;
        while i < end && (out.is_empty() || bytes + m.journal[i].1.len() <= max) {
            bytes += m.journal[i].1.len();
            out.push(m.journal[i].1.clone());
            i += 1;
        }
        let head = self.hide_from.map_or(m.next_usn, |h| h.min(m.next_usn));
        let next = m
            .journal
            .get(i)
            .filter(|_| i < end)
            .map_or(head, |(u, _)| *u);
        Ok(encode::buffer(next, &out))
    }
}

/// Record source over the model.
pub struct FakeRecords {
    pub model: Shared,
}

impl RecordSource for FakeRecords {
    fn fetch(&mut self, refs: &[FileRef]) -> Result<Fetched, SourceError> {
        let mut m = self.model.borrow_mut();
        if let Some(e) = m.fail_fetch.take() {
            return Err(e);
        }
        m.fetch_calls += 1;
        m.fetched_refs += refs.len() as u64;
        m.fetch_log.extend_from_slice(refs);
        let mut out = Fetched::default();
        for &r in refs {
            match m.record(r) {
                Some(rec) => out.records.push(rec),
                None => out.missing.push(r),
            }
        }
        Ok(out)
    }
}

/// Clock advanced by hand.
#[derive(Debug, Clone)]
pub struct ManualClock {
    base: Instant,
    offset: Rc<Cell<Duration>>,
}

impl Default for ManualClock {
    fn default() -> Self {
        Self {
            base: Instant::now(),
            offset: Rc::new(Cell::new(Duration::ZERO)),
        }
    }
}

impl ManualClock {
    pub fn advance(&self, d: Duration) {
        self.offset.set(self.offset.get() + d);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.base + self.offset.get()
    }
}

// -----------------------------------------------------------------------------
// Harness
// -----------------------------------------------------------------------------

/// A model, an index built from it, and a tailer between them.
pub struct Harness {
    pub model: Shared,
    pub journal: FakeJournal,
    pub records: FakeRecords,
    pub index: Index,
    pub tailer: Tailer,
    pub clock: ManualClock,
    pub events: Vec<LiveEvent>,
    pub cfg: TailerConfig,
    /// Stats of tailers replaced by restarts.
    pub carried: TailStats,
}

impl Harness {
    /// Scans the model now; tailing replays from `start_usn` (which may lie
    /// before the scan, as when the journal is queried before scanning).
    pub fn new(model: Model, cfg: TailerConfig, start_usn: Option<i64>) -> Self {
        let model = Rc::new(RefCell::new(model));
        let mut index = model.borrow().build_index();
        let (id, next) = {
            let m = model.borrow();
            (m.journal_id, m.next_usn)
        };
        index.set_usn_position(id, start_usn.unwrap_or(next));
        let mut journal = FakeJournal::new(model.clone());
        let tailer =
            Tailer::start(cfg.clone(), &mut journal, index_position(&index)).expect("start");
        Self {
            records: FakeRecords {
                model: model.clone(),
            },
            journal,
            model,
            index,
            tailer,
            clock: ManualClock::default(),
            events: Vec::new(),
            cfg,
            carried: TailStats::default(),
        }
    }

    pub fn step(&mut self) -> Result<(), Halt> {
        let events = &mut self.events;
        self.tailer.step(
            &mut self.journal,
            &mut self.records,
            &mut self.index as &mut dyn IndexAccess,
            &self.clock,
            &mut |e| events.push(e),
        )
    }

    /// Lets time pass and runs one step.
    pub fn tick(&mut self, ms: u64) -> Result<(), Halt> {
        self.clock.advance(Duration::from_millis(ms));
        self.step()
    }

    /// Closes every handle and runs until the journal is consumed and
    /// nothing is pending.
    pub fn settle(&mut self) -> Result<(), Halt> {
        self.model.borrow_mut().close_all();
        let tick = self.cfg.tick.as_millis() as u64 + 1;
        for _ in 0..100_000 {
            self.tick(tick)?;
            let done = self.tailer.read_position() == self.model.borrow().next_usn
                && self.tailer.pending() == 0;
            if done {
                return Ok(());
            }
        }
        panic!("tailer did not settle");
    }

    /// Simulates an app restart from a cache image.
    pub fn restart(&mut self) {
        let pos = self.tailer.position();
        self.index.set_usn_position(pos.journal_id, pos.usn);
        let bytes = self.index.to_bytes();
        let s = self.tailer.stats();
        self.carried.records += s.records;
        self.carried.ignored += s.ignored;
        self.carried.paired_renames += s.paired_renames;
        self.carried.fetched += s.fetched;
        self.index = Index::from_bytes(&bytes).expect("cache round trip");
        self.tailer = Tailer::start(
            self.cfg.clone(),
            &mut self.journal,
            index_position(&self.index),
        )
        .expect("restart");
    }

    /// Stats summed over restarts.
    pub fn total_stats(&self) -> TailStats {
        let s = self.tailer.stats();
        TailStats {
            records: s.records + self.carried.records,
            ignored: s.ignored + self.carried.ignored,
            paired_renames: s.paired_renames + self.carried.paired_renames,
            fetched: s.fetched + self.carried.fetched,
            ..s
        }
    }

    pub fn assert_matches_fresh_scan(&self) {
        self.index.check_invariants().expect("invariants");
        let live = canonical(&self.index);
        let fresh = canonical(&self.model.borrow().build_index());
        if let Some(d) = diff(&live, &fresh) {
            panic!("live index differs from a fresh scan:\n{d}");
        }
    }

    pub fn ticks(&self) -> Vec<&strata_live::TickReport> {
        self.events
            .iter()
            .filter_map(|e| match e {
                LiveEvent::Tick(t) => Some(t),
                _ => None,
            })
            .collect()
    }
}

// -----------------------------------------------------------------------------
// Canonical form
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ident {
    Virtual(String),
    Real(u64, Vec<u16>),
}

pub fn ident(index: &Index, id: EntryId) -> Ident {
    match index.file_ref(id) {
        Some(r) => Ident::Real(r.0, index.name(id).units().to_vec()),
        None => Ident::Virtual(index.name_lossy(id)),
    }
}

type CanonAgg = (u64, u64, u32, u32, Option<u32>, Option<u32>, u64, u64, bool);

/// Everything observable about one entry, by identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Canon {
    pub ident: Ident,
    pub parent: Option<Ident>,
    pub flags: u32,
    pub own: (u64, u64),
    pub contrib: (u64, u64),
    pub times: Option<(u32, u32, u32, u32)>,
    pub intended: Option<u64>,
    pub agg: Option<CanonAgg>,
}

pub fn canonical(index: &Index) -> Vec<Canon> {
    let mut out = Vec::new();
    index.for_each_in_subtree(index.root(), |id| {
        let agg = index.aggregate(id).map(|a| {
            let size = |e: Option<EntryId>, m| e.map_or(0, |e| index.contribution(e, m));
            (
                a.logical,
                a.allocated,
                a.files,
                a.dirs,
                a.newest.map(|t| t.0),
                a.oldest.map(|t| t.0),
                size(a.largest_allocated, SizeMode::Allocated),
                size(a.largest_logical, SizeMode::Logical),
                a.partial,
            )
        });
        out.push(Canon {
            ident: ident(index, id),
            parent: index.parent(id).map(|p| ident(index, p)),
            flags: index.flags(id).0,
            own: (index.own_logical(id), index.own_allocated(id)),
            contrib: (
                index.contribution(id, SizeMode::Logical),
                index.contribution(id, SizeMode::Allocated),
            ),
            times: index
                .times(id)
                .map(|t| (t.created.0, t.modified.0, t.accessed.0, t.changed.0)),
            intended: index.intended_parent(id).map(|r| r.0),
            agg,
        });
    });
    assert_eq!(out.len(), index.len(), "every live entry is reachable");
    out.sort();
    out
}

pub fn diff(a: &[Canon], b: &[Canon]) -> Option<String> {
    if a == b {
        return None;
    }
    for (x, y) in a.iter().zip(b) {
        if x != y {
            return Some(format!("live:  {x:?}\nfresh: {y:?}"));
        }
    }
    Some(format!(
        "lengths differ: live {} fresh {}",
        a.len(),
        b.len()
    ))
}
