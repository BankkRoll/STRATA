//! Live updates: targeted cases plus a property test (random
//! operation sequences applied live must equal a fresh build of the final
//! record set).

mod common;

use std::collections::BTreeMap;

use common::*;
use proptest::prelude::*;
use strata_core::{EntryFlags, FileRef, Reparse, ScanRecord, SizeMode, win32};
use strata_index::{Index, IndexError, IndexOptions, Update};

fn base() -> Vec<ScanRecord> {
    vec![
        root_rec(),
        dir(r(30, 1), ROOT, "Users"),
        dir(r(31, 1), r(30, 1), "me"),
        file(r(40, 1), r(31, 1), "a.bin", 10_000),
        file(r(41, 1), r(31, 1), "b.txt", 100),
    ]
}

fn assert_same(live: &Index, records: &[ScanRecord], opts: IndexOptions) {
    live.check_invariants().unwrap();
    let fresh = build(records.to_vec(), opts);
    if let Some(d) = diff(&canonical(live), &canonical(&fresh)) {
        panic!("live index differs from fresh build:\n{d}");
    }
}

#[test]
fn resize_reports_changed_ancestors() {
    let mut idx = build(base(), opts());
    let cs = idx
        .upsert(file(r(41, 1), r(31, 1), "b.txt", 50_000))
        .unwrap();
    let me = idx.lookup(r(31, 1)).unwrap();
    let dirs: Vec<_> = cs.aggregates.iter().map(|(d, _)| *d).collect();
    assert!(dirs.contains(&me) && dirs.contains(&idx.root()));
    assert_eq!(cs.updated, vec![idx.lookup(r(41, 1)).unwrap()]);
    let (_, agg) = cs.aggregates.iter().find(|(d, _)| *d == me).unwrap();
    assert_eq!(agg.logical, 60_000);
    assert_eq!(idx.name_lossy(agg.largest_logical.unwrap()), "b.txt");
    let mut recs = base();
    recs[4] = file(r(41, 1), r(31, 1), "b.txt", 50_000);
    assert_same(&idx, &recs, opts());
}

#[test]
fn rename_and_move_keep_ids() {
    let mut idx = build(base(), opts());
    let id = idx.lookup(r(40, 1)).unwrap();
    idx.upsert(file(r(40, 1), r(30, 1), "renamed.bin", 10_000))
        .unwrap();
    assert_eq!(idx.lookup(r(40, 1)), Some(id));
    assert_eq!(idx.path_string(id), r"\Users\renamed.bin");
    let me = idx.aggregate(idx.lookup(r(31, 1)).unwrap()).unwrap();
    assert_eq!(me.files, 1);
    let mut recs = base();
    recs[3] = file(r(40, 1), r(30, 1), "renamed.bin", 10_000);
    assert_same(&idx, &recs, opts());
}

#[test]
fn deleting_a_directory_orphans_children_until_it_returns() {
    let mut idx = build(base(), opts());
    idx.remove(r(31, 1)).unwrap();
    let a = idx.lookup(r(40, 1)).unwrap();
    assert_eq!(idx.parent(a), Some(idx.orphans_node()));
    let recs: Vec<_> = base().into_iter().filter(|x| x.id != r(31, 1)).collect();
    assert_same(&idx, &recs, opts());

    // Same record number, new sequence: the children's links are stale.
    idx.upsert(dir(r(31, 2), r(30, 1), "me")).unwrap();
    assert_eq!(idx.parent(a), Some(idx.orphans_node()));
    // The original directory comes back: children re-attach.
    idx.upsert(dir(r(31, 1), r(30, 1), "me")).unwrap();
    let me = idx.lookup(r(31, 1)).unwrap();
    assert_eq!(idx.parent(a), Some(me));
    assert_same(&idx, &base(), opts());
}

#[test]
fn moving_a_directory_into_itself_breaks_the_cycle() {
    let mut idx = build(base(), opts());
    idx.upsert(dir(r(30, 1), r(31, 1), "Users")).unwrap();
    for fr in [r(30, 1), r(31, 1)] {
        let e = idx.lookup(fr).unwrap();
        assert!(idx.flags(e).contains(EntryFlags::CYCLE_BROKEN));
        assert_eq!(idx.parent(e), Some(idx.orphans_node()));
    }
    let mut recs = base();
    recs[1] = dir(r(30, 1), r(31, 1), "Users");
    assert_same(&idx, &recs, opts());
    // Undo: the cycle heals.
    idx.upsert(dir(r(30, 1), ROOT, "Users")).unwrap();
    assert_same(&idx, &base(), opts());
}

#[test]
fn hardlink_add_and_remove() {
    for split in [false, true] {
        let o = IndexOptions {
            split_hardlinks: split,
            ..opts()
        };
        let mut idx = build(base(), o.clone());
        let mut rec = file(r(40, 1), r(31, 1), "a.bin", 10_000);
        rec.links.push(link(r(30, 1), "a-link.bin"));
        idx.upsert(rec.clone()).unwrap();
        assert_eq!(idx.links(r(40, 1)).len(), 2);
        let mut recs = base();
        recs[3] = rec.clone();
        assert_same(&idx, &recs, o.clone());

        // Drop the primary: the remaining link becomes primary.
        rec.links.remove(0);
        idx.upsert(rec.clone()).unwrap();
        let only = idx.links(r(40, 1));
        assert_eq!(only.len(), 1);
        assert!(!idx.flags(only[0]).contains(EntryFlags::HARDLINK_SECONDARY));
        assert_eq!(idx.contribution(only[0], SizeMode::Logical), 10_000);
        recs[3] = rec;
        assert_same(&idx, &recs, o);
    }
}

#[test]
fn becoming_a_junction_orphans_children() {
    let mut idx = build(base(), opts());
    let mut j = dir(r(31, 1), r(30, 1), "me");
    j.reparse = Some(Reparse {
        tag: win32::IO_REPARSE_TAG_MOUNT_POINT,
        target: None,
    });
    idx.upsert(j.clone()).unwrap();
    let a = idx.lookup(r(40, 1)).unwrap();
    assert_eq!(idx.parent(a), Some(idx.orphans_node()));
    let mut recs = base();
    recs[2] = j;
    assert_same(&idx, &recs, opts());
    idx.upsert(dir(r(31, 1), r(30, 1), "me")).unwrap();
    assert_same(&idx, &base(), opts());
}

#[test]
fn stale_remove_is_ignored_and_root_is_protected() {
    let mut idx = build(base(), opts());
    let cs = idx.remove(r(40, 7)).unwrap();
    assert!(cs.is_empty());
    assert_eq!(idx.remove(ROOT), Err(IndexError::RootRemoval));
    assert_same(&idx, &base(), opts());
}

#[test]
fn batch_apply_merges_changes_and_reuses_slots() {
    let mut idx = build(base(), opts());
    let cs = idx
        .apply([
            Update::Upsert(file(r(200, 1), r(31, 1), "new1", 1)),
            Update::Upsert(file(r(201, 1), r(31, 1), "new2", 2)),
            Update::Remove(r(200, 1)),
        ])
        .unwrap();
    assert_eq!(cs.created.len(), 1);
    assert!(
        cs.removed.is_empty(),
        "created-then-removed is not reported"
    );
    let slots = idx.slot_count();
    idx.upsert(file(r(202, 1), r(31, 1), "new3", 3)).unwrap();
    assert_eq!(idx.slot_count(), slots, "freed delta slot reused");
    let mut recs = base();
    recs.push(file(r(201, 1), r(31, 1), "new2", 2));
    recs.push(file(r(202, 1), r(31, 1), "new3", 3));
    assert_same(&idx, &recs, opts());
}

#[test]
fn virtual_blocks_update_root_totals() {
    let mut idx = build(base(), opts());
    let before = idx.aggregate(idx.root()).unwrap().allocated;
    let (b, _) = idx.add_virtual_block("System Restore / Shadow copies", 0, 1 << 30);
    assert_eq!(
        idx.aggregate(idx.root()).unwrap().allocated,
        before + (1 << 30)
    );
    idx.set_virtual_block(b, 0, 1 << 20).unwrap();
    assert_eq!(
        idx.aggregate(idx.root()).unwrap().allocated,
        before + (1 << 20)
    );
    idx.remove_virtual_block(b).unwrap();
    assert_eq!(idx.aggregate(idx.root()).unwrap().allocated, before);
    assert!(idx.set_virtual_block(idx.root(), 0, 0).is_err());
    idx.check_invariants().unwrap();
}

#[test]
fn compaction_preserves_everything() {
    let mut idx = build(base(), opts());
    idx.remove(r(41, 1)).unwrap();
    idx.upsert(file(r(300, 1), r(30, 1), "late.bin", 77))
        .unwrap();
    idx.upsert(file(r(40, 1), r(31, 1), "a-renamed.bin", 10_000))
        .unwrap();
    let before = canonical(&idx);
    let old = idx.lookup(r(300, 1)).unwrap();
    assert_eq!(idx.tombstones(), 1);
    let map = idx.compact();
    assert_eq!(idx.tombstones(), 0);
    assert_eq!(map.get(old), idx.lookup(r(300, 1)));
    assert_eq!(canonical(&idx), before);
    idx.check_invariants().unwrap();
    // Live updates keep working on the compacted layout.
    idx.upsert(file(r(301, 1), r(31, 1), "after.bin", 5))
        .unwrap();
    idx.check_invariants().unwrap();
}

// -----------------------------------------------------------------------------
// Property test
// -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    CreateFile {
        parent: u16,
        name: u8,
        size: u32,
    },
    CreateDir {
        parent: u16,
        name: u8,
    },
    Delete {
        target: u16,
    },
    Rename {
        target: u16,
        name: u8,
    },
    Move {
        target: u16,
        parent: u16,
    },
    Resize {
        target: u16,
        logical: u32,
        slack: u16,
    },
    Touch {
        target: u16,
        when: u8,
    },
    AddLink {
        target: u16,
        parent: u16,
        name: u8,
    },
    RemoveLink {
        target: u16,
        which: u8,
    },
    Reuse {
        target: u16,
        as_dir: bool,
    },
    Flag {
        target: u16,
        which: u8,
    },
    Dangle {
        target: u16,
        stale: bool,
    },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (any::<u16>(), any::<u8>(), any::<u32>())
            .prop_map(|(parent, name, size)| Op::CreateFile { parent, name, size }),
        3 => (any::<u16>(), any::<u8>()).prop_map(|(parent, name)| Op::CreateDir { parent, name }),
        2 => any::<u16>().prop_map(|target| Op::Delete { target }),
        2 => (any::<u16>(), any::<u8>()).prop_map(|(target, name)| Op::Rename { target, name }),
        3 => (any::<u16>(), any::<u16>()).prop_map(|(target, parent)| Op::Move { target, parent }),
        3 => (any::<u16>(), any::<u32>(), any::<u16>())
            .prop_map(|(target, logical, slack)| Op::Resize { target, logical, slack }),
        2 => (any::<u16>(), any::<u8>()).prop_map(|(target, when)| Op::Touch { target, when }),
        2 => (any::<u16>(), any::<u16>(), any::<u8>())
            .prop_map(|(target, parent, name)| Op::AddLink { target, parent, name }),
        1 => (any::<u16>(), any::<u8>()).prop_map(|(target, which)| Op::RemoveLink { target, which }),
        1 => (any::<u16>(), any::<bool>()).prop_map(|(target, as_dir)| Op::Reuse { target, as_dir }),
        1 => (any::<u16>(), any::<u8>()).prop_map(|(target, which)| Op::Flag { target, which }),
        1 => (any::<u16>(), any::<bool>()).prop_map(|(target, stale)| Op::Dangle { target, stale }),
    ]
}

const NAMES: [&str; 8] = [
    "a",
    "b",
    "A",
    "x.txt",
    "y.LOG",
    "z.bin",
    "Ω.dat",
    "node_modules",
];
const WHEN: [i64; 6] = [
    1_600_000_000,
    1_700_000_000,
    1_750_000_000,
    315_532_800,
    4_000_000_000,
    0,
];

struct Model {
    recs: BTreeMap<u64, ScanRecord>,
    next: u64,
}

impl Model {
    fn new() -> Self {
        let mut recs = BTreeMap::new();
        for r in base() {
            recs.insert(r.id.record(), r);
        }
        Self { recs, next: 1000 }
    }

    fn records(&self) -> Vec<ScanRecord> {
        self.recs.values().cloned().collect()
    }

    fn pick(&self, k: u16, pred: impl Fn(&ScanRecord) -> bool) -> Option<u64> {
        let v: Vec<u64> = self
            .recs
            .values()
            .filter(|r| pred(r))
            .map(|r| r.id.record())
            .collect();
        (!v.is_empty()).then(|| v[k as usize % v.len()])
    }

    fn target(&self, k: u16) -> Option<u64> {
        self.pick(k, |r| r.id != ROOT)
    }

    fn dir_ref(&self, k: u16) -> FileRef {
        let rec = self.pick(k, ScanRecord::is_dir).expect("root is a dir");
        self.recs[&rec].id
    }

    /// Applies `op` to the model and returns the live update to replay.
    fn apply(&mut self, op: &Op) -> Option<Update> {
        let name = |n: u8| NAMES[n as usize % NAMES.len()];
        match *op {
            Op::CreateFile {
                parent,
                name: n,
                size,
            } => {
                let id = r(self.next, 1);
                self.next += 1;
                let rec = file(id, self.dir_ref(parent), name(n), u64::from(size % 100_000));
                self.recs.insert(id.record(), rec.clone());
                Some(Update::Upsert(rec))
            }
            Op::CreateDir { parent, name: n } => {
                let id = r(self.next, 1);
                self.next += 1;
                let rec = dir(id, self.dir_ref(parent), name(n));
                self.recs.insert(id.record(), rec.clone());
                Some(Update::Upsert(rec))
            }
            Op::Delete { target } => {
                let t = self.target(target)?;
                let rec = self.recs.remove(&t)?;
                Some(Update::Remove(rec.id))
            }
            Op::Rename { target, name: n } => {
                let t = self.target(target)?;
                let rec = self.recs.get_mut(&t)?;
                if let Some(l) = rec.links.first_mut() {
                    l.name = strata_core::WideName::from_str_lossless(name(n));
                }
                Some(Update::Upsert(rec.clone()))
            }
            Op::Move { target, parent } => {
                let t = self.target(target)?;
                let p = self.dir_ref(parent);
                let rec = self.recs.get_mut(&t)?;
                match rec.links.first_mut() {
                    Some(l) => l.parent = p,
                    None => rec.links.push(link(p, "revived")),
                }
                Some(Update::Upsert(rec.clone()))
            }
            Op::Resize {
                target,
                logical,
                slack,
            } => {
                let t = self.target(target)?;
                let rec = self.recs.get_mut(&t)?;
                rec.sizes.logical = u64::from(logical % 1_000_000);
                rec.sizes.allocated = rec.sizes.logical + u64::from(slack);
                if rec.is_dir() {
                    rec.sizes.dir_overhead = u64::from(slack);
                }
                Some(Update::Upsert(rec.clone()))
            }
            Op::Touch { target, when } => {
                let t = self.target(target)?;
                let rec = self.recs.get_mut(&t)?;
                rec.times = times(WHEN[when as usize % WHEN.len()]);
                if WHEN[when as usize % WHEN.len()] == 0 {
                    rec.times = strata_core::Times::default();
                }
                Some(Update::Upsert(rec.clone()))
            }
            Op::AddLink {
                target,
                parent,
                name: n,
            } => {
                let t = self.pick(target, |r| !r.is_dir())?;
                let p = self.dir_ref(parent);
                let rec = self.recs.get_mut(&t)?;
                rec.links.push(link(p, name(n)));
                Some(Update::Upsert(rec.clone()))
            }
            Op::RemoveLink { target, which } => {
                let t = self.pick(target, |r| r.links.len() > 1)?;
                let rec = self.recs.get_mut(&t)?;
                let i = which as usize % rec.links.len();
                rec.links.remove(i);
                Some(Update::Upsert(rec.clone()))
            }
            Op::Reuse { target, as_dir } => {
                let t = self.target(target)?;
                let old = self.recs.get(&t)?.clone();
                let id = FileRef::from_parts(t, old.id.sequence().wrapping_add(1).max(1));
                let parent = old.links.first().map_or(ROOT, |l| l.parent);
                let rec = if as_dir {
                    dir(id, parent, "reused")
                } else {
                    file(id, parent, "reused", 42)
                };
                self.recs.insert(t, rec.clone());
                Some(Update::Upsert(rec))
            }
            Op::Flag { target, which } => {
                let t = self.target(target)?;
                let rec = self.recs.get_mut(&t)?;
                match which % 5 {
                    0 => rec.flags.0 ^= EntryFlags::PARTIAL.0,
                    1 => rec.flags.0 ^= EntryFlags::ACCESS_DENIED.0,
                    2 => rec.flags.0 ^= EntryFlags::HIDDEN.0,
                    3 => rec.flags.0 ^= EntryFlags::NTFS_METADATA.0,
                    _ => {
                        rec.reparse = match rec.reparse {
                            Some(_) => None,
                            None => Some(Reparse {
                                tag: win32::IO_REPARSE_TAG_MOUNT_POINT,
                                target: None,
                            }),
                        };
                        rec.flags = rec.flags.with_reparse(strata_core::ReparseKind::None);
                    }
                }
                Some(Update::Upsert(rec.clone()))
            }
            Op::Dangle { target, stale } => {
                let t = self.target(target)?;
                let rec = self.recs.get_mut(&t)?;
                let l = rec.links.first_mut()?;
                l.parent = if stale {
                    FileRef::from_parts(l.parent.record(), l.parent.sequence().wrapping_add(7))
                } else {
                    r(9_999_999, 1)
                };
                Some(Update::Upsert(rec.clone()))
            }
        }
    }
}

fn run(ops: &[Op], split: bool, check_every: bool) -> Result<(), TestCaseError> {
    let o = IndexOptions {
        split_hardlinks: split,
        ..opts()
    };
    let mut model = Model::new();
    let mut live = build(model.records(), o.clone());
    for (i, op) in ops.iter().enumerate() {
        if let Some(u) = model.apply(op) {
            live.apply([u])
                .map_err(|e| TestCaseError::fail(format!("op {i} {op:?}: {e}")))?;
        }
        if check_every {
            live.check_invariants()
                .map_err(|e| TestCaseError::fail(format!("after op {i} {op:?}: {e}")))?;
            let fresh = build(model.records(), o.clone());
            if let Some(d) = diff(&canonical(&live), &canonical(&fresh)) {
                return Err(TestCaseError::fail(format!("after op {i} {op:?}:\n{d}")));
            }
        }
    }
    live.check_invariants().map_err(TestCaseError::fail)?;
    let fresh = build(model.records(), o);
    let want = canonical(&fresh);
    if let Some(d) = diff(&canonical(&live), &want) {
        return Err(TestCaseError::fail(d));
    }
    // Compaction and a cache round trip preserve the live state.
    let bytes = live.to_bytes();
    live.compact();
    live.check_invariants().map_err(TestCaseError::fail)?;
    prop_assert!(diff(&canonical(&live), &want).is_none(), "after compaction");
    let loaded = Index::from_bytes(&bytes).map_err(|e| TestCaseError::fail(e.to_string()))?;
    loaded.check_invariants().map_err(TestCaseError::fail)?;
    prop_assert!(
        diff(&canonical(&loaded), &want).is_none(),
        "after cache round trip"
    );
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(400), ..ProptestConfig::default() })]

    #[test]
    fn live_updates_equal_fresh_build(ops in proptest::collection::vec(op(), 1..60)) {
        run(&ops, false, false)?;
    }

    #[test]
    fn live_updates_equal_fresh_build_split(ops in proptest::collection::vec(op(), 1..60)) {
        run(&ops, true, false)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(64), ..ProptestConfig::default() })]

    #[test]
    fn every_intermediate_state_matches(ops in proptest::collection::vec(op(), 1..25)) {
        run(&ops, false, true)?;
    }

    #[test]
    fn every_intermediate_state_matches_split(ops in proptest::collection::vec(op(), 1..25)) {
        run(&ops, true, true)?;
    }
}

/// Case count, overridable with `STRATA_PROPTEST_CASES` for long soak runs.
fn cases(default: u32) -> u32 {
    std::env::var("STRATA_PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
fn regression_reuse_then_links() {
    let ops = [
        Op::Reuse {
            target: 3900,
            as_dir: false,
        },
        Op::CreateDir { parent: 0, name: 0 },
        Op::CreateDir { parent: 0, name: 0 },
        Op::AddLink {
            target: 5065,
            parent: 6590,
            name: 0,
        },
        Op::AddLink {
            target: 6807,
            parent: 55874,
            name: 0,
        },
    ];
    if let Err(e) = run(&ops, false, true) {
        panic!("{e}");
    }
}
