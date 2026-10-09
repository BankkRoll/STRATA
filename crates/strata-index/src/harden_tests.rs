//! Capacity refusals, exercised with tiny limits: the builder and the live
//! path must refuse cleanly at the entry and name-byte limits, leave the
//! index unchanged, and still finish.

use strata_core::{EntryFlags, FileRef, NameLink, ScanRecord, Sizes, Times, WideName};

use crate::index::Limits;
use crate::{IndexBuilder, IndexError, IndexOptions};

const ROOT: FileRef = FileRef::from_parts(5, 5);

fn rec(id: u64, parent: FileRef, name: &str, dir: bool) -> ScanRecord {
    ScanRecord {
        id: if id == 5 {
            ROOT
        } else {
            FileRef::from_parts(id, 1)
        },
        links: vec![NameLink {
            parent,
            name: WideName::from_str_lossless(name),
        }],
        attributes: 0,
        flags: if dir {
            EntryFlags::DIR
        } else {
            EntryFlags::EMPTY
        },
        times: Times::default(),
        fn_created: None,
        sizes: Sizes {
            logical: 10,
            allocated: 10,
            ..Sizes::default()
        },
        reparse: None,
        ads: vec![],
    }
}

fn builder(limits: Limits) -> IndexBuilder {
    IndexBuilder::new(IndexOptions::default()).with_limits(limits)
}

fn names_of(index: &crate::Index) -> Vec<String> {
    let mut v: Vec<String> = index
        .children(index.root())
        .map(|c| index.name(c).to_string_lossy())
        .collect();
    v.sort();
    v
}

#[test]
fn builder_refuses_past_the_entry_limit_and_still_finishes() {
    let mut b = builder(Limits {
        entries: 10,
        ..Limits::default()
    });
    b.push(rec(5, ROOT, "", true)).unwrap();
    for i in 0..9 {
        b.push(rec(100 + i, ROOT, &format!("f{i}"), false)).unwrap();
    }
    assert_eq!(
        b.push(rec(200, ROOT, "one-too-many", false)),
        Err(IndexError::TooManyEntries(11))
    );
    assert_eq!(b.staged(), 10);
    let index = b.finish().unwrap();
    index.check_invariants().unwrap();
    assert_eq!(index.children(index.root()).count(), 9 + 2);
    assert!(!names_of(&index).contains(&"one-too-many".to_string()));
}

#[test]
fn builder_refuses_past_the_name_limit_and_names_stay_right() {
    let mut b = builder(Limits {
        name_bytes: 400,
        ..Limits::default()
    });
    b.push(rec(5, ROOT, "", true)).unwrap();
    let mut accepted = Vec::new();
    let mut refused = None;
    for i in 0..100u64 {
        let name = format!("file-{i:03}-ünïcödé");
        match b.push(rec(100 + i, ROOT, &name, false)) {
            Ok(()) => accepted.push(name),
            Err(e) => {
                refused = Some(e);
                break;
            }
        }
    }
    assert!(matches!(refused, Some(IndexError::NameStoreFull(n)) if n > 400));
    assert!(!accepted.is_empty());
    let staged = b.staged();
    // A refused push leaves the builder exactly as it was.
    assert!(b.push(rec(999, ROOT, &"x".repeat(500), false)).is_err());
    assert_eq!(b.staged(), staged);
    let index = b.finish().unwrap();
    index.check_invariants().unwrap();
    let mut want = accepted.clone();
    want.extend([
        crate::METADATA_NODE_NAME.to_string(),
        crate::ORPHANS_NODE_NAME.to_string(),
    ]);
    want.sort();
    assert_eq!(names_of(&index), want);
    for (i, name) in accepted.iter().enumerate() {
        let id = index
            .lookup(FileRef::from_parts(100 + i as u64, 1))
            .unwrap();
        assert_eq!(index.path_string(id), format!(r"\{name}"));
    }
}

#[test]
fn live_updates_refuse_at_the_name_limit_without_changing_the_index() {
    let mut b = builder(Limits {
        name_bytes: 300,
        ..Limits::default()
    });
    b.push(rec(5, ROOT, "", true)).unwrap();
    b.push(rec(100, ROOT, "keep.bin", false)).unwrap();
    let mut index = b.finish().unwrap();
    let before = names_of(&index);
    let len = index.len();

    // Creates fill the buffer, then refuse.
    let mut created = 0u64;
    let err = loop {
        match index.upsert(rec(
            1000 + created,
            ROOT,
            &format!("new-{created:04}"),
            false,
        )) {
            Ok(_) => created += 1,
            Err(e) => break e,
        }
        assert!(created < 1000, "never refused");
    };
    assert!(matches!(err, IndexError::NameStoreFull(_)), "{err:?}");
    index.check_invariants().unwrap();
    assert_eq!(index.len(), len + created as usize);
    assert!(
        index
            .lookup(FileRef::from_parts(1000 + created, 1))
            .is_none()
    );

    // A size change that keeps the name still applies when full.
    let mut grown = rec(100, ROOT, "keep.bin", false);
    grown.sizes.allocated = 4096;
    index.upsert(grown).unwrap();
    let keep = index.lookup(FileRef::from_parts(100, 1)).unwrap();
    assert_eq!(index.size(keep, strata_core::SizeMode::Allocated), 4096);

    // A rename that needs new bytes is refused and the old name kept.
    let err = index
        .upsert(rec(100, ROOT, &"renamed".repeat(20), false))
        .unwrap_err();
    assert!(matches!(err, IndexError::NameStoreFull(_)));
    assert_eq!(index.name(keep).to_string_lossy(), "keep.bin");
    index.check_invariants().unwrap();
    assert!(before.iter().all(|n| names_of(&index).contains(n)));

    // Compaction drops the garbage; the index stays consistent.
    index.compact();
    index.check_invariants().unwrap();
    assert_eq!(
        index.path_string(index.lookup(FileRef::from_parts(100, 1)).unwrap()),
        r"\keep.bin"
    );
}

#[test]
fn live_creates_refuse_at_the_entry_limit() {
    let mut b = builder(Limits {
        entries: 6,
        ..Limits::default()
    });
    b.push(rec(5, ROOT, "", true)).unwrap();
    let mut index = b.finish().unwrap();
    let mut n = 0;
    let err = loop {
        match index.upsert(rec(1000 + n, ROOT, &format!("e{n}"), false)) {
            Ok(_) => n += 1,
            Err(e) => break e,
        }
        assert!(n < 100);
    };
    assert!(matches!(err, IndexError::TooManyEntries(_)));
    index.check_invariants().unwrap();
    // Removing one frees a slot for the next create.
    index.remove(FileRef::from_parts(1000, 1)).unwrap();
    index.upsert(rec(5000, ROOT, "reuse", false)).unwrap();
    index.check_invariants().unwrap();
}
