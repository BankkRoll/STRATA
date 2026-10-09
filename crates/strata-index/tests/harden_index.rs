//! Edge cases: deep partial/denied propagation through live updates,
//! hardlinks with thousands of names, case-variant names, very deep Unicode
//! paths, large delete/rename churn checked against a fresh build,
//! suspicious timestamps in live updates, and search past the extension
//! table's capacity.

mod common;

use std::collections::BTreeMap;

use common::*;
use strata_core::{EntryFlags, FileRef, FileTime, NameLink, ScanRecord, SizeMode, WideName};
use strata_index::search::{CancelToken, Query, SearchOptions};
use strata_index::{Index, Update};

fn search(idx: &Index, q: &str) -> Vec<String> {
    let q = Query::parse(q, now()).unwrap();
    let out = idx
        .search(&q, &SearchOptions::default(), &CancelToken::new(), &|_| {})
        .unwrap();
    let mut v: Vec<String> = out.hits.iter().map(|h| idx.path_string(h.id)).collect();
    v.sort();
    v
}

#[test]
fn denied_folder_deep_in_the_tree_marks_every_ancestor_partial_and_clears() {
    let mut recs = vec![root_rec()];
    let mut parent = ROOT;
    for d in 0..60u64 {
        let id = r(100 + d, 1);
        recs.push(dir(id, parent, &format!("d{d}")));
        parent = id;
    }
    recs.push(dir(r(500, 1), ROOT, "sibling"));
    let mut idx = build(recs.clone(), opts());
    let deepest = r(159, 1);
    let mut denied = dir(deepest, r(158, 1), "d59");
    denied.flags |= EntryFlags::ACCESS_DENIED;
    idx.upsert(denied).unwrap();
    idx.check_invariants().unwrap();
    for d in 0..60u64 {
        let e = idx.lookup(r(100 + d, 1)).unwrap();
        assert!(idx.aggregate(e).unwrap().partial, "d{d}");
    }
    assert!(idx.aggregate(idx.root()).unwrap().partial);
    let sib = idx.lookup(r(500, 1)).unwrap();
    assert!(!idx.aggregate(sib).unwrap().partial);

    // Access restored: nothing stays partial.
    idx.upsert(dir(deepest, r(158, 1), "d59")).unwrap();
    for d in 0..60u64 {
        let e = idx.lookup(r(100 + d, 1)).unwrap();
        assert!(!idx.aggregate(e).unwrap().partial, "d{d} still partial");
    }
    assert_eq!(canonical(&idx), canonical(&build(recs, opts())));
}

fn many_links(n: usize) -> ScanRecord {
    let links: Vec<NameLink> = (0..n)
        .map(|i| link(r(1000 + (i as u64 % 40), 1), &format!("link-{i:05}.dll")))
        .collect();
    record(r(9000, 1), links, false, 10_000, 12_288)
}

#[test]
fn hardlinks_with_thousands_of_names_count_once() {
    let mut recs = vec![root_rec()];
    for d in 0..40u64 {
        recs.push(dir(r(1000 + d, 1), ROOT, &format!("dir{d:02}")));
    }
    recs.push(many_links(1500));
    let mut idx = build(recs.clone(), opts());
    idx.check_invariants().unwrap();
    let links = idx.links(r(9000, 1));
    assert_eq!(links.len(), 1500);
    let primary: Vec<_> = links
        .iter()
        .filter(|&&l| !idx.flags(l).contains(EntryFlags::HARDLINK_SECONDARY))
        .collect();
    assert_eq!(primary.len(), 1);
    assert_eq!(idx.name_lossy(*primary[0]), "link-00000.dll");
    let total = idx.aggregate(idx.root()).unwrap();
    assert_eq!(total.allocated, 12_288);
    assert_eq!(total.logical, 10_000);
    assert_eq!(total.files, 1500, "every name is a file entry");
    assert_eq!(idx.link_count(links[777]), 1500);

    // Dropping the first name hands the bytes to the next one.
    let mut fewer = many_links(1500);
    fewer.links.remove(0);
    idx.upsert(fewer.clone()).unwrap();
    idx.check_invariants().unwrap();
    let links = idx.links(r(9000, 1));
    assert_eq!(links.len(), 1499);
    let counted: u64 = links
        .iter()
        .map(|&l| idx.contribution(l, SizeMode::Allocated))
        .sum();
    assert_eq!(counted, 12_288);
    assert_eq!(idx.aggregate(idx.root()).unwrap().allocated, 12_288);
    *recs.last_mut().unwrap() = fewer;
    assert_eq!(canonical(&idx), canonical(&build(recs, opts())));
}

#[test]
fn case_variant_names_are_distinct_entries() {
    let recs = vec![
        root_rec(),
        dir(r(30, 1), ROOT, "dir"),
        file(r(40, 1), r(30, 1), "A.txt", 1),
        file(r(41, 1), r(30, 1), "a.txt", 2),
        file(r(42, 1), r(30, 1), "ǅ.txt", 3),
        file(r(43, 1), r(30, 1), "ǆ.txt", 4),
    ];
    let mut idx = build(recs, opts());
    idx.check_invariants().unwrap();
    let a_up = idx.lookup(r(40, 1)).unwrap();
    let a_lo = idx.lookup(r(41, 1)).unwrap();
    assert_ne!(a_up, a_lo);
    assert_eq!(idx.path_string(a_up), r"\dir\A.txt");
    assert_eq!(idx.path_string(a_lo), r"\dir\a.txt");
    assert_eq!(idx.aggregate(idx.root()).unwrap().files, 4);
    assert_eq!(search(&idx, "a.txt"), vec![r"\dir\A.txt", r"\dir\a.txt"]);
    assert_eq!(search(&idx, "a.txt case:yes"), vec![r"\dir\a.txt"]);
    assert_eq!(search(&idx, "A.txt case:yes"), vec![r"\dir\A.txt"]);

    // Removing one variant never touches the other.
    idx.remove(r(40, 1)).unwrap();
    assert!(idx.lookup(r(40, 1)).is_none());
    assert_eq!(
        idx.path_string(idx.lookup(r(41, 1)).unwrap()),
        r"\dir\a.txt"
    );
    // Renaming one onto the other's spelling keeps both entries.
    idx.upsert(file(r(43, 1), r(30, 1), "a.txt", 4)).unwrap();
    idx.check_invariants().unwrap();
    assert_eq!(search(&idx, "a.txt case:yes").len(), 2);
}

fn unicode_name(level: usize) -> WideName {
    let base = match level % 5 {
        0 => "🦀🦀",
        1 => "שלום-مرحبا",
        2 => "trailing. ",
        3 => " leading",
        _ => "ÄÖÜ-ß-ǅ",
    };
    let mut units: Vec<u16> = base.encode_utf16().collect();
    units.extend(format!("-{level}").encode_utf16());
    if level.is_multiple_of(7) {
        units.push(0xDC00);
    }
    WideName::from_units(units)
}

#[test]
fn very_deep_unicode_paths_build_search_and_resolve() {
    let mut recs = vec![root_rec()];
    let mut parent = ROOT;
    let mut expected: Vec<u16> = Vec::new();
    for level in 0..2800usize {
        let id = r(100 + level as u64, 1);
        let name = unicode_name(level);
        expected.push(u16::from(b'\\'));
        expected.extend_from_slice(name.units());
        recs.push(record(id, vec![NameLink { parent, name }], true, 0, 0));
        parent = id;
    }
    recs.push(file(r(5000, 1), parent, "leaf-🦀.bin", 7));
    expected.extend("\\leaf-🦀.bin".encode_utf16());
    assert!(expected.len() > 32_767, "deeper than any Win32 path");
    let mut idx = build(recs, opts());
    idx.check_invariants().unwrap();
    let leaf = idx.lookup(r(5000, 1)).unwrap();
    assert_eq!(idx.path(leaf).units(), &expected[..]);
    assert_eq!(search(&idx, "leaf-🦀").len(), 1);
    assert_eq!(search(&idx, "שלום").len(), 560);
    assert_eq!(search(&idx, "*-2799\\leaf-🦀.bin").len(), 1);
    assert_eq!(idx.aggregate(idx.root()).unwrap().logical, 7);

    // Moving the deep subtree to the root updates every cached path.
    let mid = r(100 + 1400, 1);
    idx.upsert(record(mid, vec![link(ROOT, "moved")], true, 0, 0))
        .unwrap();
    idx.check_invariants().unwrap();
    let p = idx.path_string(leaf);
    assert!(p.starts_with(r"\moved\"), "{}", &p[..40]);
}

/// Deterministic xorshift so the churn is reproducible without proptest.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn delete_and_rename_churn_matches_a_fresh_build() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut model: BTreeMap<u64, ScanRecord> = BTreeMap::new();
    model.insert(ROOT.0, root_rec());
    let dirs: Vec<FileRef> = (0..30).map(|d| r(100 + d, 1)).collect();
    for (i, &d) in dirs.iter().enumerate() {
        let parent = if i < 5 { ROOT } else { dirs[i % 5] };
        model.insert(d.0, dir(d, parent, &format!("dir{i}")));
    }
    let mut next_file = 10_000u64;
    let mut idx = build(model.values().cloned(), opts());
    for step in 0..6000u32 {
        let pick_dir = |rng: &mut Rng| dirs[rng.below(dirs.len() as u64) as usize];
        let files: Vec<u64> = model
            .keys()
            .copied()
            .filter(|k| !dirs.iter().any(|d| d.0 == *k) && *k != ROOT.0)
            .collect();
        let update = match rng.below(10) {
            0..=3 => {
                let id = r(next_file, 1);
                next_file += 1;
                let rec = file(
                    id,
                    pick_dir(&mut rng),
                    &format!("f{step}.dat"),
                    rng.below(1 << 20),
                );
                model.insert(id.0, rec.clone());
                Update::Upsert(rec)
            }
            4 | 5 if !files.is_empty() => {
                let k = files[rng.below(files.len() as u64) as usize];
                let mut rec = model[&k].clone();
                rec.links[0].name = WideName::from_str_lossless(&format!("ren{step}.Dat"));
                if rng.below(2) == 0 {
                    rec.links[0].parent = pick_dir(&mut rng);
                }
                model.insert(k, rec.clone());
                Update::Upsert(rec)
            }
            6 | 7 if !files.is_empty() => {
                let k = files[rng.below(files.len() as u64) as usize];
                model.remove(&k);
                Update::Remove(FileRef(k))
            }
            8 if !files.is_empty() => {
                // Delete and immediately reuse the record with a new sequence.
                let k = files[rng.below(files.len() as u64) as usize];
                let old = model.remove(&k).unwrap();
                let reused = r(FileRef(k).record(), old.id.sequence().wrapping_add(1));
                let rec = file(reused, pick_dir(&mut rng), &format!("reuse{step}"), 5);
                model.insert(reused.0, rec.clone());
                idx.apply([Update::Remove(FileRef(k))]).unwrap();
                Update::Upsert(rec)
            }
            _ => {
                // Rename a directory (its subtree moves with it).
                let d = dirs[5 + rng.below(25) as usize];
                let mut rec = model[&d.0].clone();
                rec.links[0].name = WideName::from_str_lossless(&format!("dir-r{step}"));
                model.insert(d.0, rec.clone());
                Update::Upsert(rec)
            }
        };
        idx.apply([update]).unwrap();
        if step % 1500 == 0 {
            idx.check_invariants().unwrap();
        }
        if step == 3000 {
            idx.compact();
        }
    }
    idx.check_invariants().unwrap();
    assert_eq!(
        canonical(&idx),
        canonical(&build(model.into_values(), opts()))
    );
}

#[test]
fn suspicious_times_in_live_updates_follow_the_reference_clock() {
    let mut idx = build(vec![root_rec(), dir(r(30, 1), ROOT, "d")], opts());
    let flagged = |idx: &Index, id| {
        idx.flags(idx.lookup(id).unwrap())
            .contains(EntryFlags::SUSPICIOUS_TIME)
    };
    let at = |id, secs: i64| {
        let mut f = file(id, r(30, 1), &format!("f{}", id.record()), 1);
        f.times = times(secs);
        f
    };
    let now_secs = now().to_unix_secs();
    idx.upsert(at(r(40, 1), 600_000_000)).unwrap(); // 1989
    idx.upsert(at(r(41, 1), now_secs + 2 * 86_400)).unwrap();
    idx.upsert(at(r(42, 1), now_secs + 23 * 3600)).unwrap();
    idx.upsert(at(r(43, 1), 631_152_000)).unwrap(); // 1990-01-01
    assert!(flagged(&idx, r(40, 1)));
    assert!(flagged(&idx, r(41, 1)));
    assert!(!flagged(&idx, r(42, 1)));
    assert!(!flagged(&idx, r(43, 1)));

    // A week later the same "future" time is ordinary once the reference
    // clock moves, and stale flags clear on the next update.
    idx.set_now(FileTime::from_unix_secs(now_secs + 7 * 86_400));
    idx.upsert(at(r(41, 1), now_secs + 2 * 86_400)).unwrap();
    assert!(!flagged(&idx, r(41, 1)));
    // A zero time is "unknown", never suspicious.
    let mut zero = file(r(44, 1), r(30, 1), "zero", 1);
    zero.times = strata_core::Times::default();
    idx.upsert(zero).unwrap();
    assert!(!flagged(&idx, r(44, 1)));
}

#[test]
fn extensions_beyond_the_table_capacity_are_still_searchable() {
    let mut recs = vec![root_rec(), dir(r(30, 1), ROOT, "d")];
    // 70,000 distinct extensions; the table holds 65,535.
    for i in 0..70_000u64 {
        recs.push(file(r(100 + i, 1), r(30, 1), &format!("f{i}.x{i:05}"), 1));
    }
    let idx = build(recs, opts());
    idx.check_invariants().unwrap();
    for i in [0u64, 65_000, 65_534, 65_535, 69_999] {
        let hits = search(&idx, &format!("ext:x{i:05}"));
        assert_eq!(hits, vec![format!(r"\d\f{i}.x{i:05}")], "ext {i}");
    }
    assert_eq!(search(&idx, "ext:X69999").len(), 1, "case-insensitive");
    assert!(search(&idx, "ext:nope").is_empty());
    let all = idx.extension_breakdown(None, SizeMode::Logical);
    assert_eq!(all.iter().map(|b| b.files).sum::<u64>(), 70_000);
}
