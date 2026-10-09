//! File object / file key → name mapping.
//!
//! Kernel-File write events carry no name, only two kernel pointers:
//! - `FileObject`, one per open handle, named by the `Create` event that
//!   opened it;
//! - `FileKey`, one per open stream (the file system's context), named by
//!   `NameCreate` and released by `NameDelete`.
//!
//! A write is resolved by key first (it survives handle churn and is renamed
//! by `NameCreate`), then by file object. Both pointers are recycled by the
//! kernel, so a new `Create`/`NameCreate` for the same pointer always replaces
//! the old name.
//!
//! **Renames.** `RenamePath` carries one path. If it differs from the name
//! mapped for the file, it is taken as the new name and both
//! pointers move to it; if it equals the current name it is the old name, and
//! the follow-up `NameCreate` supplies the new one. Either convention of the
//! provider therefore ends with the new name mapped.
//!
//! The maps are bounded: each holds two generations of at most
//! `capacity / 2` entries; when the current generation fills it becomes the
//! old one and the previous old one is dropped. Entries for files that are
//! still being written are re-inserted on use, so they survive rotation.

use std::collections::HashMap;
use std::sync::Arc;

use crate::paths::FileInfo;

#[derive(Debug)]
struct Generations {
    cur: HashMap<u64, Arc<FileInfo>>,
    old: HashMap<u64, Arc<FileInfo>>,
    half: usize,
}

impl Generations {
    fn new(capacity: usize) -> Self {
        Self {
            cur: HashMap::new(),
            old: HashMap::new(),
            half: (capacity / 2).max(1),
        }
    }

    fn insert(&mut self, k: u64, v: Arc<FileInfo>) {
        if self.cur.len() >= self.half && !self.cur.contains_key(&k) {
            self.old = std::mem::take(&mut self.cur);
        }
        self.old.remove(&k);
        self.cur.insert(k, v);
    }

    fn get(&mut self, k: u64) -> Option<Arc<FileInfo>> {
        if let Some(v) = self.cur.get(&k) {
            return Some(v.clone());
        }
        let v = self.old.remove(&k)?;
        self.insert(k, v.clone());
        Some(v)
    }

    fn remove(&mut self, k: u64) -> Option<Arc<FileInfo>> {
        self.cur.remove(&k).or_else(|| self.old.remove(&k))
    }

    fn len(&self) -> usize {
        self.cur.len() + self.old.len()
    }

    fn clear(&mut self) {
        self.cur.clear();
        self.old.clear();
    }
}

/// The two pointer → name maps.
#[derive(Debug)]
pub struct FileTable {
    by_object: Generations,
    by_key: Generations,
}

impl Default for FileTable {
    fn default() -> Self {
        Self::with_capacity(Self::DEFAULT_CAPACITY)
    }
}

impl FileTable {
    /// Default bound per map (entries).
    pub const DEFAULT_CAPACITY: usize = 262_144;

    /// A table holding at most `capacity` entries per map.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            by_object: Generations::new(capacity),
            by_key: Generations::new(capacity),
        }
    }

    /// `Create`: `file_object` now refers to `file`.
    pub fn open(&mut self, file_object: u64, file: Arc<FileInfo>) {
        self.by_object.insert(file_object, file);
    }

    /// `NameCreate`: `key` now refers to `file`.
    pub fn name_key(&mut self, key: u64, file: Arc<FileInfo>) {
        self.by_key.insert(key, file);
    }

    /// `NameDelete`: drops `key` if it still names the file with `hash`.
    pub fn release_key(&mut self, key: u64, hash: u64) {
        if let Some(cur) = self.by_key.get(key)
            && cur.hash == hash
        {
            self.by_key.remove(key);
        }
    }

    /// The file a write on (`file_object`, `key`) goes to.
    pub fn lookup(&mut self, file_object: u64, key: u64) -> Option<Arc<FileInfo>> {
        if key != 0
            && let Some(f) = self.by_key.get(key)
        {
            return Some(f);
        }
        self.by_object.get(file_object)
    }

    /// Applies a `RenamePath` event. Returns `(old, new)` when the file moved
    /// from a known name to a different one.
    pub fn rename(
        &mut self,
        file_object: u64,
        key: u64,
        path: Arc<FileInfo>,
    ) -> Option<(Arc<FileInfo>, Arc<FileInfo>)> {
        let current = self.lookup(file_object, key);
        if current.as_ref().is_some_and(|c| c.hash == path.hash) {
            return None;
        }
        self.by_object.insert(file_object, path.clone());
        if key != 0 {
            self.by_key.insert(key, path.clone());
        }
        current.map(|old| (old, path))
    }

    /// Entries across both maps.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_object.len() + self.by_key.len()
    }

    /// Whether both maps are empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Forgets every mapping.
    pub fn clear(&mut self) {
        self.by_object.clear();
        self.by_key.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::PathMapper;
    use strata_win::path::DeviceMap;

    fn f(m: &mut PathMapper, p: &str) -> Arc<FileInfo> {
        m.file_from_dos(p.to_owned())
    }

    #[test]
    fn reused_file_object_takes_the_new_name() {
        let mut m = PathMapper::new(DeviceMap::default());
        let mut t = FileTable::default();
        t.open(0x10, f(&mut m, r"C:\a\one.txt"));
        assert_eq!(&*t.lookup(0x10, 0).unwrap().path, r"C:\a\one.txt");
        t.open(0x10, f(&mut m, r"C:\b\two.txt"));
        assert_eq!(&*t.lookup(0x10, 0).unwrap().path, r"C:\b\two.txt");
    }

    #[test]
    fn key_wins_over_object_and_release_checks_name() {
        let mut m = PathMapper::new(DeviceMap::default());
        let mut t = FileTable::default();
        t.open(1, f(&mut m, r"C:\obj.txt"));
        let k = f(&mut m, r"C:\key.txt");
        t.name_key(2, k.clone());
        assert_eq!(&*t.lookup(1, 2).unwrap().path, r"C:\key.txt");
        // A stale NameDelete for another name leaves the mapping alone.
        t.release_key(2, f(&mut m, r"C:\other.txt").hash);
        assert!(t.lookup(9, 2).is_some());
        t.release_key(2, k.hash);
        assert_eq!(&*t.lookup(1, 2).unwrap().path, r"C:\obj.txt");
    }

    #[test]
    fn rename_either_convention() {
        let mut m = PathMapper::new(DeviceMap::default());
        let mut t = FileTable::default();
        let old = f(&mut m, r"C:\d\tmp123.part");
        t.open(5, old.clone());
        t.name_key(6, old.clone());
        // Path = new name.
        let (o, n) = t.rename(5, 6, f(&mut m, r"C:\d\final.bin")).unwrap();
        assert_eq!(
            (&*o.path, &*n.path),
            (r"C:\d\tmp123.part", r"C:\d\final.bin")
        );
        assert_eq!(&*t.lookup(5, 6).unwrap().path, r"C:\d\final.bin");
        // Path = old (current) name: no change until NameCreate.
        assert!(t.rename(5, 6, f(&mut m, r"C:\d\final.bin")).is_none());
        t.name_key(6, f(&mut m, r"C:\e\moved.bin"));
        assert_eq!(&*t.lookup(5, 6).unwrap().path, r"C:\e\moved.bin");
    }

    #[test]
    fn bounded_with_generations() {
        let mut m = PathMapper::new(DeviceMap::default());
        let mut t = FileTable::with_capacity(8);
        let hot = f(&mut m, r"C:\hot.log");
        t.open(1_000, hot);
        for i in 0..100u64 {
            t.open(i, f(&mut m, &format!(r"C:\f{i}")));
            // Touching the hot entry keeps it alive across rotations.
            assert!(t.lookup(1_000, 0).is_some());
        }
        assert!(t.len() <= 8);
        assert!(t.lookup(0, 0).is_none());
    }
}
