//! Every view on extreme inputs: sizes at the top of the `u64` range, a
//! single huge file beside 100,000 tiny siblings, a 2,000-level chain and
//! degenerate viewports. Nothing may panic and every coordinate must be
//! finite.

use strata_layout::{
    IcicleConfig, MindMapConfig, PackConfig, SunburstConfig, TreemapConfig, VecTree, layout_icicle,
    layout_mindmap, layout_pack, layout_sunburst, layout_treemap,
};

fn all_views_finite(t: &VecTree, w: f32, h: f32) {
    let l = layout_treemap(t, VecTree::ROOT, &TreemapConfig::new(w, h, 1.0));
    for r in l.rects().iter() {
        assert!(r.x.is_finite() && r.y.is_finite() && r.w.is_finite() && r.h.is_finite());
        assert!(r.w >= 0.0 && r.h >= 0.0);
    }
    let l = layout_icicle(t, VecTree::ROOT, &IcicleConfig::new(w, h, 1.0));
    for r in l.rects().iter() {
        assert!(r.x.is_finite() && r.y.is_finite() && r.w.is_finite() && r.h.is_finite());
    }
    let l = layout_sunburst(t, VecTree::ROOT, &SunburstConfig::new(w, h, 1.0));
    for r in l.arcs().iter() {
        assert!(r.a0.is_finite() && r.a1.is_finite() && r.r0.is_finite() && r.r1.is_finite());
    }
    let l = layout_pack(t, VecTree::ROOT, &PackConfig::new(w, h, 1.0));
    for c in l.circles().iter() {
        assert!(c.cx.is_finite() && c.cy.is_finite() && c.r.is_finite());
    }
    let l = layout_mindmap(t, VecTree::ROOT, &MindMapConfig::new(w, h, 1.0));
    for c in l.circles().iter() {
        assert!(c.cx.is_finite() && c.cy.is_finite() && c.r.is_finite());
    }
}

#[test]
fn sizes_at_the_top_of_the_range() {
    let mut t = VecTree::new(0);
    let a = t.add_dir(VecTree::ROOT, 1);
    let b = t.add_dir(VecTree::ROOT, 2);
    t.add_file(a, u64::MAX, 3);
    t.add_file(a, u64::MAX, 4);
    t.add_file(b, u64::MAX / 2, 5);
    t.add_file(b, 1, 6);
    t.add_file(VecTree::ROOT, 0, 7);
    for (w, h) in [(1920.0, 1080.0), (1.0, 1.0), (10_000.0, 3.0)] {
        all_views_finite(&t, w, h);
    }
}

#[test]
fn one_giant_beside_many_tiny_files() {
    let mut t = VecTree::new(0);
    t.add_file(VecTree::ROOT, 1 << 50, 1);
    let d = t.add_dir(VecTree::ROOT, 2);
    for i in 0..100_000u64 {
        t.add_file(d, 1 + i % 3, 3);
    }
    all_views_finite(&t, 2560.0, 1440.0);
}

#[test]
fn a_two_thousand_level_chain() {
    let mut t = VecTree::new(0);
    let mut p = VecTree::ROOT;
    for i in 0..2000u32 {
        p = t.add_dir(p, i);
    }
    t.add_file(p, 12345, 0);
    all_views_finite(&t, 1920.0, 1080.0);
}

#[test]
fn degenerate_viewports_and_empty_trees() {
    let empty = VecTree::new(0);
    let mut one = VecTree::new(0);
    one.add_file(VecTree::ROOT, 7, 0);
    for t in [&empty, &one] {
        for (w, h) in [
            (0.0, 0.0),
            (0.0, 100.0),
            (1e-6, 1e-6),
            (f32::MAX, f32::MAX),
            (-5.0, 10.0),
            (f32::NAN, 10.0),
            (f32::INFINITY, 10.0),
        ] {
            let _ = layout_treemap(t, VecTree::ROOT, &TreemapConfig::new(w, h, 1.0));
            let _ = layout_icicle(t, VecTree::ROOT, &IcicleConfig::new(w, h, 1.0));
            let _ = layout_sunburst(t, VecTree::ROOT, &SunburstConfig::new(w, h, 1.0));
            let _ = layout_pack(t, VecTree::ROOT, &PackConfig::new(w, h, 1.0));
            let _ = layout_mindmap(t, VecTree::ROOT, &MindMapConfig::new(w, h, 1.0));
        }
    }
}
