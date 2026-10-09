//! Nested squarified treemap with headers, padding, LOD aggregation,
//! viewport culling and optional cushion coefficients.
//!
//! # Traversal
//!
//! The tree is walked in pre-order with an explicit stack (real directory
//! chains can be thousands deep; recursion would overflow the 1 MiB Windows
//! main-thread stack). For each directory that is laid out:
//!
//! 1. Fetch children, drop zero-size ones (they are counted in the
//!    aggregate), sort by size descending with the id as tie-break. Ids are
//!    unique, so the order — and therefore the output — is bit-identical for
//!    the same input regardless of child order.
//! 2. **LOD cut.** Children whose proportional area is below `min_px²` are
//!    folded into one aggregate weight. The sorted order makes this a binary
//!    search.
//! 3. Squarify the kept children plus the aggregate into the directory's
//!    content box (rect minus padding minus header strip).
//! 4. If any kept child came out thinner than `min_px` on a side (a long
//!    thin final row), move the cut to that child and repeat. Each round
//!    strictly shrinks the kept set; after a few rounds the cut also shrinks
//!    geometrically, which bounds the worst case at O(n log n).
//! 5. Push the resulting items as a stack frame; each is emitted (and
//!    possibly descended into) before its next sibling, giving pre-order.
//!
//! Nodes whose rect misses the visible region are skipped with their whole
//! subtree, so zoomed-in layouts cost only what is on screen.
//!
//! Total cost is O(V log V) in the number of *visited* children V, which LOD
//! bounds by the pixel count rather than the tree size.

use crate::buffer::{AggregateRecord, LabelRecord, NO_INDEX, NodeFlags, RectRecord};
use crate::cushion::{CushionParams, Surface};
use crate::geom::{R64, Rect, ViewTransform, sane_len};
use crate::rects::{RectLayout, RectLayoutKind};
use crate::source::{LayoutSource, NodeId};
use crate::squarify::squarify;

/// Treemap settings. All lengths are **device pixels**; use
/// [`TreemapConfig::new`] to derive DPI-aware defaults.
///
/// # Example
///
/// ```
/// use strata_layout::{TreemapConfig, ViewTransform};
///
/// // 1707x960 CSS px window at 150% scaling, zoomed 3x around the cursor.
/// let cfg = TreemapConfig::new(2560.0, 1440.0, 1.5)
///     .with_view(ViewTransform::zoom_about(3.0, 1200.0, 700.0));
/// assert_eq!(cfg.padding, 3.0);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct TreemapConfig {
    /// Canvas area the root fills at zoom 1. Also the visible region:
    /// anything outside it is culled and geometry is clipped to it.
    pub viewport: Rect,
    /// Visual zoom applied before layout (see [`ViewTransform`]).
    pub view: ViewTransform,
    /// Gap between a directory's edge and its children.
    pub padding: f32,
    /// Height of the directory header strip (name + size).
    pub header_height: f32,
    /// A directory gets a header only when at least this wide...
    pub header_min_width: f32,
    /// ...and at least this tall.
    pub header_min_height: f32,
    /// LOD threshold: no emitted rect is narrower or shorter than this, and
    /// children whose share of the area is below `min_px²` are aggregated.
    pub min_px: f32,
    /// Deepest level laid out (root = 0). Deeper directories are emitted as
    /// [`NodeFlags::TRUNCATED`] blocks.
    pub max_depth: u16,
    /// Minimum label box width for a label candidate.
    pub label_min_width: f32,
    /// Minimum label box height for a label candidate.
    pub label_min_height: f32,
    /// Emit cushion coefficients (parallel buffer) when set.
    pub cushion: Option<CushionParams>,
}

impl TreemapConfig {
    /// Defaults for a `width`×`height` device-pixel canvas at `dpi_scale`
    /// (1.0 = 96 DPI). Spacing scales with DPI; `min_px` stays at one device
    /// pixel. Non-finite or non-positive scales are treated as 1.0.
    #[must_use]
    pub fn new(width: f32, height: f32, dpi_scale: f32) -> Self {
        let s = if dpi_scale.is_finite() && dpi_scale > 0.0 {
            dpi_scale
        } else {
            1.0
        };
        Self {
            viewport: Rect::new(0.0, 0.0, width, height),
            view: ViewTransform::IDENTITY,
            padding: 2.0 * s,
            header_height: 16.0 * s,
            header_min_width: 64.0 * s,
            header_min_height: 40.0 * s,
            min_px: 1.0,
            max_depth: 64,
            label_min_width: 40.0 * s,
            label_min_height: 13.0 * s,
            cushion: None,
        }
    }

    /// Returns the config with a visual-zoom transform.
    #[must_use]
    pub fn with_view(mut self, view: ViewTransform) -> Self {
        self.view = view;
        self
    }
}

/// Lays out the subtree under `root` as a nested squarified treemap.
///
/// # Example
///
/// ```
/// use strata_layout::{layout_treemap, NodeFlags, TreemapConfig, VecTree};
///
/// let mut tree = VecTree::new(0);
/// let src = tree.add_dir(VecTree::ROOT, 1);
/// tree.add_file(src, 3_000, 2);
/// tree.add_file(src, 1_000, 2);
/// tree.add_file(VecTree::ROOT, 4_000, 3);
///
/// let layout = layout_treemap(&tree, VecTree::ROOT, &TreemapConfig::new(1000.0, 500.0, 1.0));
/// let root = layout.rects().get(0).unwrap();
/// assert_eq!(root.id, VecTree::ROOT);
/// assert!(root.flags.contains(NodeFlags::DIR | NodeFlags::HAS_HEADER));
/// assert_eq!(layout.rects().len(), 5);
/// ```
#[must_use]
pub fn layout_treemap<S: LayoutSource + ?Sized>(
    src: &S,
    root: NodeId,
    cfg: &TreemapConfig,
) -> RectLayout {
    let mut out = RectLayout::new();
    layout_treemap_into(src, root, cfg, &mut out);
    out
}

/// Like [`layout_treemap`] but reuses `out`'s allocations, which matters
/// when relayouting every frame during zoom or live updates.
pub fn layout_treemap_into<S: LayoutSource + ?Sized>(
    src: &S,
    root: NodeId,
    cfg: &TreemapConfig,
    out: &mut RectLayout,
) {
    out.reset(RectLayoutKind::Treemap, cfg.cushion.is_some());
    let clip = R64::from_rect(cfg.viewport);
    let root_rect = cfg.view.sanitized().apply_rect(cfg.viewport);
    let p = Params::new(cfg);
    if !root_rect.is_finite() || root_rect.w() < p.min_px || root_rect.h() < p.min_px {
        return;
    }
    let mut eng = Engine {
        src,
        p,
        clip,
        out,
        kids: Vec::new(),
        weights: Vec::new(),
        boxes: Vec::new(),
        arena: Vec::new(),
    };
    let root_item = Item {
        id: root,
        size: src.size(root),
        rect: root_rect,
        agg: NO_INDEX,
    };
    let mut frames: Vec<Frame> = Vec::new();
    if let Some(f) = eng.emit(&root_item, NO_INDEX, 0, &Surface::default(), 0) {
        frames.push(f);
    }
    while let Some(top) = frames.last_mut() {
        if top.next < top.end {
            let item = eng.arena[top.next];
            top.next += 1;
            let (rec, depth, surface, color) = (top.rec, top.depth + 1, top.surface, top.color);
            if let Some(f) = eng.emit(&item, rec, depth, &surface, color) {
                frames.push(f);
            }
        } else {
            let (rec, start) = (top.rec, top.start);
            frames.pop();
            eng.out.subtree_end[rec as usize] = eng.out.rects.len() as u32;
            eng.arena.truncate(start);
        }
    }
    out.index_wide_dirs();
}

/// Config converted to sanitized `f64`.
#[derive(Debug, Clone, Copy)]
struct Params {
    pad: f64,
    header_h: f64,
    header_min_w: f64,
    header_min_h: f64,
    min_px: f64,
    max_depth: u16,
    label_min_w: f64,
    label_min_h: f64,
    cushion: Option<CushionParams>,
}

impl Params {
    fn new(cfg: &TreemapConfig) -> Self {
        Self {
            pad: sane_len(cfg.padding),
            header_h: sane_len(cfg.header_height),
            header_min_w: sane_len(cfg.header_min_width),
            header_min_h: sane_len(cfg.header_min_height),
            // A zero threshold would let squarify emit sub-pixel slivers
            // without bound; clamp to a tiny but positive floor.
            min_px: sane_len(cfg.min_px).max(1e-3),
            max_depth: cfg.max_depth,
            label_min_w: sane_len(cfg.label_min_width),
            label_min_h: sane_len(cfg.label_min_height),
            cushion: cfg.cushion,
        }
    }
}

/// A child waiting to be emitted.
#[derive(Debug, Clone, Copy)]
struct Item {
    id: NodeId,
    /// Entry bytes, or the aggregate's bytes.
    size: u64,
    rect: R64,
    /// Aggregate side-table index, or `NO_INDEX` for a real entry.
    agg: u32,
}

/// A directory whose children are being emitted.
#[derive(Debug, Clone, Copy)]
struct Frame {
    rec: u32,
    depth: u16,
    color: u32,
    surface: Surface,
    start: usize,
    next: usize,
    end: usize,
}

struct Engine<'a, S: ?Sized> {
    src: &'a S,
    p: Params,
    clip: R64,
    out: &'a mut RectLayout,
    kids: Vec<(NodeId, u64)>,
    weights: Vec<f64>,
    boxes: Vec<R64>,
    arena: Vec<Item>,
}

impl<S: LayoutSource + ?Sized> Engine<'_, S> {
    /// Emits one record; returns a frame when its children should follow.
    fn emit(
        &mut self,
        item: &Item,
        parent: u32,
        depth: u16,
        parent_surface: &Surface,
        parent_color: u32,
    ) -> Option<Frame> {
        let p = self.p;
        let visible = item.rect.intersect(&self.clip)?;
        let is_agg = item.agg != NO_INDEX;
        let is_dir = !is_agg && self.src.is_dir(item.id);
        let r = item.rect;
        let has_header = is_dir
            && p.header_h > 0.0
            && r.w() >= p.header_min_w
            && r.h() >= p.header_min_h
            && r.h() >= p.header_h + 2.0 * p.pad;
        let top = p.pad + if has_header { p.header_h } else { 0.0 };
        let content = r.inset(p.pad, top, p.pad, p.pad);
        let descend = is_dir
            && depth < p.max_depth
            && content.w() >= p.min_px
            && content.h() >= p.min_px
            && content.intersect(&self.clip).is_some();

        let mut flags = NodeFlags::EMPTY;
        if is_dir {
            flags |= NodeFlags::DIR;
        }
        if has_header {
            flags |= NodeFlags::HAS_HEADER;
        }
        flags |= if is_agg {
            NodeFlags::AGGREGATE
        } else {
            NodeFlags::SELECTABLE
        };
        if is_dir && !descend {
            flags |= NodeFlags::TRUNCATED;
        }
        if visible != r {
            flags |= NodeFlags::CLIPPED;
        }
        let color = if is_agg {
            parent_color
        } else {
            self.src.color_key(item.id)
        };
        let v = visible.to_rect();
        let index = self.out.rects.push(RectRecord {
            x: v.x,
            y: v.y,
            w: v.w,
            h: v.h,
            id: item.id,
            color_key: color,
            parent,
            depth,
            flags,
        });
        self.out.subtree_end.push(index + 1);

        let surface = match (p.cushion, self.out.cushions.as_mut()) {
            (Some(c), Some(buf)) => {
                let s = parent_surface.with_ridges(&r, c.ridge_height(depth));
                buf.push(s.record());
                s
            }
            _ => Surface::default(),
        };

        if is_agg && let Some(mut a) = self.out.aggregates.get(item.agg as usize) {
            a.record = index;
            self.out.aggregates.set(item.agg as usize, a);
        }

        let label_box = if has_header {
            Some(R64 {
                x0: r.x0 + p.pad,
                y0: r.y0 + p.pad,
                x1: r.x1 - p.pad,
                y1: r.y0 + p.pad + p.header_h,
            })
        } else if !descend {
            Some(r.inset(p.pad, p.pad, p.pad, p.pad))
        } else {
            None
        };
        if let Some(b) = label_box.and_then(|b| b.intersect(&self.clip))
            && b.w() >= p.label_min_w
            && b.h() >= p.label_min_h
        {
            let b = b.to_rect();
            self.out.labels.push(LabelRecord {
                record: index,
                id: item.id,
                x: b.x,
                y: b.y,
                w: b.w,
                h: b.h,
                size: item.size,
            });
        }

        if !descend {
            return None;
        }
        let start = self.arena.len();
        self.layout_children(item.id, index, content);
        let end = self.arena.len();
        (end > start).then_some(Frame {
            rec: index,
            depth,
            color,
            surface,
            start,
            next: start,
            end,
        })
    }

    /// Squarifies `dir`'s children into `content` and appends them to the
    /// arena (see the module docs for the LOD loop).
    fn layout_children(&mut self, dir: NodeId, dir_rec: u32, content: R64) {
        let min_px = self.p.min_px;
        self.kids.clear();
        self.src.children(dir, &mut self.kids);
        let all = self.kids.len();
        self.kids.retain(|&(_, s)| s > 0);
        let zero = all - self.kids.len();
        self.kids
            .sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let total: u128 = self.kids.iter().map(|&(_, s)| u128::from(s)).sum();

        let mut cut = 0;
        let mut tail: u128 = total;
        if total > 0 {
            let k = content.area() / total as f64;
            let min_area = min_px * min_px;
            cut = self
                .kids
                .partition_point(|&(_, s)| s as f64 * k >= min_area);
            let mut rounds = 0;
            loop {
                tail = self.kids[cut..].iter().map(|&(_, s)| u128::from(s)).sum();
                self.weights.clear();
                self.weights
                    .extend(self.kids[..cut].iter().map(|&(_, s)| s as f64));
                if tail > 0 {
                    self.weights.push(tail as f64);
                }
                self.boxes.clear();
                squarify(&self.weights, content, &mut self.boxes);
                let thin = self.boxes[..cut]
                    .iter()
                    .position(|b| b.w() < min_px || b.h() < min_px);
                match thin {
                    None => break,
                    Some(bad) => {
                        rounds += 1;
                        cut = if rounds < 4 {
                            bad
                        } else {
                            bad.min(cut * 3 / 4)
                        };
                    }
                }
            }
        }

        let excluded = all - cut;
        let agg = if excluded > 0 {
            self.out.aggregates.push(AggregateRecord {
                record: NO_INDEX,
                parent: dir_rec,
                dir_id: dir,
                count: u32::try_from(excluded).unwrap_or(u32::MAX),
                bytes: u64::try_from(tail).unwrap_or(u64::MAX),
            })
        } else {
            NO_INDEX
        };
        debug_assert!(zero <= excluded);
        for (i, &(id, size)) in self.kids[..cut].iter().enumerate() {
            self.arena.push(Item {
                id,
                size,
                rect: self.boxes[i],
                agg: NO_INDEX,
            });
        }
        if tail > 0 && cut < self.boxes.len() {
            let b = self.boxes[cut];
            if b.w() >= min_px && b.h() >= min_px {
                self.arena.push(Item {
                    id: dir,
                    size: u64::try_from(tail).unwrap_or(u64::MAX),
                    rect: b,
                    agg,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::NodeFlags;
    use crate::hierarchy::Hierarchy;
    use crate::source::VecTree;

    fn cfg(w: f32, h: f32) -> TreemapConfig {
        TreemapConfig::new(w, h, 1.0)
    }

    fn assert_finite(l: &RectLayout) {
        for r in l.rects().iter() {
            assert!(r.rect().is_valid(), "{r:?}");
        }
    }

    #[test]
    fn single_child_fills_content_box() {
        let mut t = VecTree::new(0);
        let f = t.add_file(VecTree::ROOT, 10, 0);
        let c = cfg(300.0, 200.0);
        let l = layout_treemap(&t, VecTree::ROOT, &c);
        assert_eq!(l.len(), 2);
        let r = l.rects().get(1).unwrap();
        assert_eq!(r.id, f);
        assert_eq!(r.x, c.padding);
        assert_eq!(r.y, c.padding + c.header_height);
        assert_eq!(r.x + r.w, 300.0 - c.padding);
        assert_eq!(r.y + r.h, 200.0 - c.padding);
    }

    #[test]
    fn all_zero_sizes_emit_only_the_root_and_count_them() {
        let mut t = VecTree::new(0);
        for _ in 0..5 {
            t.add_file(VecTree::ROOT, 0, 0);
        }
        let l = layout_treemap(&t, VecTree::ROOT, &cfg(300.0, 200.0));
        assert_eq!(l.len(), 1);
        let a = l.aggregates().get(0).unwrap();
        assert_eq!((a.count, a.bytes, a.record), (5, 0, NO_INDEX));
    }

    #[test]
    fn one_huge_and_many_tiny_aggregates_the_tiny() {
        let mut t = VecTree::new(0);
        let big = t.add_file(VecTree::ROOT, 1 << 40, 0);
        for _ in 0..10_000 {
            t.add_file(VecTree::ROOT, 1, 0);
        }
        let l = layout_treemap(&t, VecTree::ROOT, &cfg(800.0, 600.0));
        assert_finite(&l);
        assert_eq!(l.rects().get(1).unwrap().id, big);
        let a = l.aggregates().get(0).unwrap();
        assert_eq!((a.count, a.bytes), (10_000, 10_000));
        // 10 kB out of 1 TB is far below a pixel, so not even the aggregate draws.
        assert_eq!(a.record, NO_INDEX);
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn many_equal_tiny_files_become_one_hatched_block() {
        let mut t = VecTree::new(0);
        for _ in 0..1_000_000 {
            t.add_file(VecTree::ROOT, 1, 0);
        }
        let l = layout_treemap(&t, VecTree::ROOT, &cfg(100.0, 100.0));
        assert_finite(&l);
        let a = l.aggregates().get(0).unwrap();
        assert_eq!(a.count + (l.len() as u32 - 2), 1_000_000);
        let rec = l.rects().get(a.record as usize).unwrap();
        assert!(rec.flags.contains(NodeFlags::AGGREGATE));
        assert!(!rec.flags.contains(NodeFlags::SELECTABLE));
        assert_eq!(rec.id, VecTree::ROOT);
    }

    #[test]
    fn zero_and_degenerate_viewports_are_empty() {
        let t = VecTree::synthetic(&crate::SyntheticSpec {
            nodes: 200,
            ..Default::default()
        });
        for (w, h) in [(0.0, 0.0), (0.5, 100.0), (f32::NAN, 10.0), (-5.0, 10.0)] {
            let l = layout_treemap(&t, VecTree::ROOT, &cfg(w, h));
            assert!(l.is_empty(), "{w}x{h}");
            assert_eq!(l.hit_test(0.0, 0.0), None);
        }
    }

    #[test]
    fn extreme_aspect_viewports_stay_finite_and_respect_min_px() {
        let t = VecTree::synthetic(&crate::SyntheticSpec {
            nodes: 3_000,
            ..Default::default()
        });
        for (w, h) in [(100_000.0, 3.0), (3.0, 100_000.0), (1.0, 1.0)] {
            let l = layout_treemap(&t, VecTree::ROOT, &cfg(w, h));
            assert_finite(&l);
            for r in l.rects().iter().skip(1) {
                assert!(r.w >= 0.999 && r.h >= 0.999, "{r:?}");
            }
        }
    }

    #[test]
    fn nan_config_values_are_neutralized() {
        let t = VecTree::synthetic(&crate::SyntheticSpec {
            nodes: 500,
            ..Default::default()
        });
        let mut c = cfg(640.0, 480.0);
        c.padding = f32::NAN;
        c.header_height = f32::INFINITY;
        c.min_px = f32::NAN;
        c.view.scale = f64::NAN;
        c.cushion = Some(CushionParams {
            height: f32::NAN,
            falloff: f32::INFINITY,
        });
        let l = layout_treemap(&t, VecTree::ROOT, &c);
        assert!(!l.is_empty());
        assert_finite(&l);
        for k in l.cushions().unwrap().iter() {
            assert!(k.kx1.is_finite() && k.kx2.is_finite() && k.ky1.is_finite());
        }
    }

    #[test]
    fn depth_limit_truncates() {
        let mut t = VecTree::new(0);
        let a = t.add_dir(VecTree::ROOT, 0);
        let b = t.add_dir(a, 0);
        t.add_file(b, 5, 0);
        let mut c = cfg(800.0, 600.0);
        c.max_depth = 1;
        let l = layout_treemap(&t, VecTree::ROOT, &c);
        assert_eq!(l.len(), 2);
        let ra = l.rects().get(1).unwrap();
        assert_eq!(ra.id, a);
        assert!(ra.flags.contains(NodeFlags::DIR | NodeFlags::TRUNCATED));
    }

    #[test]
    fn zoom_culls_offscreen_and_reveals_depth() {
        let t = VecTree::synthetic(&crate::SyntheticSpec {
            nodes: 50_000,
            ..Default::default()
        });
        let base = cfg(800.0, 600.0);
        let full = layout_treemap(&t, VecTree::ROOT, &base);
        let zoomed = layout_treemap(
            &t,
            VecTree::ROOT,
            &base
                .clone()
                .with_view(ViewTransform::zoom_about(50.0, 400.0, 300.0)),
        );
        assert_finite(&zoomed);
        let max_depth = |l: &RectLayout| l.rects().iter().map(|r| r.depth).max().unwrap();
        assert!(max_depth(&zoomed) >= max_depth(&full));
        for r in zoomed.rects().iter() {
            assert!(r.x >= 0.0 && r.y >= 0.0 && r.x + r.w <= 800.0 && r.y + r.h <= 600.0);
        }
        assert!(
            zoomed
                .rects()
                .get(0)
                .unwrap()
                .flags
                .contains(NodeFlags::CLIPPED)
        );
    }

    #[test]
    fn hit_test_returns_deepest_and_chain() {
        let mut t = VecTree::new(0);
        let a = t.add_dir(VecTree::ROOT, 0);
        let f = t.add_file(a, 100, 0);
        t.add_file(VecTree::ROOT, 100, 0);
        let l = layout_treemap(&t, VecTree::ROOT, &cfg(800.0, 600.0));
        let fi = (0..l.len()).find(|&i| l.id(i) == Some(f)).unwrap();
        let r = l.rects().get(fi).unwrap();
        let p = l.pick(r.x + r.w / 2.0, r.y + r.h / 2.0).unwrap();
        assert_eq!(p.id, f);
        assert_eq!(p.ancestors, vec![VecTree::ROOT, a]);
        // The padding at the very corner belongs to the root.
        assert_eq!(l.pick(0.5, 0.5).unwrap().id, VecTree::ROOT);
        assert_eq!(l.hit_test(-1.0, 5.0), None);
        assert_eq!(l.hit_test(f32::NAN, 5.0), None);
    }

    #[test]
    fn into_reuses_and_matches_fresh() {
        let t = VecTree::synthetic(&crate::SyntheticSpec {
            nodes: 2_000,
            ..Default::default()
        });
        let c = cfg(640.0, 480.0);
        let mut reused = layout_treemap(&t, VecTree::ROOT, &cfg(100.0, 100.0));
        layout_treemap_into(&t, VecTree::ROOT, &c, &mut reused);
        assert_eq!(reused, layout_treemap(&t, VecTree::ROOT, &c));
    }
}
