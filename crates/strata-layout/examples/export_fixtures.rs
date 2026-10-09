//! Exports layout buffers as frontend test fixtures.
//!
//! The UI decodes the exact bytes this crate produces, so its tests run
//! against real layouts of `VecTree::synthetic` trees rather than
//! hand-written buffers. Each layout is written as one *layout frame*: the
//! container the backend sends over the `layout://frame` Tauri Channel.
//! [`write_frame`] is the reference encoder for that container; the UI
//! decoder is `ui/src/lib/layout/frame.ts`.
//!
//! Alongside every frame goes a JSON file with the Rust picking results for
//! a grid of sample points plus the `subtree_end` table, so the TypeScript
//! picker can be checked against the Rust one record for record.
//!
//! ```text
//! cargo run -p strata-layout --example export_fixtures --release -- <out-dir>
//! cargo run -p strata-layout --example export_fixtures --release -- <out-dir> --large
//! ```
//!
//! `--large` writes only the 1M-node, LOD-off treemap used by the dev
//! harness for frame-time measurements (~16 MiB, not committed).

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use strata_layout::{
    CircleLayout, CushionParams, Hierarchy, IcicleConfig, IcicleOrientation, LayoutSource,
    MindMapConfig, NodeId, PackConfig, Pick, RectLayout, SunburstConfig, SyntheticSpec,
    TreemapConfig, VecTree, ViewTransform, layout_icicle, layout_mindmap, layout_pack,
    layout_sunburst, layout_treemap, transition_rects,
};

/// `"STLF"` read as a little-endian `u32`.
const FRAME_MAGIC: u32 = u32::from_le_bytes(*b"STLF");
const FRAME_VERSION: u16 = 1;
const FRAME_HEADER: usize = 128;

/// View kinds as encoded in the frame header.
#[derive(Clone, Copy)]
enum View {
    Treemap = 0,
    Icicle = 1,
    Flame = 2,
    Sunburst = 3,
    Bubbles = 4,
    MindMap = 5,
}

/// Header fields that are not buffers.
struct FrameMeta {
    view: View,
    seq: u32,
    root: NodeId,
    root_bytes: u64,
    width: f32,
    height: f32,
    dpr: f32,
    transform: ViewTransform,
    center: (f32, f32),
    ring_width: f32,
}

/// Encodes a layout frame: a 128-byte header, then five 16-byte-aligned
/// sections (nodes, aggregates, labels, cushions, transitions).
fn write_frame(meta: &FrameMeta, sections: [&[u8]; 5]) -> Vec<u8> {
    let mut out = vec![0u8; FRAME_HEADER];
    let mut table = [(0u32, 0u32); 5];
    for (slot, bytes) in table.iter_mut().zip(sections) {
        while !out.len().is_multiple_of(16) {
            out.push(0);
        }
        *slot = (out.len() as u32, bytes.len() as u32);
        out.extend_from_slice(bytes);
    }
    let mut flags = 0u32;
    if !sections[3].is_empty() {
        flags |= 1;
    }
    if !sections[4].is_empty() {
        flags |= 2;
    }
    let h = &mut out[..FRAME_HEADER];
    let put = |h: &mut [u8], off: usize, v: &[u8]| h[off..off + v.len()].copy_from_slice(v);
    put(h, 0, &FRAME_MAGIC.to_le_bytes());
    put(h, 4, &FRAME_VERSION.to_le_bytes());
    put(h, 6, &(meta.view as u16).to_le_bytes());
    put(h, 8, &meta.seq.to_le_bytes());
    put(h, 12, &meta.root.to_le_bytes());
    put(h, 16, &meta.width.to_le_bytes());
    put(h, 20, &meta.height.to_le_bytes());
    put(h, 24, &meta.dpr.to_le_bytes());
    put(h, 28, &flags.to_le_bytes());
    put(h, 32, &meta.transform.scale.to_le_bytes());
    put(h, 40, &meta.transform.tx.to_le_bytes());
    put(h, 48, &meta.transform.ty.to_le_bytes());
    put(h, 56, &meta.center.0.to_le_bytes());
    put(h, 60, &meta.center.1.to_le_bytes());
    put(h, 64, &meta.ring_width.to_le_bytes());
    for (k, (off, len)) in table.iter().enumerate() {
        put(h, 72 + k * 8, &off.to_le_bytes());
        put(h, 76 + k * 8, &len.to_le_bytes());
    }
    put(h, 112, &meta.root_bytes.to_le_bytes());
    out
}

/// Fixture-only color keys in the packed encoding the UI expects (see
/// `ui/src/lib/palette.ts`): the synthetic category plus
/// deterministic pseudo-random safety, age, type and app slots, so every
/// color mode has something to show.
struct PackedKeys<'a>(&'a VecTree);

fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl LayoutSource for PackedKeys<'_> {
    fn size(&self, id: NodeId) -> u64 {
        self.0.size(id)
    }
    fn children(&self, id: NodeId, out: &mut Vec<(NodeId, u64)>) {
        self.0.children(id, out);
    }
    fn is_dir(&self, id: NodeId) -> bool {
        self.0.is_dir(id)
    }
    fn color_key(&self, id: NodeId) -> u32 {
        let h = mix(u64::from(id) + 0x9E37_79B9);
        let category = self.0.color_key(id) & 0xF;
        let safety = (h % 5) as u32;
        let age = ((h >> 8) % 21) as u32;
        let file_type = if self.0.is_dir(id) {
            0
        } else {
            ((h >> 16) % 40) as u32 + 1
        };
        let app = ((h >> 24) % 12) as u32;
        let recent = u32::from((h >> 40).is_multiple_of(50));
        category | safety << 4 | age << 7 | file_type << 12 | app << 20 | recent << 30
    }
}

fn tree(nodes: usize) -> VecTree {
    VecTree::synthetic(&SyntheticSpec {
        nodes,
        mean_fanout: 16,
        dir_ratio: 0.2,
        max_depth: 12,
        seed: 42,
    })
}

/// Per node, 16 bytes: `size:u64@0 parent:u32@8 info:u32@12` where `info`
/// is the packed color key with bit 31 set for directories. Lets tests and
/// the dev harness answer name/size lookups for fixture ids.
fn tree_bytes(t: &VecTree) -> Vec<u8> {
    let src = PackedKeys(t);
    let mut out = Vec::with_capacity(t.len() * 16);
    for id in 0..t.len() as u32 {
        out.extend_from_slice(&t.size(id).to_le_bytes());
        out.extend_from_slice(&t.parent(id).unwrap_or(0).to_le_bytes());
        let info = (src.color_key(id) & 0x7FFF_FFFF) | (u32::from(t.is_dir(id)) << 31);
        out.extend_from_slice(&info.to_le_bytes());
    }
    out
}

fn pick_json(p: Option<Pick>) -> String {
    match p {
        None => "null".into(),
        Some(p) => format!(
            "{{\"index\":{},\"id\":{},\"flags\":{},\"ancestors\":[{}]}}",
            p.index,
            p.id,
            p.flags.bits(),
            p.ancestors
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

/// Sample points: a regular grid plus deterministic jittered points, and a
/// few outside the canvas.
fn sample_points(w: f32, h: f32) -> Vec<(f32, f32)> {
    let mut pts = Vec::new();
    for gy in 0..24 {
        for gx in 0..32 {
            pts.push(((gx as f32 + 0.5) * w / 32.0, (gy as f32 + 0.5) * h / 24.0));
        }
    }
    for k in 0..400u64 {
        let a = mix(k * 2 + 1);
        let b = mix(k * 2 + 2);
        pts.push((
            (a % 1_000_000) as f32 / 1_000_000.0 * w,
            (b % 1_000_000) as f32 / 1_000_000.0 * h,
        ));
    }
    pts.extend([(-1.0, 5.0), (w + 3.0, 5.0), (5.0, -0.5), (w * 0.5, h)]);
    pts
}

fn picks_json(pts: &[(f32, f32)], pick: impl Fn(f32, f32) -> Option<Pick>) -> String {
    pts.iter()
        .map(|&(x, y)| format!("{{\"x\":{x},\"y\":{y},\"pick\":{}}}", pick_json(pick(x, y))))
        .collect::<Vec<_>>()
        .join(",\n")
}

fn subtree_json(len: usize, end: impl Fn(usize) -> Option<usize>) -> String {
    (0..len)
        .map(|i| end(i).map_or("null".into(), |e| e.to_string()))
        .collect::<Vec<_>>()
        .join(",")
}

fn write(dir: &Path, name: &str, bytes: &[u8]) {
    let path = dir.join(name);
    let mut f =
        fs::File::create(&path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    f.write_all(bytes).expect("write fixture");
}

fn rect_frame(l: &RectLayout, meta: &FrameMeta, transitions: &[u8]) -> Vec<u8> {
    write_frame(
        meta,
        [
            l.rects().as_bytes(),
            l.aggregates().as_bytes(),
            l.labels().as_bytes(),
            l.cushions().map_or(&[][..], |c| c.as_bytes()),
            transitions,
        ],
    )
}

fn circle_frame(l: &CircleLayout, meta: &FrameMeta) -> Vec<u8> {
    write_frame(
        meta,
        [
            l.circles().as_bytes(),
            l.aggregates().as_bytes(),
            l.labels().as_bytes(),
            &[],
            &[],
        ],
    )
}

fn meta(view: View, t: &VecTree, root: NodeId, w: f32, h: f32, dpr: f32) -> FrameMeta {
    FrameMeta {
        view,
        seq: 1,
        root,
        root_bytes: t.size(root),
        width: w,
        height: h,
        dpr,
        transform: ViewTransform::IDENTITY,
        center: (0.0, 0.0),
        ring_width: 0.0,
    }
}

fn export_small(dir: &Path) {
    let t = tree(4_000);
    let src = PackedKeys(&t);
    write(dir, "tree.bin", &tree_bytes(&t));
    let (w, h, dpr) = (960.0f32, 640.0f32, 1.25f32);

    // Treemap with cushions.
    let mut cfg = TreemapConfig::new(w, h, dpr);
    cfg.cushion = Some(CushionParams::default());
    let tm = layout_treemap(&src, VecTree::ROOT, &cfg);
    write(
        dir,
        "treemap.frame.bin",
        &rect_frame(&tm, &meta(View::Treemap, &t, VecTree::ROOT, w, h, dpr), &[]),
    );
    let pts = sample_points(w, h);
    write(
        dir,
        "treemap.picks.json",
        format!(
            "{{\"subtreeEnd\":[{}],\n\"picks\":[\n{}\n]}}\n",
            subtree_json(tm.rects().len(), |i| tm.subtree_end(i)),
            picks_json(&pts, |x, y| tm.pick(x, y))
        )
        .as_bytes(),
    );

    // Drill into the largest top-level directory, with transitions.
    let mut kids = Vec::new();
    t.children(VecTree::ROOT, &mut kids);
    let drill = kids
        .iter()
        .filter(|&&(id, _)| t.is_dir(id))
        .max_by_key(|&&(id, s)| (s, std::cmp::Reverse(id)))
        .map(|&(id, _)| id)
        .expect("synthetic root has a directory");
    let dm = layout_treemap(&src, drill, &cfg);
    let tr = transition_rects(&tm, &dm);
    let mut m = meta(View::Treemap, &t, drill, w, h, dpr);
    m.seq = 2;
    write(
        dir,
        "treemap-drill.frame.bin",
        &rect_frame(&dm, &m, tr.as_bytes()),
    );

    // A zoomed layout (visual zoom 3x about a point) to check transforms.
    let view = ViewTransform::zoom_about(3.0, 300.0, 200.0);
    let zm = layout_treemap(
        &src,
        VecTree::ROOT,
        &TreemapConfig::new(w, h, dpr).with_view(view),
    );
    let mut m = meta(View::Treemap, &t, VecTree::ROOT, w, h, dpr);
    m.transform = view;
    write(dir, "treemap-zoom.frame.bin", &rect_frame(&zm, &m, &[]));
    write(
        dir,
        "treemap-zoom.picks.json",
        format!(
            "{{\"subtreeEnd\":[{}],\n\"picks\":[\n{}\n]}}\n",
            subtree_json(zm.rects().len(), |i| zm.subtree_end(i)),
            picks_json(&pts, |x, y| zm.pick(x, y))
        )
        .as_bytes(),
    );

    // One flat directory wide enough to get a per-directory pick grid.
    let mut flat = VecTree::new(0);
    for i in 0..3_000u64 {
        flat.add_file(VecTree::ROOT, 1_000 + (i * 7919) % 50_000, (i % 15) as u32);
    }
    let mut fcfg = TreemapConfig::new(w, h, dpr);
    fcfg.min_px = 0.01;
    fcfg.padding = 0.0;
    let fl = layout_treemap(&PackedKeys(&flat), VecTree::ROOT, &fcfg);
    write(
        dir,
        "treemap-wide.frame.bin",
        &rect_frame(
            &fl,
            &meta(View::Treemap, &flat, VecTree::ROOT, w, h, dpr),
            &[],
        ),
    );
    write(
        dir,
        "treemap-wide.picks.json",
        format!(
            "{{\"subtreeEnd\":[{}],\n\"picks\":[\n{}\n]}}\n",
            subtree_json(fl.rects().len(), |i| fl.subtree_end(i)),
            picks_json(&pts, |x, y| fl.pick(x, y))
        )
        .as_bytes(),
    );

    // Icicle and flame.
    for (name, view, orientation) in [
        ("icicle", View::Icicle, IcicleOrientation::TopDown),
        ("flame", View::Flame, IcicleOrientation::BottomUp),
    ] {
        let mut icfg = IcicleConfig::new(w, h, dpr);
        icfg.orientation = orientation;
        let il = layout_icicle(&src, VecTree::ROOT, &icfg);
        write(
            dir,
            &format!("{name}.frame.bin"),
            &rect_frame(&il, &meta(view, &t, VecTree::ROOT, w, h, dpr), &[]),
        );
        write(
            dir,
            &format!("{name}.picks.json"),
            format!(
                "{{\"subtreeEnd\":[{}],\n\"picks\":[\n{}\n]}}\n",
                subtree_json(il.rects().len(), |i| il.subtree_end(i)),
                picks_json(&pts, |x, y| il.pick(x, y))
            )
            .as_bytes(),
        );
    }

    // Sunburst.
    let sb = layout_sunburst(&src, VecTree::ROOT, &SunburstConfig::new(w, h, dpr));
    let mut m = meta(View::Sunburst, &t, VecTree::ROOT, w, h, dpr);
    m.center = sb.center();
    m.ring_width = sb.ring_width();
    write(
        dir,
        "sunburst.frame.bin",
        &write_frame(
            &m,
            [
                sb.arcs().as_bytes(),
                sb.aggregates().as_bytes(),
                &[],
                &[],
                &[],
            ],
        ),
    );
    write(
        dir,
        "sunburst.picks.json",
        format!(
            "{{\"subtreeEnd\":[{}],\n\"picks\":[\n{}\n]}}\n",
            subtree_json(sb.len(), |i| sb.subtree_end(i)),
            picks_json(&pts, |x, y| sb.pick(x, y))
        )
        .as_bytes(),
    );

    // Bubbles and mind map.
    let pk = layout_pack(&src, VecTree::ROOT, &PackConfig::new(w, h, dpr));
    write(
        dir,
        "bubbles.frame.bin",
        &circle_frame(&pk, &meta(View::Bubbles, &t, VecTree::ROOT, w, h, dpr)),
    );
    write(
        dir,
        "bubbles.picks.json",
        format!(
            "{{\"subtreeEnd\":[{}],\n\"picks\":[\n{}\n]}}\n",
            subtree_json(pk.len(), |i| pk.subtree_end(i)),
            picks_json(&pts, |x, y| pk.pick(x, y))
        )
        .as_bytes(),
    );
    let mm = layout_mindmap(&src, VecTree::ROOT, &MindMapConfig::new(w, h, dpr));
    write(
        dir,
        "mindmap.frame.bin",
        &circle_frame(&mm, &meta(View::MindMap, &t, VecTree::ROOT, w, h, dpr)),
    );
    write(
        dir,
        "mindmap.picks.json",
        format!(
            "{{\"subtreeEnd\":[{}],\n\"picks\":[\n{}\n]}}\n",
            subtree_json(mm.len(), |i| mm.subtree_end(i)),
            picks_json(&pts, |x, y| mm.pick(x, y))
        )
        .as_bytes(),
    );
    println!(
        "small fixtures: treemap {} rects, drill {} rects / {} transitions, wide {} rects, sunburst {} arcs, bubbles {}, mindmap {}",
        tm.rects().len(),
        dm.rects().len(),
        tr.len(),
        fl.rects().len(),
        sb.len(),
        pk.len(),
        mm.len()
    );
}

fn export_large(dir: &Path) {
    let t = tree(1_000_000);
    let src = PackedKeys(&t);
    let (w, h, dpr) = (2560.0f32, 1440.0f32, 1.5f32);
    let mut cfg = TreemapConfig::new(w, h, dpr);
    // Sub-pixel threshold and no padding: the stress case for instancing.
    cfg.min_px = 0.01;
    cfg.padding = 0.0;
    cfg.cushion = Some(CushionParams::default());
    let l = layout_treemap(&src, VecTree::ROOT, &cfg);
    write(
        dir,
        "treemap-1m.frame.bin",
        &rect_frame(&l, &meta(View::Treemap, &t, VecTree::ROOT, w, h, dpr), &[]),
    );
    write(dir, "tree-1m.bin", &tree_bytes(&t));
    println!(
        "large fixture: {} nodes, {} rects",
        t.len(),
        l.rects().len()
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(
        args.next()
            .expect("usage: export_fixtures <out-dir> [--large]"),
    );
    let large = args.next().is_some_and(|a| a == "--large");
    fs::create_dir_all(&dir).expect("create output dir");
    if large {
        export_large(&dir);
    } else {
        export_small(&dir);
    }
}
