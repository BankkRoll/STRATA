//! Sunburst: the hierarchy as concentric rings of sectors (SPEC Â§16.2).
//!
//! The root is the central disc; depth `d` occupies the ring
//! `[dÂ·ring, (d+1)Â·ring]`, where `ring = radius / (max_depth + 1)`. Each
//! directory's angular span is sliced among its children in proportion to
//! size (see `partition`), and children whose arc length at their outer
//! radius is below `min_px` are folded into one aggregate sector.
//!
//! Picking converts the point to polar coordinates, picks the ring from the
//! radius, then descends from the root choosing the child whose angular
//! range contains the angle: O(depth Ã— siblings scanned).

use crate::buffer::{AggregateRecord, ArcRecord, NO_INDEX, NodeFlags, RecordBuf, rd_f32};
use crate::geom::{R64, Rect, sane_len};
use crate::hierarchy::{Hierarchy, Pick, make_pick};
use crate::partition::{Scratch, Slice, Span, partition};
use crate::source::{LayoutSource, NodeId};

const TAU: f64 = std::f64::consts::TAU;

/// Sunburst settings, in device pixels.
///
/// # Example
///
/// ```
/// use strata_layout::SunburstConfig;
/// let cfg = SunburstConfig::new(1200.0, 800.0, 1.25);
/// assert_eq!(cfg.max_depth, 6);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct SunburstConfig {
    /// Canvas area; the chart is centered in it.
    pub viewport: Rect,
    /// Number of rings around the root disc.
    pub max_depth: u16,
    /// LOD threshold: minimum arc length (at the sector's outer radius).
    pub min_px: f32,
    /// Space between the outermost ring and the viewport edge.
    pub margin: f32,
}

impl SunburstConfig {
    /// DPI-aware defaults for a `width`Ã—`height` device-pixel canvas.
    #[must_use]
    pub fn new(width: f32, height: f32, dpi_scale: f32) -> Self {
        let s = if dpi_scale.is_finite() && dpi_scale > 0.0 {
            dpi_scale
        } else {
            1.0
        };
        Self {
            viewport: Rect::new(0.0, 0.0, width, height),
            max_depth: 6,
            min_px: 1.5 * s,
            margin: 4.0 * s,
        }
    }
}

/// Sectors in pre-order plus the aggregate side table.
///
/// The main buffer holds [`ArcRecord`]s (32 bytes each; see
/// [`crate::buffer`]); index 0 is the root disc. [`center`](Self::center)
/// and [`ring_width`](Self::ring_width) are needed to turn records into
/// vertices.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ArcLayout {
    center: (f32, f32),
    ring: f32,
    arcs: RecordBuf<ArcRecord>,
    aggregates: RecordBuf<AggregateRecord>,
    subtree_end: Vec<u32>,
}

impl Hierarchy for ArcLayout {
    fn node_bytes(&self) -> &[u8] {
        self.arcs.as_bytes()
    }
}

impl ArcLayout {
    /// Chart center in device pixels.
    #[must_use]
    pub fn center(&self) -> (f32, f32) {
        self.center
    }

    /// Radial thickness of one ring (and the root disc's radius).
    #[must_use]
    pub fn ring_width(&self) -> f32 {
        self.ring
    }

    /// The sector buffer.
    #[must_use]
    pub fn arcs(&self) -> &RecordBuf<ArcRecord> {
        &self.arcs
    }

    /// The small-items aggregate side table.
    #[must_use]
    pub fn aggregates(&self) -> &RecordBuf<AggregateRecord> {
        &self.aggregates
    }

    /// One past the last descendant of record `i`.
    #[must_use]
    pub fn subtree_end(&self, i: usize) -> Option<usize> {
        self.subtree_end.get(i).map(|&e| e as usize)
    }

    /// Record index of the sector under (`x`, `y`), or `None`.
    #[must_use]
    pub fn hit_test(&self, x: f32, y: f32) -> Option<u32> {
        if self.arcs.is_empty() || !x.is_finite() || !y.is_finite() || self.ring <= 0.0 {
            return None;
        }
        let dx = f64::from(x) - f64::from(self.center.0);
        let dy = f64::from(y) - f64::from(self.center.1);
        let r = dx.hypot(dy);
        let mut a = dx.atan2(-dy);
        if a < 0.0 {
            a += TAU;
        }
        let ring = (r / f64::from(self.ring)).floor();
        if !(0.0..f64::from(u16::MAX)).contains(&ring) {
            return None;
        }
        let target = ring as u16;
        let b = self.arcs.raw();
        let field = |i: usize, o: usize| f64::from(rd_f32(&b[i * 32..], o));
        let mut node = 0usize;
        for _ in 0..target {
            let end = self.subtree_end[node] as usize;
            let mut c = node + 1;
            let mut next = None;
            while c < end {
                if a >= field(c, 0) && a < field(c, 4) {
                    next = Some(c);
                    break;
                }
                c = self.subtree_end[c] as usize;
            }
            node = next?;
        }
        (r >= field(node, 8) && r < field(node, 12)).then_some(node as u32)
    }

    /// Picks the sector under (`x`, `y`) with its ancestor chain.
    #[must_use]
    pub fn pick(&self, x: f32, y: f32) -> Option<Pick> {
        let i = self.hit_test(x, y)?;
        make_pick(self, i as usize)
    }
}

/// Lays out the subtree under `root` as a sunburst.
///
/// # Example
///
/// ```
/// use strata_layout::{layout_sunburst, SunburstConfig, VecTree};
///
/// let mut tree = VecTree::new(0);
/// tree.add_file(VecTree::ROOT, 3, 0);
/// tree.add_file(VecTree::ROOT, 1, 0);
/// let l = layout_sunburst(&tree, VecTree::ROOT, &SunburstConfig::new(400.0, 400.0, 1.0));
/// let big = l.arcs().get(1).unwrap();
/// assert!((big.a1 - big.a0 - std::f32::consts::TAU * 0.75).abs() < 1e-5);
/// ```
#[must_use]
pub fn layout_sunburst<S: LayoutSource + ?Sized>(
    src: &S,
    root: NodeId,
    cfg: &SunburstConfig,
) -> ArcLayout {
    let mut out = ArcLayout::default();
    let vp = R64::from_rect(cfg.viewport);
    let min_px = sane_len(cfg.min_px).max(1e-3);
    let radius = vp.w().min(vp.h()) / 2.0 - sane_len(cfg.margin);
    let ring = radius / (f64::from(cfg.max_depth) + 1.0);
    if !(radius.is_finite() && ring > 0.0) {
        return out;
    }
    out.center = (
        ((vp.x0 + vp.x1) / 2.0) as f32,
        ((vp.y0 + vp.y1) / 2.0) as f32,
    );
    out.ring = ring as f32;

    let mut scratch = Scratch::default();
    let mut arena: Vec<Slice> = Vec::new();
    // (record index, depth, arena start, next, end)
    let mut frames: Vec<(u32, u16, usize, usize, usize)> = Vec::new();
    let root_slice = Slice {
        id: root,
        size: src.size(root),
        lo: 0.0,
        hi: TAU,
        agg: NO_INDEX,
    };
    let mut pending = Some((root_slice, NO_INDEX, 0u16));
    loop {
        if let Some((s, parent, depth)) = pending.take() {
            let is_agg = s.agg != NO_INDEX;
            let is_dir = !is_agg && src.is_dir(s.id);
            let descend = is_dir && depth < cfg.max_depth;
            let mut flags = if is_agg {
                NodeFlags::AGGREGATE
            } else {
                NodeFlags::SELECTABLE
            };
            if is_dir {
                flags |= NodeFlags::DIR;
            }
            if is_dir && !descend {
                flags |= NodeFlags::TRUNCATED;
            }
            let r0 = if depth == 0 {
                0.0
            } else {
                ring * f64::from(depth)
            };
            let index = out.arcs.push(ArcRecord {
                a0: s.lo as f32,
                a1: s.hi as f32,
                r0: r0 as f32,
                r1: (ring * (f64::from(depth) + 1.0)) as f32,
                id: s.id,
                color_key: if is_agg {
                    out.arcs.get(parent as usize).map_or(0, |p| p.color_key)
                } else {
                    src.color_key(s.id)
                },
                parent,
                depth,
                flags,
            });
            out.subtree_end.push(index + 1);
            if is_agg && let Some(mut a) = out.aggregates.get(s.agg as usize) {
                a.record = index;
                out.aggregates.set(s.agg as usize, a);
            }
            if descend {
                let start = arena.len();
                partition(
                    src,
                    Span {
                        dir: s.id,
                        dir_rec: index,
                        lo: s.lo,
                        hi: s.hi,
                        px_per_unit: ring * (f64::from(depth) + 2.0),
                        min_px,
                    },
                    &mut scratch,
                    &mut out.aggregates,
                    &mut arena,
                );
                if arena.len() > start {
                    frames.push((index, depth, start, start, arena.len()));
                }
            }
        }
        let Some(top) = frames.last_mut() else { break };
        if top.3 < top.4 {
            pending = Some((arena[top.3], top.0, top.1 + 1));
            top.3 += 1;
        } else {
            let (rec, start) = (top.0, top.2);
            frames.pop();
            out.subtree_end[rec as usize] = out.arcs.len() as u32;
            arena.truncate(start);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::VecTree;

    #[test]
    fn hit_test_resolves_rings_and_angles() {
        let mut t = VecTree::new(0);
        let a = t.add_dir(VecTree::ROOT, 0);
        let a1 = t.add_file(a, 50, 0);
        let b = t.add_file(VecTree::ROOT, 50, 0);
        let cfg = SunburstConfig {
            margin: 0.0,
            max_depth: 3,
            ..SunburstConfig::new(400.0, 400.0, 1.0)
        };
        let l = layout_sunburst(&t, VecTree::ROOT, &cfg);
        assert_eq!(l.ring_width(), 50.0);
        // Center â†’ root.
        assert_eq!(l.pick(200.0, 200.0).unwrap().id, VecTree::ROOT);
        // `a` spans the first half (12 â†’ 6 o'clock, clockwise = right side).
        assert_eq!(l.pick(275.0, 200.0).unwrap().id, a);
        assert_eq!(l.pick(125.0, 200.0).unwrap().id, b);
        let deep = l.pick(325.0, 200.0).unwrap();
        assert_eq!(
            (deep.id, deep.ancestors.clone()),
            (a1, vec![VecTree::ROOT, a])
        );
        // Ring 2 on the `b` side is empty (b is a file).
        assert_eq!(l.hit_test(75.0, 200.0), None);
        assert_eq!(l.hit_test(f32::NAN, 0.0), None);
    }

    #[test]
    fn degenerate_inputs_are_safe() {
        let mut t = VecTree::new(0);
        t.add_file(VecTree::ROOT, 0, 0);
        for (w, h) in [(0.0, 0.0), (f32::NAN, 5.0), (8.0, 8.0)] {
            let l = layout_sunburst(&t, VecTree::ROOT, &SunburstConfig::new(w, h, 1.0));
            for a in l.arcs().iter() {
                assert!(
                    a.a0.is_finite() && a.a1.is_finite() && a.r0.is_finite() && a.r1.is_finite()
                );
            }
        }
        let l = layout_sunburst(&t, VecTree::ROOT, &SunburstConfig::new(300.0, 300.0, 1.0));
        assert_eq!(l.len(), 1);
        assert_eq!(l.aggregates().get(0).unwrap().count, 1);
    }
}
