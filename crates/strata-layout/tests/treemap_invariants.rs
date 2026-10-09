//! Treemap invariants on random trees (SPEC Ã‚Â§22): area proportional to size
//! within the parent's content box, siblings never overlap, children stay
//! inside their parent, output is deterministic, LOD accounts for every
//! excluded byte, nothing is NaN, and picking returns the deepest rect.

mod common;

use common::*;
use proptest::prelude::*;
use strata_layout::{
    CushionParams, Hierarchy, HitGrid, LayoutSource, NodeFlags, RectRecord, TreemapConfig, VecTree,
    layout_treemap,
};

fn config_strategy() -> impl Strategy<Value = TreemapConfig> {
    (
        1.0f32..2500.0,
        1.0f32..1500.0,
        0.5f32..3.0,
        0.25f32..4.0,
        any::<bool>(),
        1u16..40,
    )
        .prop_map(|(w, h, dpi, min_px, cushion, depth)| {
            let mut c = TreemapConfig::new(w, h, dpi);
            c.min_px = min_px;
            c.max_depth = depth;
            if cushion {
                c.cushion = Some(CushionParams::default());
            }
            c
        })
}

/// Content box of a laid-out directory record, recomputed from its flags.
fn content(r: &RectRecord, c: &TreemapConfig) -> (f64, f64, f64, f64) {
    let pad = f64::from(c.padding);
    let header = if r.flags.contains(NodeFlags::HAS_HEADER) {
        f64::from(c.header_height)
    } else {
        0.0
    };
    let x0 = f64::from(r.x) + pad;
    let y0 = f64::from(r.y) + pad + header;
    let x1 = f64::from(r.x) + f64::from(r.w) - pad;
    let y1 = f64::from(r.y) + f64::from(r.h) - pad;
    (x0, y0, x1, y1)
}

/// Overlap area, ignoring sub-1e-3 px slivers that come from rounding
/// `x + w` back to `f32` (shared edges are exact in the buffer).
fn overlap(a: &RectRecord, b: &RectRecord) -> f64 {
    let w = f64::from(a.x + a.w).min(f64::from(b.x + b.w)) - f64::from(a.x.max(b.x));
    let h = f64::from(a.y + a.h).min(f64::from(b.y + b.h)) - f64::from(a.y.max(b.y));
    (w - 1e-3).max(0.0) * (h - 1e-3).max(0.0)
}

fn check(t: &VecTree, c: &TreemapConfig) {
    let l = layout_treemap(t, VecTree::ROOT, c);
    let recs: Vec<RectRecord> = l.rects().iter().collect();
    let min_px = c.min_px;
    let tol = 0.02;

    for (i, r) in recs.iter().enumerate() {
        assert!(r.rect().is_valid(), "record {i} not finite: {r:?}");
        assert!(
            !r.flags.contains(NodeFlags::CLIPPED),
            "identity view never clips"
        );
        if i > 0 {
            assert!(
                r.w >= min_px - 1e-3 && r.h >= min_px - 1e-3,
                "below LOD: {r:?}"
            );
            assert!((r.parent as usize) < i, "parent must precede child");
        }
    }
    if let Some(cushions) = l.cushions() {
        assert_eq!(cushions.len(), recs.len());
    }

    let aggs = aggregates_by_parent(l.aggregates());
    let by_parent = children_of(recs.iter().map(|r| r.parent));

    for (i, r) in recs.iter().enumerate() {
        let kids_idx = by_parent.get(&(i as u32)).cloned().unwrap_or_default();
        if !r.flags.contains(NodeFlags::DIR) || r.flags.contains(NodeFlags::TRUNCATED) {
            assert!(kids_idx.is_empty(), "leaf or truncated with children");
            assert!(!aggs.contains_key(&(i as u32)));
            continue;
        }
        // Subtree skip pointers bracket exactly the descendants.
        let end = l.subtree_end(i).unwrap();
        for &k in &kids_idx {
            assert!(k > i && k < end);
        }

        let emitted: Vec<_> = kids_idx
            .iter()
            .map(|&k| (recs[k].id, recs[k].flags.contains(NodeFlags::AGGREGATE)))
            .collect();
        check_accounting(t, r.id, &emitted, aggs.get(&(i as u32)));

        let (x0, y0, x1, y1) = content(r, c);
        let area = (x1 - x0) * (y1 - y0);
        let (_, total) = kids(t, r.id);
        for &k in &kids_idx {
            let ch = &recs[k];
            assert!(
                f64::from(ch.x) >= x0 - tol && f64::from(ch.y) >= y0 - tol,
                "{ch:?} outside {r:?}"
            );
            assert!(
                f64::from(ch.x + ch.w) <= x1 + tol && f64::from(ch.y + ch.h) <= y1 + tol,
                "{ch:?} outside {r:?}"
            );
            let bytes = if ch.flags.contains(NodeFlags::AGGREGATE) {
                u128::from(aggs[&(i as u32)].bytes)
            } else {
                u128::from(t.size(ch.id))
            };
            let expected = bytes as f64 / total as f64 * area;
            let actual = f64::from(ch.w) * f64::from(ch.h);
            let slack = 0.01 * (f64::from(ch.w) + f64::from(ch.h)) + 1e-4 * expected;
            assert!(
                (actual - expected).abs() <= slack,
                "area {actual} vs {expected} for {ch:?} in {r:?}"
            );
        }
        for (a, &ka) in kids_idx.iter().enumerate() {
            for &kb in &kids_idx[a + 1..] {
                let o = overlap(&recs[ka], &recs[kb]);
                assert!(
                    o <= 1e-3,
                    "siblings overlap by {o}: {:?} {:?}",
                    recs[ka],
                    recs[kb]
                );
            }
        }
    }

    // Picking returns a containing rect none of whose children contain the point.
    let grid = HitGrid::build(&l, 8.0);
    if let Some(root) = recs.first() {
        for k in 0..64 {
            let x = root.x + root.w * ((k * 37 % 64) as f32 + 0.5) / 64.0;
            let y = root.y + root.h * ((k * 11 % 64) as f32 + 0.5) / 64.0;
            let hit = l.hit_test(x, y).expect("inside root");
            let hr = &recs[hit as usize];
            assert!(hr.rect().contains(x, y));
            for &k in by_parent.get(&hit).map(Vec::as_slice).unwrap_or(&[]) {
                assert!(!recs[k].rect().contains(x, y), "not deepest");
            }
            assert_eq!(grid.hit_test(&l, x, y), Some(hit));
            let p = l.pick(x, y).unwrap();
            assert_eq!(p.ancestors.len(), hr.depth as usize);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn treemap_invariants(t in tree_strategy(300), c in config_strategy()) {
        check(&t, &c);
    }

    #[test]
    fn treemap_is_deterministic_and_order_independent(
        t in tree_strategy(300),
        c in config_strategy(),
        seed in any::<u64>(),
    ) {
        let a = layout_treemap(&t, VecTree::ROOT, &c);
        let b = layout_treemap(&t, VecTree::ROOT, &c);
        prop_assert_eq!(a.rects().as_bytes(), b.rects().as_bytes());
        let mut shuffled = t.clone();
        shuffled.shuffle_children(seed);
        let s = layout_treemap(&shuffled, VecTree::ROOT, &c);
        prop_assert_eq!(a.rects().as_bytes(), s.rects().as_bytes());
        prop_assert_eq!(a.aggregates().as_bytes(), s.aggregates().as_bytes());
        prop_assert_eq!(a.labels().as_bytes(), s.labels().as_bytes());
        prop_assert_eq!(a.cushions().map(|c| c.as_bytes()), s.cushions().map(|c| c.as_bytes()));
    }

    #[test]
    fn labels_point_at_real_records(t in tree_strategy(200), c in config_strategy()) {
        let l = layout_treemap(&t, VecTree::ROOT, &c);
        for lab in l.labels().iter() {
            let r = l.rects().get(lab.record as usize).unwrap();
            prop_assert_eq!(r.id, lab.id);
            prop_assert!(lab.w >= c.label_min_width - 1e-3 && lab.h >= c.label_min_height - 1e-3);
            prop_assert!(lab.x >= r.x - 1e-3 && lab.x + lab.w <= r.x + r.w + 1e-3);
            prop_assert!(lab.y >= r.y - 1e-3 && lab.y + lab.h <= r.y + r.h + 1e-3);
            prop_assert_eq!(l.id(lab.record as usize), Some(lab.id));
        }
    }
}
