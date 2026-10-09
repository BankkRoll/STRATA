//! Path reconstruction with deep chains, Unicode and unpaired surrogates,
//! cycles reached from long tails, and directory records reused with a new
//! sequence number.

use strata_cli::paths::{CYCLE, ORPHAN, PathIndex};
use strata_core::{EntryFlags, FileRef, NameLink, ScanRecord, Sizes, Times, WideName};

const ROOT: FileRef = FileRef::from_parts(5, 5);

fn dir(n: u64, seq: u16, parent: FileRef, name: WideName) -> ScanRecord {
    ScanRecord {
        id: FileRef::from_parts(n, seq),
        links: vec![NameLink { parent, name }],
        attributes: 0x10,
        flags: EntryFlags::DIR,
        times: Times::default(),
        fn_created: None,
        sizes: Sizes::default(),
        reparse: None,
        ads: vec![],
    }
}

fn n(s: &str) -> WideName {
    WideName::from_str_lossless(s)
}

#[test]
fn deep_unicode_chains_resolve_exactly() {
    let mut idx = PathIndex::default();
    let mut parent = ROOT;
    let mut want = String::new();
    for i in 0..3000u64 {
        let name = if i % 2 == 0 {
            n(&format!("Ærø🦀{i}"))
        } else {
            WideName::from_units(vec![0x61, 0xD800, u16::from(b'0' + (i % 10) as u8)])
        };
        want.push('\\');
        want.push_str(&name.to_string_lossy());
        idx.add(&dir(100 + i, 1, parent, name));
        parent = FileRef::from_parts(100 + i, 1);
    }
    want.push_str("\\leaf");
    assert_eq!(idx.path(parent, &n("leaf")), want);
    // Memoized ancestors give the same answer from the middle.
    let mid = FileRef::from_parts(100 + 1500, 1);
    assert!(want.starts_with(&idx.path(mid, &n("x"))[..idx.path(mid, &n("x")).len() - 2]));
}

#[test]
fn cycles_behind_long_tails_and_stale_sequences() {
    let mut idx = PathIndex::default();
    // 30 -> 31 -> 32 -> 30 is a loop; 40..140 is a tail leading into it.
    idx.add(&dir(30, 1, FileRef::from_parts(32, 1), n("a")));
    idx.add(&dir(31, 1, FileRef::from_parts(30, 1), n("b")));
    idx.add(&dir(32, 1, FileRef::from_parts(31, 1), n("c")));
    let mut parent = FileRef::from_parts(30, 1);
    for i in 40..140u64 {
        idx.add(&dir(i, 1, parent, n(&format!("t{i}"))));
        parent = FileRef::from_parts(i, 1);
    }
    let p = idx.path(parent, &n("f"));
    assert!(p.starts_with(CYCLE), "{}", &p[..40]);
    assert!(p.ends_with(r"\t139\f"));
    for r in [30u64, 31, 32] {
        assert!(
            idx.path(FileRef::from_parts(r, 1), &n("f"))
                .starts_with(CYCLE)
        );
    }

    // A directory renamed and re-added after paths were resolved.
    idx.add(&dir(500, 1, ROOT, n("old")));
    assert_eq!(idx.path(FileRef::from_parts(500, 1), &n("f")), r"\old\f");
    idx.add(&dir(500, 1, ROOT, n("renamed")));
    assert_eq!(
        idx.path(FileRef::from_parts(500, 1), &n("f")),
        r"\renamed\f"
    );

    // A child resolved before its parent arrived is fixed once it does.
    idx.add(&dir(601, 1, FileRef::from_parts(600, 1), n("child")));
    let early = idx.path(FileRef::from_parts(601, 1), &n("f"));
    assert!(early.starts_with(ORPHAN), "{early}");
    idx.add(&dir(600, 1, ROOT, n("parent")));
    assert_eq!(
        idx.path(FileRef::from_parts(601, 1), &n("f")),
        r"\parent\child\f"
    );

    // A directory record reused with a new sequence: old references are
    // orphans, new ones resolve.
    let mut fresh = PathIndex::default();
    fresh.add(&dir(50, 2, ROOT, n("new")));
    assert_eq!(fresh.path(FileRef::from_parts(50, 2), &n("f")), r"\new\f");
    assert!(
        fresh
            .path(FileRef::from_parts(50, 1), &n("f"))
            .starts_with(ORPHAN)
    );
}
