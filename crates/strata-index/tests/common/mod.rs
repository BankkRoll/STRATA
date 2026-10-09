//! Shared helpers for the integration tests: record constructors and a
//! canonical, id-independent form of an index for equality checks.

#![allow(dead_code)]

use strata_core::{
    EntryFlags, FileRef, FileTime, NameLink, ScanRecord, SizeMode, Sizes, Times, WideName,
};
use strata_index::{EntryId, Index, IndexBuilder, IndexOptions};

pub const ROOT: FileRef = FileRef::from_parts(5, 5);

/// A "now" in 2025, so 2020-ish times are plausible.
pub fn now() -> FileTime {
    FileTime::from_unix_secs(1_760_000_000)
}

pub fn opts() -> IndexOptions {
    IndexOptions {
        now: now(),
        ..IndexOptions::default()
    }
}

pub fn r(record: u64, seq: u16) -> FileRef {
    FileRef::from_parts(record, seq)
}

pub fn link(parent: FileRef, name: &str) -> NameLink {
    NameLink {
        parent,
        name: WideName::from_str_lossless(name),
    }
}

pub fn times(mtime_unix: i64) -> Times {
    let t = FileTime::from_unix_secs(mtime_unix);
    Times {
        created: t,
        modified: t,
        accessed: t,
        changed: t,
    }
}

pub fn record(
    id: FileRef,
    links: Vec<NameLink>,
    dir: bool,
    logical: u64,
    allocated: u64,
) -> ScanRecord {
    ScanRecord {
        id,
        links,
        attributes: 0,
        flags: if dir {
            EntryFlags::DIR
        } else {
            EntryFlags::EMPTY
        },
        times: times(1_700_000_000),
        fn_created: None,
        sizes: Sizes {
            logical,
            allocated,
            ..Sizes::default()
        },
        reparse: None,
        ads: vec![],
    }
}

pub fn root_rec() -> ScanRecord {
    record(ROOT, vec![link(ROOT, "")], true, 0, 0)
}

pub fn dir(id: FileRef, parent: FileRef, name: &str) -> ScanRecord {
    record(id, vec![link(parent, name)], true, 0, 0)
}

pub fn file(id: FileRef, parent: FileRef, name: &str, size: u64) -> ScanRecord {
    record(
        id,
        vec![link(parent, name)],
        false,
        size,
        size.next_multiple_of(4096),
    )
}

pub fn build(records: impl IntoIterator<Item = ScanRecord>, opts: IndexOptions) -> Index {
    let mut b = IndexBuilder::new(opts);
    b.push_batch(records).expect("push");
    b.finish().expect("finish")
}

/// Identity of an entry independent of its id.
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

/// Aggregate fields with largest-descendant *sizes* (ids differ between
/// builds; ties may legitimately pick different entries of equal size).
pub type CanonAgg = (u64, u64, u32, u32, Option<u32>, Option<u32>, u64, u64, bool);

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
    assert_eq!(
        out.len(),
        index.len(),
        "every live entry is reachable from the root"
    );
    out.sort();
    out
}

/// First difference between two canonical forms, for readable failures.
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
