//! Shared proptest strategies and helpers for the layout invariant suites.

#![allow(dead_code)]

use proptest::prelude::*;
use std::collections::HashMap;
use strata_layout::{AggregateRecord, LayoutSource, NodeId, RecordBuf, VecTree};

/// One generated node: (parent selector, size, is_dir, color).
pub type Spec = (u16, u64, bool, u8);

fn size_strategy() -> impl Strategy<Value = u64> {
    prop_oneof![
        1 => Just(0u64),
        4 => 1u64..1_000,
        3 => 1u64..10_000_000,
        1 => 1u64..(1u64 << 42),
    ]
}

/// Random trees of up to `max` nodes with a mix of shapes: bushy, deep,
/// zero-size files, empty directories and huge size ranges.
pub fn tree_strategy(max: usize) -> impl Strategy<Value = VecTree> {
    prop::collection::vec(
        (
            any::<u16>(),
            size_strategy(),
            prop::bool::weighted(0.25),
            any::<u8>(),
        ),
        0..max,
    )
    .prop_map(build)
}

pub fn build(specs: Vec<Spec>) -> VecTree {
    let mut t = VecTree::new(0);
    let mut dirs: Vec<NodeId> = vec![VecTree::ROOT];
    for (sel, size, dir, color) in specs {
        let parent = dirs[sel as usize % dirs.len()];
        if dir {
            dirs.push(t.add_dir(parent, u32::from(color)));
        } else {
            t.add_file(parent, size, u32::from(color));
        }
    }
    t
}

/// All children of `id` and their byte total.
pub fn kids(t: &VecTree, id: NodeId) -> (Vec<(NodeId, u64)>, u128) {
    let mut v = Vec::new();
    t.children(id, &mut v);
    let total = v.iter().map(|&(_, s)| u128::from(s)).sum();
    (v, total)
}

/// Aggregate side-table entries keyed by the directory's record index.
pub fn aggregates_by_parent(buf: &RecordBuf<AggregateRecord>) -> HashMap<u32, AggregateRecord> {
    let mut m = HashMap::new();
    for a in buf.iter() {
        assert!(
            m.insert(a.parent, a).is_none(),
            "two aggregates for one dir"
        );
    }
    m
}

/// Children record indices per parent record index.
pub fn children_of(parents: impl Iterator<Item = u32>) -> HashMap<u32, Vec<usize>> {
    let mut m: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, p) in parents.enumerate() {
        m.entry(p).or_default().push(i);
    }
    m
}

/// Checks that emitted children plus the aggregate account for every child
/// and every byte of directory `dir_id`.
pub fn check_accounting(
    t: &VecTree,
    dir_id: NodeId,
    emitted: &[(NodeId, bool)],
    agg: Option<&AggregateRecord>,
) {
    let (all, total) = kids(t, dir_id);
    let sizes: HashMap<NodeId, u64> = all.iter().copied().collect();
    let real: Vec<NodeId> = emitted.iter().filter(|e| !e.1).map(|e| e.0).collect();
    let emitted_bytes: u128 = real.iter().map(|id| u128::from(sizes[id])).sum();
    let agg_bytes = agg.map_or(0, |a| u128::from(a.bytes));
    let agg_count = agg.map_or(0, |a| a.count as usize);
    assert_eq!(emitted_bytes + agg_bytes, total, "bytes for dir {dir_id}");
    assert_eq!(real.len() + agg_count, all.len(), "count for dir {dir_id}");
}
