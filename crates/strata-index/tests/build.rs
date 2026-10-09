//! Build-path behaviour: ordering, accounting rules, orphans, cycles,
//! metadata grouping, reparse points, timestamps, partial scans, memory.

mod common;

use common::*;
use strata_core::{EntryFlags, FileTime, Reparse, ScanRecord, SizeMode, Sizes, WideName, win32};
use strata_index::{IndexBuilder, IndexError, IndexOptions, METADATA_NODE_NAME, ORPHANS_NODE_NAME};

fn sample() -> Vec<ScanRecord> {
    vec![
        root_rec(),
        dir(r(30, 1), ROOT, "Users"),
        dir(r(31, 1), r(30, 1), "me"),
        file(r(40, 1), r(31, 1), "a.bin", 10_000),
        file(r(41, 1), r(31, 1), "b.txt", 100),
        file(r(42, 1), ROOT, "pagefile.sys", 1 << 20),
    ]
}

#[test]
fn arrival_order_does_not_matter() {
    let fwd = build(sample(), opts());
    let mut rev = sample();
    rev.reverse();
    let rev = build(rev, opts());
    assert_eq!(canonical(&fwd), canonical(&rev));
    fwd.check_invariants().unwrap();
}

#[test]
fn aggregates_and_counts() {
    let idx = build(sample(), opts());
    let root = idx.aggregate(idx.root()).unwrap();
    assert_eq!(root.logical, 10_000 + 100 + (1 << 20));
    assert_eq!(root.allocated, 12_288 + 4096 + (1 << 20));
    assert_eq!(root.files, 3);
    // Users, me, and the two virtual groups (not counted).
    assert_eq!(root.dirs, 2);
    let me = idx.lookup(r(31, 1)).unwrap();
    let a = idx.aggregate(me).unwrap();
    assert_eq!((a.files, a.dirs, a.allocated), (2, 0, 16_384));
    assert_eq!(idx.name_lossy(a.largest_allocated.unwrap()), "a.bin");
    assert_eq!(idx.path_string(me), r"\Users\me");
    assert_eq!(idx.child_count(idx.root()), 4);
}

#[test]
fn stale_and_missing_parents_become_orphans() {
    let mut recs = sample();
    // Parent record 30 exists with sequence 1; this link names sequence 9.
    recs.push(file(r(50, 1), r(30, 9), "stale.dat", 5));
    recs.push(file(r(51, 1), r(999, 1), "lost.dat", 7));
    recs.push(record(r(52, 3), vec![], false, 11, 11));
    let idx = build(recs, opts());
    idx.check_invariants().unwrap();
    let orph = idx.orphans_node();
    assert_eq!(idx.name_lossy(orph), ORPHANS_NODE_NAME);
    for (fr, name) in [
        (r(50, 1), "stale.dat"),
        (r(51, 1), "lost.dat"),
        (r(52, 3), "<record 52>"),
    ] {
        let e = idx.lookup(fr).unwrap();
        assert_eq!(idx.parent(e), Some(orph), "{name}");
        assert!(idx.flags(e).contains(EntryFlags::ORPHAN));
        assert_eq!(idx.name_lossy(e), name);
    }
    assert_eq!(
        idx.intended_parent(idx.lookup(r(50, 1)).unwrap()),
        Some(r(30, 9))
    );
    assert_eq!(idx.intended_parent(idx.lookup(r(52, 3)).unwrap()), None);
    assert_eq!(idx.aggregate(orph).unwrap().logical, 23);
    // Orphans still count toward the volume total.
    assert_eq!(
        idx.aggregate(idx.root()).unwrap().logical,
        10_000 + 100 + (1 << 20) + 23
    );
}

#[test]
fn hardlinks_count_once_or_split() {
    let mut recs = sample();
    recs.push(record(
        r(60, 1),
        vec![
            link(r(31, 1), "primary.dll"),
            link(r(30, 1), "second.dll"),
            link(ROOT, "third.dll"),
        ],
        false,
        1000,
        4096,
    ));
    let idx = build(recs.clone(), opts());
    idx.check_invariants().unwrap();
    let links = idx.links(r(60, 1));
    assert_eq!(links.len(), 3);
    assert_eq!(idx.name_lossy(links[0]), "primary.dll");
    assert!(!idx.flags(links[0]).contains(EntryFlags::HARDLINK_SECONDARY));
    assert!(idx.flags(links[1]).contains(EntryFlags::HARDLINK_SECONDARY));
    assert_eq!(idx.contribution(links[0], SizeMode::Allocated), 4096);
    assert_eq!(idx.contribution(links[1], SizeMode::Allocated), 0);
    assert_eq!(idx.own_allocated(links[1]), 4096);
    assert_eq!(idx.link_count(links[2]), 3);
    let total = idx.aggregate(idx.root()).unwrap();
    assert_eq!(total.allocated, 12_288 + 4096 + (1 << 20) + 4096);
    assert_eq!(total.files, 6);

    let split = build(
        recs,
        IndexOptions {
            split_hardlinks: true,
            ..opts()
        },
    );
    split.check_invariants().unwrap();
    let links = split.links(r(60, 1));
    let shares: Vec<u64> = links
        .iter()
        .map(|&l| split.contribution(l, SizeMode::Logical))
        .collect();
    assert_eq!(shares, vec![334, 333, 333]);
    assert_eq!(
        split.aggregate(split.root()).unwrap().allocated,
        total.allocated
    );
}

#[test]
fn ntfs_metadata_is_grouped_but_keeps_its_structure() {
    let meta = |id, parent, name: &str, dir: bool, size: u64| {
        let mut rec = if dir {
            common::dir(id, parent, name)
        } else {
            file(id, parent, name, size)
        };
        rec.flags |= EntryFlags::NTFS_METADATA;
        rec
    };
    let mut recs = sample();
    // The root itself is a metadata record on NTFS.
    recs[0].flags |= EntryFlags::NTFS_METADATA;
    recs.push(meta(r(0, 1), ROOT, "$MFT", false, 1 << 22));
    recs.push(meta(r(11, 11), ROOT, "$Extend", true, 0));
    recs.push(meta(r(70, 1), r(11, 11), "$UsnJrnl", false, 1 << 21));
    let idx = build(recs, opts());
    idx.check_invariants().unwrap();
    let m = idx.metadata_node();
    assert_eq!(idx.name_lossy(m), METADATA_NODE_NAME);
    let mft = idx.lookup(r(0, 1)).unwrap();
    let ext = idx.lookup(r(11, 11)).unwrap();
    let usn = idx.lookup(r(70, 1)).unwrap();
    assert_eq!(idx.parent(mft), Some(m));
    assert_eq!(idx.parent(ext), Some(m));
    assert_eq!(idx.parent(usn), Some(ext));
    assert_eq!(idx.intended_parent(mft), Some(ROOT));
    assert_eq!(
        idx.path_string(usn),
        format!(r"\{METADATA_NODE_NAME}\$Extend\$UsnJrnl")
    );
    assert_eq!(idx.aggregate(m).unwrap().files, 2);
}

#[test]
fn parent_cycles_are_broken() {
    let mut recs = sample();
    recs.push(dir(r(80, 1), r(81, 1), "A"));
    recs.push(dir(r(81, 1), r(80, 1), "B"));
    recs.push(file(r(82, 1), r(80, 1), "inside.txt", 9));
    recs.push(dir(r(83, 1), r(83, 1), "self"));
    let idx = build(recs, opts());
    idx.check_invariants().unwrap();
    let orph = idx.orphans_node();
    for fr in [r(80, 1), r(81, 1), r(83, 1)] {
        let e = idx.lookup(fr).unwrap();
        assert_eq!(idx.parent(e), Some(orph));
        assert!(idx.flags(e).contains(EntryFlags::CYCLE_BROKEN));
    }
    let a = idx.lookup(r(80, 1)).unwrap();
    assert_eq!(idx.parent(idx.lookup(r(82, 1)).unwrap()), Some(a));
    assert_eq!(idx.aggregate(a).unwrap().logical, 9);
}

#[test]
fn reparse_points_are_leaves() {
    let mut recs = sample();
    let mut junction = dir(r(90, 1), r(31, 1), "link");
    junction.reparse = Some(Reparse {
        tag: win32::IO_REPARSE_TAG_MOUNT_POINT,
        target: Some(WideName::from_str_lossless(r"D:\data")),
    });
    junction.sizes.allocated = 0;
    recs.push(junction);
    recs.push(file(r(91, 1), r(90, 1), "through.bin", 1 << 30));
    let idx = build(recs, opts());
    idx.check_invariants().unwrap();
    let j = idx.lookup(r(90, 1)).unwrap();
    assert_eq!(idx.flags(j).reparse(), strata_core::ReparseKind::MountPoint);
    assert_eq!(idx.child_count(j), 0);
    let t = idx.lookup(r(91, 1)).unwrap();
    assert_eq!(idx.parent(t), Some(idx.orphans_node()));
    let me = idx.lookup(r(31, 1)).unwrap();
    assert_eq!(idx.aggregate(me).unwrap().logical, 10_100);
}

#[test]
fn suspicious_times_are_flagged_and_ignored_for_extremes() {
    let mut recs = sample();
    let mut old = file(r(95, 1), r(31, 1), "1980.txt", 1);
    old.times = times(315_532_800); // 1980-01-01
    let mut future = file(r(96, 1), r(31, 1), "future.txt", 1);
    future.times = times(4_000_000_000);
    recs.push(old);
    recs.push(future);
    let idx = build(recs, opts());
    for fr in [r(95, 1), r(96, 1)] {
        assert!(
            idx.flags(idx.lookup(fr).unwrap())
                .contains(EntryFlags::SUSPICIOUS_TIME)
        );
    }
    let me = idx.aggregate(idx.lookup(r(31, 1)).unwrap()).unwrap();
    let t = strata_core::EpochSecs::from_filetime(FileTime::from_unix_secs(1_700_000_000));
    assert_eq!(me.newest, Some(t));
    assert_eq!(me.oldest, Some(t));
}

#[test]
fn partial_propagates_to_ancestors() {
    let mut recs = sample();
    let mut denied = dir(r(97, 1), r(31, 1), "locked");
    denied.flags |= EntryFlags::ACCESS_DENIED;
    recs.push(denied);
    let idx = build(recs.clone(), opts());
    for fr in [r(97, 1), r(31, 1), r(30, 1), ROOT] {
        let e = idx.lookup(fr).unwrap();
        assert!(idx.flags(e).contains(EntryFlags::PARTIAL), "{fr:?}");
        assert!(idx.aggregate(e).unwrap().partial);
    }
    let other = idx.lookup(r(42, 1)).unwrap();
    assert!(!idx.flags(other).contains(EntryFlags::PARTIAL));

    let mut b = IndexBuilder::new(opts());
    b.push_batch(sample()).unwrap();
    b.set_partial(true);
    let idx = b.finish().unwrap();
    assert!(idx.aggregate(idx.root()).unwrap().partial);
    assert!(
        !idx.aggregate(idx.lookup(r(31, 1)).unwrap())
            .unwrap()
            .partial
    );
}

#[test]
fn virtual_blocks_add_bytes_not_files() {
    let mut b = IndexBuilder::new(opts());
    b.push_batch(sample()).unwrap();
    b.add_virtual_block("Unaccounted / system reserved", 0, 5 << 20);
    let idx = b.finish().unwrap();
    let agg = idx.aggregate(idx.root()).unwrap();
    assert_eq!(agg.allocated, 12_288 + 4096 + (1 << 20) + (5 << 20));
    assert_eq!(agg.files, 3);
    assert_ne!(
        idx.name_lossy(agg.largest_allocated.unwrap()),
        "Unaccounted / system reserved"
    );
}

#[test]
fn duplicate_records_last_wins() {
    let mut recs = sample();
    recs.push(file(r(40, 1), r(30, 1), "moved.bin", 1));
    let idx = build(recs, opts());
    idx.check_invariants().unwrap();
    let e = idx.lookup(r(40, 1)).unwrap();
    assert_eq!(idx.name_lossy(e), "moved.bin");
    assert_eq!(idx.links(r(40, 1)).len(), 1);
    assert_eq!(idx.aggregate(idx.root()).unwrap().files, 3);
}

#[test]
fn unpaired_surrogates_survive() {
    let units = vec![u16::from(b'x'), 0xD800, u16::from(b'y')];
    let mut recs = sample();
    recs.push(record(
        r(98, 1),
        vec![strata_core::NameLink {
            parent: ROOT,
            name: WideName::from_units(units.clone()),
        }],
        false,
        1,
        1,
    ));
    let idx = build(recs, opts());
    let e = idx.lookup(r(98, 1)).unwrap();
    assert_eq!(idx.name(e).units(), &units[..]);
    assert_eq!(idx.name_lossy(e), "x\u{FFFD}y");
    let mut path: Vec<u16> = vec![u16::from(b'\\')];
    path.extend(&units);
    assert_eq!(idx.path(e).units(), &path[..]);
}

#[test]
fn huge_sizes_spill_exactly() {
    let mut recs = sample();
    let big = 5u64 << 40;
    recs.push(record(
        r(99, 1),
        vec![link(ROOT, "huge.vhdx")],
        false,
        big,
        big + 4096,
    ));
    recs.push(record(
        r(100, 1),
        vec![link(ROOT, "edge")],
        false,
        u64::from(u32::MAX),
        1,
    ));
    let idx = build(recs, opts());
    let e = idx.lookup(r(99, 1)).unwrap();
    assert_eq!(idx.own_logical(e), big);
    assert_eq!(idx.own_allocated(e), big + 4096);
    assert_eq!(
        idx.own_logical(idx.lookup(r(100, 1)).unwrap()),
        u64::from(u32::MAX)
    );
    assert_eq!(
        idx.aggregate(idx.root()).unwrap().logical,
        10_100 + (1 << 20) + big + u64::from(u32::MAX)
    );
}

#[test]
fn missing_root_gets_a_virtual_one() {
    let recs: Vec<_> = sample().into_iter().skip(1).collect();
    let idx = build(recs, opts());
    idx.check_invariants().unwrap();
    assert!(idx.flags(idx.root()).contains(EntryFlags::VIRTUAL));
    // Everything that pointed at the missing root is an orphan.
    let users = idx.lookup(r(30, 1)).unwrap();
    assert_eq!(idx.parent(users), Some(idx.orphans_node()));
    assert_eq!(idx.aggregate(idx.root()).unwrap().files, 3);
}

#[test]
fn reserved_reference_is_refused() {
    let mut b = IndexBuilder::new(opts());
    let mut rec = root_rec();
    rec.id = strata_core::FileRef(u64::MAX);
    assert_eq!(b.push(rec), Err(IndexError::ReservedFileRef(u64::MAX)));
}

#[test]
fn lite_mode_drops_times_but_keeps_aggregates() {
    let lite = build(
        sample(),
        IndexOptions {
            lite: true,
            ..opts()
        },
    );
    let full = build(sample(), opts());
    let e = lite.lookup(r(40, 1)).unwrap();
    assert!(lite.times(e).is_none());
    assert_eq!(
        lite.aggregate(lite.root()).unwrap(),
        full.aggregate(full.root()).unwrap()
    );
    assert!(lite.memory_report().columns < full.memory_report().columns);
    lite.check_invariants().unwrap();
}

#[test]
fn directory_overhead_and_ads_count_toward_allocated() {
    let mut recs = sample();
    let mut d = dir(r(101, 1), ROOT, "big-dir");
    d.sizes = Sizes {
        dir_overhead: 8192,
        ..Sizes::default()
    };
    let mut f = file(r(102, 1), r(101, 1), "with-ads", 10);
    f.sizes.ads_logical = 26;
    f.sizes.ads_allocated = 0;
    f.ads.push(strata_core::AdsInfo {
        name: WideName::from_str_lossless("Zone.Identifier"),
        logical: 26,
        allocated: 0,
    });
    recs.push(d);
    recs.push(f);
    let idx = build(recs, opts());
    let d = idx.aggregate(idx.lookup(r(101, 1)).unwrap()).unwrap();
    assert_eq!((d.logical, d.allocated), (36, 8192 + 4096));
    assert!(
        idx.flags(idx.lookup(r(102, 1)).unwrap())
            .contains(EntryFlags::HAS_ADS)
    );
}

#[test]
fn memory_budget_on_a_small_tree() {
    let mut recs = vec![root_rec()];
    let mut next = 100u64;
    for d in 0..2_000u64 {
        let id = r(next, 1);
        next += 1;
        recs.push(dir(id, ROOT, &format!("dir{d}")));
        for f in 0..20 {
            recs.push(file(r(next, 1), id, &format!("file{f}.dat"), f * 100));
            next += 1;
        }
    }
    let idx = build(recs, opts());
    let m = idx.memory_report();
    assert!(
        m.bytes_per_entry() <= 64.0,
        "{m:?} → {}",
        m.bytes_per_entry()
    );
}
