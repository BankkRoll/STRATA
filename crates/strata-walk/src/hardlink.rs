//! Hardlink merging.
//!
//! The MFT scanner emits one record per file with every `(parent, name)`
//! link. The walker meets the links of a hardlinked file in different
//! directories, on different threads, in no particular order, so it holds
//! such records back here until it has seen as many links as the file's
//! `NumberOfLinks`, then emits the merged record once. Links outside the walk
//! root are never seen; those records are released by [`HardlinkTable::drain`]
//! when the walk ends (or is cancelled) with the links that were found.
//!
//! Only files with `NumberOfLinks > 1` (known from the allocation pass) enter
//! the table, so its size is bounded by the number of hardlinked files.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use strata_core::ScanRecord;

const SHARDS: usize = 32;

fn shard_of(key: u128) -> usize {
    let folded = (key as u64) ^ ((key >> 64) as u64);
    (folded.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 59) as usize % SHARDS
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // NOTE: a panic while holding a shard only ever leaves a complete map
    // behind (inserts are single operations), so poisoning is ignored.
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct Pending {
    record: ScanRecord,
    expected: u32,
}

/// Records of hardlinked files awaiting their remaining links.
pub(crate) struct HardlinkTable {
    shards: Vec<Mutex<HashMap<u128, Pending>>>,
    merged: AtomicU64,
}

impl std::fmt::Debug for HardlinkTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HardlinkTable")
            .field("merged", &self.merged)
            .finish_non_exhaustive()
    }
}

impl HardlinkTable {
    pub(crate) fn new() -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Mutex::default()).collect(),
            merged: AtomicU64::new(0),
        }
    }

    /// Adds one sighting of the file identified by `key`. `record` carries
    /// exactly the link just seen. Returns the merged record once `expected`
    /// links have been collected.
    pub(crate) fn add(&self, key: u128, record: ScanRecord, expected: u32) -> Option<ScanRecord> {
        let mut shard = lock(&self.shards[shard_of(key)]);
        let done = match shard.get_mut(&key) {
            Some(p) => {
                self.merged
                    .fetch_add(record.links.len() as u64, Ordering::Relaxed);
                p.record.links.extend(record.links);
                p.expected = p.expected.max(expected);
                p.record.links.len() >= p.expected as usize
            }
            None if record.links.len() >= expected as usize => return Some(record),
            None => {
                shard.insert(key, Pending { record, expected });
                false
            }
        };
        if done {
            shard.remove(&key).map(|p| p.record)
        } else {
            None
        }
    }

    /// Removes and returns every record still waiting for links.
    pub(crate) fn drain(&self) -> Vec<ScanRecord> {
        self.shards
            .iter()
            .flat_map(|s| lock(s).drain().map(|(_, p)| p.record).collect::<Vec<_>>())
            .collect()
    }

    /// Extra links merged into an earlier record so far.
    pub(crate) fn merged(&self) -> u64 {
        self.merged.load(Ordering::Relaxed)
    }
}

/// Record ids produced so far, used to keep ids unique when hardlink counts
/// are unknown (allocation pass off) and when entries move between
/// directories during a walk.
pub(crate) struct SeenIds {
    shards: Vec<Mutex<HashSet<u128>>>,
}

impl std::fmt::Debug for SeenIds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeenIds").finish_non_exhaustive()
    }
}

impl SeenIds {
    pub(crate) fn new() -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Mutex::default()).collect(),
        }
    }

    /// Records `key`; returns `false` if it had been seen before.
    pub(crate) fn insert(&self, key: u128) -> bool {
        lock(&self.shards[shard_of(key)]).insert(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::{EntryFlags, FileRef, NameLink, Sizes, Times, WideName};

    fn rec(id: u64, parent: u64, name: &str) -> ScanRecord {
        ScanRecord {
            id: FileRef(id),
            links: vec![NameLink {
                parent: FileRef(parent),
                name: WideName::from_str_lossless(name),
            }],
            attributes: 0,
            flags: EntryFlags::EMPTY,
            times: Times::default(),
            fn_created: None,
            sizes: Sizes::default(),
            reparse: None,
            ads: Vec::new(),
        }
    }

    #[test]
    fn merges_when_all_links_seen() {
        let t = HardlinkTable::new();
        assert!(t.add(7, rec(7, 1, "a"), 3).is_none());
        assert!(t.add(7, rec(7, 2, "b"), 3).is_none());
        let merged = t.add(7, rec(7, 3, "c"), 3).expect("complete");
        assert_eq!(merged.links.len(), 3);
        assert_eq!(t.merged(), 2);
        assert!(t.drain().is_empty());
    }

    #[test]
    fn drain_releases_incomplete_records() {
        let t = HardlinkTable::new();
        assert!(t.add(9, rec(9, 1, "a"), 5).is_none());
        assert!(t.add(9, rec(9, 2, "b"), 5).is_none());
        let left = t.drain();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].links.len(), 2);
    }

    #[test]
    fn single_link_passes_through() {
        let t = HardlinkTable::new();
        assert!(t.add(1, rec(1, 1, "a"), 1).is_some());
    }

    #[test]
    fn seen_ids_detects_repeats() {
        let s = SeenIds::new();
        assert!(s.insert(5));
        assert!(!s.insert(5));
        assert!(s.insert(u128::MAX - 1));
    }
}
