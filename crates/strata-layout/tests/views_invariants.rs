//! Invariants for the M9 views on random trees: sunburst, icicle, circle
//! packing and mind map. Sizes are proportional, siblings never overlap,
//! children stay within their parent, LOD accounts for every byte, output
//! is deterministic and order-independent, and picking finds the records.

mod common;

use common::*;
use proptest::prelude::*;
use strata_layout::{
    ArcRecord, CircleRecord, Hierarchy, IcicleConfig, IcicleOrientation, LayoutSource,
    MindMapConfig, NodeFlags, PackConfig, RectRecord, SunburstConfig, VecTree, layout_icicle,
    layout_mindmap, layout_pack, layout_sunburst,
};

/// Every non-aggregate child's byte share, keyed by record index.
fn share(t: &VecTree, dir: u32, child_id: u32, agg_bytes: Option<u64>) -> f64 {
    let (_, total) = kids(t, dir);
    let bytes = agg_bytes.map_or_else(|| u128::from(t.size(child_id)), u128::from);
    bytes as f64 / total as f64
}

fn check_sunburst(t: &VecTree, c: &SunburstConfig) {
    let l = layout_sunburst(t, VecTree::ROOT, c);
    let recs: Vec<ArcRecord> = l.arcs().iter().collect();
    let aggs = aggregates_by_parent(l.aggregates());
    let by_parent = children_of(recs.iter().map(|r| r.parent));
    let tau = std::f32::consts::TAU;
    for (i, r) in recs.iter().enumerate() {
        assert!(r.a0.is_finite() && r.a1.is_finite() && r.r0.is_finite() && r.r1.is_finite());
        assert!(r.a0 >= -1e-6 && r.a1 <= tau + 1e-5 && r.a0 <= r.a1 && r.r0 < r.r1);
        if i > 0 {
            assert!((r.a1 - r.a0) * r.r1 >= c.min_px - 1e-3, "below LOD: {r:?}");
        }
        let Some(kids_idx) = by_parent.get(&(i as u32)) else {
            continue;
        };
        let emitted: Vec<_> = kids_idx
            .iter()
            .map(|&k| (recs[k].id, recs[k].flags.contains(NodeFlags::AGGREGATE)))
            .collect();
        check_accounting(t, r.id, &emitted, aggs.get(&(i as u32)));
        let span = f64::from(r.a1 - r.a0);
        let mut prev_end = r.a0;
        for &k in kids_idx {
            let ch = &recs[k];
            assert_eq!(ch.r0, r.r1);
            assert!(ch.a0 >= prev_end - 1e-5, "siblings overlap");
            assert!(ch.a1 <= r.a1 + 1e-5, "child leaves parent");
            prev_end = ch.a1;
            let agg = ch
                .flags
                .contains(NodeFlags::AGGREGATE)
                .then(|| aggs[&(i as u32)].bytes);
            let expected = share(t, r.id, ch.id, agg) * span;
            assert!(
                (f64::from(ch.a1 - ch.a0) - expected).abs() < 1e-4,
                "angle not proportional"
            );
        }
    }
    // Picking the middle of every reasonably large sector returns it.
    let (cx, cy) = l.center();
    for (i, r) in recs.iter().enumerate() {
        if (r.a1 - r.a0) * r.r0.max(1.0) < 2.0 || r.r1 - r.r0 < 2.0 {
            continue;
        }
        let a = (r.a0 + r.a1) / 2.0;
        let rad = (r.r0 + r.r1) / 2.0;
        let p = l.pick(cx + rad * a.sin(), cy - rad * a.cos()).expect("hit");
        assert_eq!(p.index as usize, i);
        assert_eq!(p.ancestors.len(), r.depth as usize);
    }
}

fn check_icicle(t: &VecTree, c: &IcicleConfig) {
    let l = layout_icicle(t, VecTree::ROOT, c);
    let recs: Vec<RectRecord> = l.rects().iter().collect();
    let aggs = aggregates_by_parent(l.aggregates());
    let by_parent = children_of(recs.iter().map(|r| r.parent));
    let row = c.row_height();
    for (i, r) in recs.iter().enumerate() {
        assert!(r.rect().is_valid());
        if i > 0 {
            assert!(r.w >= c.min_px - 1e-3);
        }
        let Some(kids_idx) = by_parent.get(&(i as u32)) else {
            continue;
        };
        let emitted: Vec<_> = kids_idx
            .iter()
            .map(|&k| (recs[k].id, recs[k].flags.contains(NodeFlags::AGGREGATE)))
            .collect();
        check_accounting(t, r.id, &emitted, aggs.get(&(i as u32)));
        let mut prev_end = r.x;
        for &k in kids_idx {
            let ch = &recs[k];
            let dy = match c.orientation {
                IcicleOrientation::TopDown => ch.y - r.y,
                IcicleOrientation::BottomUp => r.y - ch.y,
            };
            assert!((dy - row).abs() < 1e-2, "child not in next row");
            assert!(ch.x >= prev_end - 1e-3 && ch.x + ch.w <= r.x + r.w + 1e-2);
            prev_end = ch.x + ch.w;
            let agg = ch
                .flags
                .contains(NodeFlags::AGGREGATE)
                .then(|| aggs[&(i as u32)].bytes);
            let expected = share(t, r.id, ch.id, agg) * f64::from(r.w);
            assert!((f64::from(ch.w) - expected).abs() < 1e-2 + 1e-5 * expected);
        }
    }
    for (i, r) in recs.iter().enumerate() {
        if r.w >= 1.0 && r.h >= 1.0 {
            let p = l.pick(r.x + r.w / 2.0, r.y + r.h / 2.0).expect("hit");
            assert_eq!(p.index as usize, i);
        }
    }
}

fn check_pack(t: &VecTree, c: &PackConfig) {
    let l = layout_pack(t, VecTree::ROOT, c);
    let recs: Vec<CircleRecord> = l.circles().iter().collect();
    let aggs = aggregates_by_parent(l.aggregates());
    let by_parent = children_of(recs.iter().map(|r| r.parent));
    let dist = |a: &CircleRecord, b: &CircleRecord| {
        (f64::from(a.cx - b.cx).powi(2) + f64::from(a.cy - b.cy).powi(2)).sqrt()
    };
    for (i, r) in recs.iter().enumerate() {
        assert!(r.cx.is_finite() && r.cy.is_finite() && r.r.is_finite() && r.r >= 0.0);
        if i > 0 {
            assert!(2.0 * r.r >= c.min_px - 1e-3, "below LOD: {r:?}");
        }
        let Some(kids_idx) = by_parent.get(&(i as u32)) else {
            continue;
        };
        let emitted: Vec<_> = kids_idx
            .iter()
            .map(|&k| (recs[k].id, recs[k].flags.contains(NodeFlags::AGGREGATE)))
            .collect();
        check_accounting(t, r.id, &emitted, aggs.get(&(i as u32)));
        let tol = 1e-3 * f64::from(r.r) + 1e-3;
        let inner = f64::from(r.r) - f64::from(c.padding);
        for (a, &ka) in kids_idx.iter().enumerate() {
            let ch = &recs[ka];
            assert!(
                dist(ch, r) + f64::from(ch.r) <= inner + tol,
                "child escapes parent"
            );
            for &kb in &kids_idx[a + 1..] {
                let o = &recs[kb];
                assert!(
                    dist(ch, o) >= f64::from(ch.r + o.r) - tol,
                    "circles overlap: {ch:?} {o:?}"
                );
            }
        }
        // Radii (before the sibling gap) scale with Ã¢Ë†Å¡size.
        let real: Vec<_> = kids_idx
            .iter()
            .map(|&k| &recs[k])
            .filter(|k| !k.flags.contains(NodeFlags::AGGREGATE))
            .collect();
        if let Some(first) = real.first() {
            let k0 = f64::from(first.r + c.sibling_gap / 2.0) / (t.size(first.id) as f64).sqrt();
            for ch in &real {
                let k = f64::from(ch.r + c.sibling_gap / 2.0) / (t.size(ch.id) as f64).sqrt();
                assert!((k - k0).abs() <= 1e-3 * k0, "radius not Ã¢Ë†Â Ã¢Ë†Å¡size");
            }
        }
    }
    for (i, r) in recs.iter().enumerate() {
        if r.r < 1.0 {
            continue;
        }
        let p = l.pick(r.cx, r.cy).expect("center hits");
        // The hit is the record itself or one of its descendants.
        let end = l.subtree_end(i).unwrap();
        assert!((p.index as usize) >= i && (p.index as usize) < end);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(192))]

    #[test]
    fn sunburst_invariants(
        t in tree_strategy(300),
        w in 1.0f32..2000.0,
        h in 1.0f32..2000.0,
        depth in 0u16..10,
        min_px in 0.25f32..4.0,
    ) {
        let mut c = SunburstConfig::new(w, h, 1.0);
        c.max_depth = depth;
        c.min_px = min_px;
        check_sunburst(&t, &c);
    }

    #[test]
    fn icicle_invariants(
        t in tree_strategy(300),
        w in 1.0f32..2000.0,
        h in 1.0f32..2000.0,
        depth in 0u16..12,
        min_px in 0.25f32..4.0,
        flame in any::<bool>(),
    ) {
        let mut c = IcicleConfig::new(w, h, 1.0);
        c.max_depth = depth;
        c.min_px = min_px;
        if flame {
            c.orientation = IcicleOrientation::BottomUp;
        }
        check_icicle(&t, &c);
    }

    #[test]
    fn pack_invariants(
        t in tree_strategy(250),
        w in 1.0f32..2000.0,
        h in 1.0f32..2000.0,
        min_px in 0.5f32..6.0,
        dpi in 0.5f32..3.0,
    ) {
        let mut c = PackConfig::new(w, h, dpi);
        c.min_px = min_px;
        check_pack(&t, &c);
    }

    #[test]
    fn views_are_deterministic_and_order_independent(
        t in tree_strategy(250),
        seed in any::<u64>(),
    ) {
        let mut s = t.clone();
        s.shuffle_children(seed);
        let sb = SunburstConfig::new(900.0, 700.0, 1.0);
        prop_assert_eq!(
            layout_sunburst(&t, VecTree::ROOT, &sb).arcs().as_bytes().to_vec(),
            layout_sunburst(&s, VecTree::ROOT, &sb).arcs().as_bytes().to_vec()
        );
        let ic = IcicleConfig::new(900.0, 700.0, 1.0);
        prop_assert_eq!(
            layout_icicle(&t, VecTree::ROOT, &ic).rects().as_bytes().to_vec(),
            layout_icicle(&s, VecTree::ROOT, &ic).rects().as_bytes().to_vec()
        );
        let pc = PackConfig::new(900.0, 700.0, 1.0);
        prop_assert_eq!(
            layout_pack(&t, VecTree::ROOT, &pc).circles().as_bytes().to_vec(),
            layout_pack(&s, VecTree::ROOT, &pc).circles().as_bytes().to_vec()
        );
        let mc = MindMapConfig::new(900.0, 700.0, 1.0);
        prop_assert_eq!(
            layout_mindmap(&t, VecTree::ROOT, &mc).circles().as_bytes().to_vec(),
            layout_mindmap(&s, VecTree::ROOT, &mc).circles().as_bytes().to_vec()
        );
    }

    #[test]
    fn mindmap_accounts_for_everything(
        t in tree_strategy(300),
        keep in 1u16..20,
        depth in 0u16..5,
    ) {
        let mut c = MindMapConfig::new(1000.0, 800.0, 1.0);
        c.max_children = keep;
        c.max_depth = depth;
        let l = layout_mindmap(&t, VecTree::ROOT, &c);
        let recs: Vec<CircleRecord> = l.circles().iter().collect();
        let aggs = aggregates_by_parent(l.aggregates());
        let by_parent = children_of(recs.iter().map(|r| r.parent));
        for (i, r) in recs.iter().enumerate() {
            prop_assert!(r.cx.is_finite() && r.cy.is_finite() && r.r > 0.0);
            prop_assert!(r.depth <= depth);
            if let Some(kids_idx) = by_parent.get(&(i as u32)) {
                let real = kids_idx.iter().filter(|&&k| !recs[k].flags.contains(NodeFlags::AGGREGATE)).count();
                prop_assert!(real <= usize::from(keep));
                let emitted: Vec<_> = kids_idx
                    .iter()
                    .map(|&k| (recs[k].id, recs[k].flags.contains(NodeFlags::AGGREGATE)))
                    .collect();
                check_accounting(&t, r.id, &emitted, aggs.get(&(i as u32)));
            }
        }
        prop_assert_eq!(l.len(), recs.len());
    }
}
