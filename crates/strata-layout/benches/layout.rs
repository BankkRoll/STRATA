//! Layout and picking benchmarks (SPEC §2: drill-down relayout of a 100k
//! subtree ≤ 50 ms; picking < 1 ms; 60 fps with 1M+ entries).

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use strata_layout::{
    HitGrid, IcicleConfig, LayoutSource, MindMapConfig, NodeId, PackConfig, SunburstConfig,
    SyntheticSpec, TreemapConfig, VecTree, ViewTransform, layout_icicle, layout_mindmap,
    layout_pack, layout_sunburst, layout_treemap, layout_treemap_into, transition_rects,
};

fn tree(nodes: usize) -> VecTree {
    VecTree::synthetic(&SyntheticSpec {
        nodes,
        mean_fanout: 16,
        dir_ratio: 0.2,
        max_depth: 12,
        seed: 42,
    })
}

fn depth_of(t: &VecTree) -> u32 {
    let mut stack = vec![(VecTree::ROOT, 0u32)];
    let mut kids = Vec::new();
    let mut max = 0;
    while let Some((id, d)) = stack.pop() {
        max = max.max(d);
        kids.clear();
        t.children(id, &mut kids);
        stack.extend(kids.iter().map(|&(c, _)| (c, d + 1)));
    }
    max
}

fn flat(n: u64) -> VecTree {
    let mut t = VecTree::new(0);
    for i in 0..n {
        t.add_file(VecTree::ROOT, 1 + i % 977, 0);
    }
    t
}

/// The QHD @ 150% viewport from the brief.
fn qhd() -> TreemapConfig {
    TreemapConfig::new(2560.0, 1440.0, 1.5)
}

fn treemap(c: &mut Criterion) {
    let t100k = tree(100_000);
    let t1m = tree(1_000_000);
    eprintln!("1m tree depth: {}", depth_of(&t1m));
    let cfg = qhd();
    let mut g = c.benchmark_group("treemap");
    g.sample_size(20);

    let mut out = layout_treemap(&t100k, VecTree::ROOT, &cfg);
    eprintln!("100k qhd: {} rects", out.rects().len());
    g.bench_function("100k_qhd_lod", |b| {
        b.iter(|| layout_treemap_into(&t100k, VecTree::ROOT, black_box(&cfg), &mut out));
    });

    // Sub-pixel threshold and no padding: nearly every entry is emitted, the
    // worst case for a 100k drill-down.
    let mut no_lod = cfg.clone();
    no_lod.min_px = 0.01;
    no_lod.padding = 0.0;
    layout_treemap_into(&t100k, VecTree::ROOT, &no_lod, &mut out);
    eprintln!("100k no-lod: {} rects", out.rects().len());
    g.bench_function("100k_no_lod", |b| {
        b.iter(|| layout_treemap_into(&t100k, VecTree::ROOT, black_box(&no_lod), &mut out));
    });

    let mut out1m = layout_treemap(&t1m, VecTree::ROOT, &cfg);
    eprintln!("1m qhd: {} rects", out1m.rects().len());
    g.bench_function("1m_qhd_lod", |b| {
        b.iter(|| layout_treemap_into(&t1m, VecTree::ROOT, black_box(&cfg), &mut out1m));
    });

    let mut cushion = cfg.clone();
    cushion.cushion = Some(strata_layout::CushionParams::default());
    g.bench_function("1m_qhd_lod_cushion", |b| {
        b.iter(|| layout_treemap_into(&t1m, VecTree::ROOT, black_box(&cushion), &mut out1m));
    });

    let zoomed = cfg
        .clone()
        .with_view(ViewTransform::zoom_about(40.0, 1300.0, 700.0));
    layout_treemap_into(&t1m, VecTree::ROOT, &zoomed, &mut out1m);
    eprintln!("1m zoomed 40x: {} rects", out1m.rects().len());
    g.bench_function("1m_qhd_zoom40", |b| {
        b.iter(|| layout_treemap_into(&t1m, VecTree::ROOT, black_box(&zoomed), &mut out1m));
    });

    let f = flat(1_000_000);
    let mut flat_cfg = cfg.clone();
    flat_cfg.min_px = 0.01;
    flat_cfg.padding = 0.0;
    g.bench_function("flat_1m_no_lod", |b| {
        b.iter(|| layout_treemap_into(&f, VecTree::ROOT, black_box(&flat_cfg), &mut out1m));
    });
    g.finish();
}

fn picking(c: &mut Criterion) {
    let t = tree(1_000_000);
    let mut cfg = qhd();
    cfg.min_px = 0.01;
    cfg.padding = 0.0;
    let layout = layout_treemap(&t, VecTree::ROOT, &cfg);
    eprintln!("picking layout: {} rects", layout.rects().len());
    let grid = HitGrid::build(&layout, 16.0);
    let points: Vec<(f32, f32)> = (0..1024)
        .map(|i| {
            let f = i as f32;
            ((f * 97.13) % 2560.0, (f * 53.71) % 1440.0)
        })
        .collect();
    let mut g = c.benchmark_group("pick");
    g.bench_function("descent_1024_points", |b| {
        b.iter(|| {
            for &(x, y) in &points {
                black_box(layout.hit_test(x, y));
            }
        });
    });
    g.bench_function("pick_with_chain_1024_points", |b| {
        b.iter(|| {
            for &(x, y) in &points {
                black_box(layout.pick(x, y));
            }
        });
    });
    g.bench_function("grid_1024_points", |b| {
        b.iter(|| {
            for &(x, y) in &points {
                black_box(grid.hit_test(&layout, x, y));
            }
        });
    });
    g.sample_size(10);
    g.bench_function("grid_build", |b| {
        b.iter(|| black_box(HitGrid::build(&layout, 16.0)));
    });

    let f = flat(1_000_000);
    let flat_layout = layout_treemap(&f, VecTree::ROOT, &cfg);
    eprintln!("flat layout: {} rects", flat_layout.rects().len());
    let flat_grid = HitGrid::build(&flat_layout, 16.0);
    g.sample_size(30);
    g.bench_function("descent_flat_1024_points", |b| {
        b.iter(|| {
            for &(x, y) in &points {
                black_box(flat_layout.hit_test(x, y));
            }
        });
    });
    g.bench_function("grid_flat_1024_points", |b| {
        b.iter(|| {
            for &(x, y) in &points {
                black_box(flat_grid.hit_test(&flat_layout, x, y));
            }
        });
    });
    g.finish();
}

fn views(c: &mut Criterion) {
    let t = tree(1_000_000);
    let t100k = tree(100_000);
    let mut g = c.benchmark_group("views");
    g.sample_size(20);
    let sb = SunburstConfig::new(2560.0, 1440.0, 1.5);
    eprintln!(
        "sunburst 1m: {} arcs",
        layout_sunburst(&t, VecTree::ROOT, &sb).arcs().len()
    );
    g.bench_function("sunburst_1m", |b| {
        b.iter(|| layout_sunburst(&t, VecTree::ROOT, black_box(&sb)));
    });
    let ic = IcicleConfig::new(2560.0, 1440.0, 1.5);
    g.bench_function("icicle_1m", |b| {
        b.iter(|| layout_icicle(&t, VecTree::ROOT, black_box(&ic)));
    });
    let pc = PackConfig::new(2560.0, 1440.0, 1.5);
    eprintln!(
        "pack 1m: {} circles",
        layout_pack(&t, VecTree::ROOT, &pc).circles().len()
    );
    g.bench_function("pack_1m", |b| {
        b.iter(|| layout_pack(&t, VecTree::ROOT, black_box(&pc)));
    });
    g.bench_function("pack_100k", |b| {
        b.iter(|| layout_pack(&t100k, VecTree::ROOT, black_box(&pc)));
    });
    let mc = MindMapConfig::new(2560.0, 1440.0, 1.5);
    g.bench_function("mindmap_1m", |b| {
        b.iter(|| layout_mindmap(&t, VecTree::ROOT, black_box(&mc)));
    });

    let cfg = qhd();
    let before = layout_treemap(&t, VecTree::ROOT, &cfg);
    let target: NodeId = before
        .rects()
        .iter()
        .find(|r| r.depth == 1 && r.w > 200.0)
        .map_or(VecTree::ROOT, |r| r.id);
    let after = layout_treemap(&t, target, &cfg);
    eprintln!(
        "transition: {} -> {} rects",
        before.rects().len(),
        after.rects().len()
    );
    g.bench_function("transition_drill_down", |b| {
        b.iter(|| transition_rects(black_box(&before), black_box(&after)));
    });
    g.finish();
}

criterion_group!(benches, treemap, picking, views);
criterion_main!(benches);
