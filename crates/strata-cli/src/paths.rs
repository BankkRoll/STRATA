//! Minimal path reconstruction from parent references.
//!
//! Only directories are remembered (sequence, first parent, first name), which
//! is all that building a file's path needs. This is deliberately not a tree:
//! aggregation belongs to `strata-index`.

use std::collections::{HashMap, HashSet};

use strata_core::{FileRef, ScanRecord, WideName};

/// Prefix for paths whose parent chain hits a missing or stale directory.
pub const ORPHAN: &str = "<orphan>";
/// Prefix for paths whose parent chain loops.
pub const CYCLE: &str = "<cycle>";

/// Depth beyond which a chain is treated as a loop. NTFS paths are capped at
/// 32,767 UTF-16 units, so 16,384 one-character levels is already impossible.
const MAX_DEPTH: usize = 16_384;

#[derive(Debug, Clone)]
struct Dir {
    sequence: u16,
    parent: FileRef,
    name: WideName,
}

/// Directory map plus a memo of resolved directory paths.
#[derive(Debug, Default)]
pub struct PathIndex {
    dirs: HashMap<u64, Dir>,
    memo: HashMap<u64, String>,
}

impl PathIndex {
    /// Remembers `r` if it is a directory with at least one name.
    pub fn add(&mut self, r: &ScanRecord) {
        if let (true, Some(l)) = (r.is_dir(), r.links.first()) {
            self.dirs.insert(
                r.id.record(),
                Dir {
                    sequence: r.id.sequence(),
                    parent: l.parent,
                    name: l.name.clone(),
                },
            );
            // A directory arriving after paths were resolved can complete
            // (or change) any memoized chain, including "<orphan>" ones.
            if !self.memo.is_empty() {
                self.memo.clear();
            }
        }
    }

    /// Volume-relative path (`\a\b`) of `name` inside directory `parent`.
    /// The root's self-link yields `\`.
    pub fn path(&mut self, parent: FileRef, name: &WideName) -> String {
        if parent.record() == FileRef::NTFS_ROOT_RECORD && is_root_name(name) {
            return "\\".to_owned();
        }
        let mut p = self.dir_path(parent);
        if !p.ends_with('\\') {
            p.push('\\');
        }
        p.push_str(&name.to_string_lossy());
        p
    }

    /// Path of a directory reference, resolving iteratively with memoization.
    fn dir_path(&mut self, dir: FileRef) -> String {
        if dir.record() == FileRef::NTFS_ROOT_RECORD {
            return "\\".to_owned();
        }
        // Walk up until the root, a memoized ancestor, or a break in the chain.
        let mut chain: Vec<u64> = Vec::new();
        let mut seen = HashSet::new();
        let mut cur = dir;
        let prefix = loop {
            let n = cur.record();
            if n == FileRef::NTFS_ROOT_RECORD {
                break String::new();
            }
            let Some(d) = self.dirs.get(&n) else {
                break ORPHAN.to_owned();
            };
            // The memo is keyed by record, so it is only valid once the
            // reference's sequence number has been checked.
            if d.sequence != cur.sequence() {
                break ORPHAN.to_owned();
            }
            if let Some(p) = self.memo.get(&n) {
                break p.clone();
            }
            if !seen.insert(n) || chain.len() >= MAX_DEPTH {
                break CYCLE.to_owned();
            }
            chain.push(n);
            cur = d.parent;
        };
        let mut path = prefix;
        for n in chain.iter().rev() {
            if let Some(d) = self.dirs.get(n) {
                path.push('\\');
                path.push_str(&d.name.to_string_lossy());
            }
            self.memo.insert(*n, path.clone());
        }
        if path.is_empty() {
            "\\".to_owned()
        } else {
            path
        }
    }
}

fn is_root_name(name: &WideName) -> bool {
    name.units() == [u16::from(b'.')]
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::{EntryFlags, NameLink, Sizes, Times};

    fn dir(n: u64, seq: u16, parent: FileRef, name: &str) -> ScanRecord {
        ScanRecord {
            id: FileRef::from_parts(n, seq),
            links: vec![NameLink {
                parent,
                name: WideName::from_str_lossless(name),
            }],
            attributes: 0x10,
            flags: EntryFlags::DIR,
            times: Times::default(),
            fn_created: None,
            sizes: Sizes::default(),
            reparse: None,
            ads: vec![],
        }
    }

    #[test]
    fn resolves_nested_orphan_stale_and_cyclic_chains() {
        let root = FileRef::from_parts(5, 5);
        let mut idx = PathIndex::default();
        idx.add(&dir(20, 1, root, "a"));
        idx.add(&dir(21, 1, FileRef::from_parts(20, 1), "b"));
        idx.add(&dir(22, 1, FileRef::from_parts(99, 1), "lost"));
        idx.add(&dir(23, 1, FileRef::from_parts(20, 7), "stale"));
        idx.add(&dir(30, 1, FileRef::from_parts(31, 1), "x"));
        idx.add(&dir(31, 1, FileRef::from_parts(30, 1), "y"));
        let n = |s: &str| WideName::from_str_lossless(s);
        assert_eq!(idx.path(root, &n(".")), "\\");
        assert_eq!(idx.path(root, &n("f")), "\\f");
        assert_eq!(idx.path(FileRef::from_parts(21, 1), &n("f")), "\\a\\b\\f");
        assert_eq!(
            idx.path(FileRef::from_parts(22, 1), &n("f")),
            "<orphan>\\lost\\f"
        );
        assert_eq!(
            idx.path(FileRef::from_parts(23, 1), &n("f")),
            "<orphan>\\stale\\f"
        );
        assert_eq!(idx.path(FileRef::from_parts(21, 2), &n("f")), "<orphan>\\f");
        assert!(
            idx.path(FileRef::from_parts(30, 1), &n("f"))
                .starts_with("<cycle>")
        );
        // Memoized second lookup is identical.
        assert_eq!(idx.path(FileRef::from_parts(21, 1), &n("g")), "\\a\\b\\g");
    }
}
