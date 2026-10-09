//! Icicle and flame layouts: the hierarchy as stacked horizontal bars
//! (SPEC §16.2).
//!
//! Depth `d` is row `d`; each directory's x-range is sliced among its
//! children in proportion to size (see `partition`), with sub-`min_px`
//! children folded into one aggregate bar. The icicle hangs from the top;
//! the flame graph grows from the bottom. Output is a [`RectLayout`] of kind
//! [`RectLayoutKind::Icicle`], so it shares the treemap's record format,
//! label table, and transition support.

use crate::buffer::{LabelRecord, NO_INDEX, NodeFlags, RectRecord};
use crate::geom::{R64, Rect, sane_len};
use crate::partition::{Scratch, Slice, Span, partition};
use crate::rects::{RectLayout, RectLayoutKind};
use crate::source::{LayoutSource, NodeId};

/// Which way the bars stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IcicleOrientation {
    /// Root on top, children below (icicle).
    #[default]
    TopDown,
    /// Root at the bottom, children above (flame graph).
    BottomUp,
}

/// Icicle settings, in device pixels.
///
/// # Example
///
/// ```
/// use strata_layout::{IcicleConfig, IcicleOrientation};
/// let mut cfg = IcicleConfig::new(1600.0, 900.0, 1.0);
/// cfg.orientation = IcicleOrientation::BottomUp;
/// assert_eq!(cfg.row_height(), 100.0);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct IcicleConfig {
    /// Canvas area.
    pub viewport: Rect,
    /// Deepest row laid out (root = row 0).
    pub max_depth: u16,
    /// Row height; `None` divides the viewport height into `max_depth + 1`
    /// rows.
    pub row_height: Option<f32>,
    /// Vertical gap left at the bottom of each row.
    pub row_gap: f32,
    /// LOD threshold: minimum bar width.
    pub min_px: f32,
    /// Stacking direction.
    pub orientation: IcicleOrientation,
    /// Inset of the label box inside each bar.
    pub label_inset: f32,
    /// Minimum label box width.
    pub label_min_width: f32,
    /// Minimum label box height.
    pub label_min_height: f32,
}

impl IcicleConfig {
    /// DPI-aware defaults for a `width`×`height` device-pixel canvas.
    #[must_use]
    pub fn new(width: f32, height: f32, dpi_scale: f32) -> Self {
        let s = if dpi_scale.is_finite() && dpi_scale > 0.0 {
            dpi_scale
        } else {
            1.0
        };
        Self {
            viewport: Rect::new(0.0, 0.0, width, height),
            max_depth: 8,
            row_height: None,
            row_gap: s,
            min_px: 1.0,
            orientation: IcicleOrientation::TopDown,
            label_inset: 3.0 * s,
            label_min_width: 40.0 * s,
            label_min_height: 12.0 * s,
        }
    }

    /// The effective row height.
    #[must_use]
    pub fn row_height(&self) -> f32 {
        match self.row_height {
            Some(h) if h.is_finite() && h > 0.0 => h,
            _ => self.viewport.h / (f32::from(self.max_depth) + 1.0),
        }
    }
}

/// Lays out the subtree under `root` as an icicle (or flame graph).
///
/// # Example
///
/// ```
/// use strata_layout::{layout_icicle, IcicleConfig, VecTree};
///
/// let mut tree = VecTree::new(0);
/// let d = tree.add_dir(VecTree::ROOT, 0);
/// tree.add_file(d, 30, 0);
/// tree.add_file(VecTree::ROOT, 10, 0);
/// let l = layout_icicle(&tree, VecTree::ROOT, &IcicleConfig::new(400.0, 300.0, 1.0));
/// let dir = l.rects().get(1).unwrap();
/// assert_eq!((dir.id, dir.w), (d, 300.0));
/// assert_eq!(l.pick(10.0, 80.0).unwrap().id, d + 1); // the 30-byte file, row 2
/// ```
#[must_use]
pub fn layout_icicle<S: LayoutSource + ?Sized>(
    src: &S,
    root: NodeId,
    cfg: &IcicleConfig,
) -> RectLayout {
    let mut out = RectLayout::new();
    layout_icicle_into(src, root, cfg, &mut out);
    out
}

/// Like [`layout_icicle`] but reuses `out`'s allocations.
pub fn layout_icicle_into<S: LayoutSource + ?Sized>(
    src: &S,
    root: NodeId,
    cfg: &IcicleConfig,
    out: &mut RectLayout,
) {
    out.reset(RectLayoutKind::Icicle, false);
    let vp = R64::from_rect(cfg.viewport);
    let row = f64::from(cfg.row_height());
    let min_px = sane_len(cfg.min_px).max(1e-3);
    if !(row.is_finite() && row > 0.0 && vp.w() >= min_px) {
        return;
    }
    let gap = sane_len(cfg.row_gap);
    let bar_h = if gap < row { row - gap } else { row };
    let inset = sane_len(cfg.label_inset);
    let (lw, lh) = (
        sane_len(cfg.label_min_width),
        sane_len(cfg.label_min_height),
    );
    let row_y = |depth: u16| match cfg.orientation {
        IcicleOrientation::TopDown => vp.y0 + row * f64::from(depth),
        IcicleOrientation::BottomUp => {
            vp.y1 - row * (f64::from(depth) + 1.0) + gap.min(row - bar_h)
        }
    };

    let mut scratch = Scratch::default();
    let mut arena: Vec<Slice> = Vec::new();
    let mut frames: Vec<(u32, u16, u32, usize, usize, usize)> = Vec::new();
    let mut pending = Some((
        Slice {
            id: root,
            size: src.size(root),
            lo: vp.x0,
            hi: vp.x1,
            agg: NO_INDEX,
        },
        NO_INDEX,
        0u16,
        0u32,
    ));
    loop {
        if let Some((s, parent, depth, parent_color)) = pending.take() {
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
            let y0 = row_y(depth);
            let bar = R64 {
                x0: s.lo,
                y0,
                x1: s.hi,
                y1: y0 + bar_h,
            };
            let r = bar.to_rect();
            let color = if is_agg {
                parent_color
            } else {
                src.color_key(s.id)
            };
            let index = out.rects.push(RectRecord {
                x: r.x,
                y: r.y,
                w: r.w,
                h: r.h,
                id: s.id,
                color_key: color,
                parent,
                depth,
                flags,
            });
            out.subtree_end.push(index + 1);
            if is_agg && let Some(mut a) = out.aggregates.get(s.agg as usize) {
                a.record = index;
                out.aggregates.set(s.agg as usize, a);
            }
            let label = bar.inset(inset, inset, inset, inset);
            if label.w() >= lw && label.h() >= lh {
                let b = label.to_rect();
                out.labels.push(LabelRecord {
                    record: index,
                    id: s.id,
                    x: b.x,
                    y: b.y,
                    w: b.w,
                    h: b.h,
                    size: s.size,
                });
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
                        px_per_unit: 1.0,
                        min_px,
                    },
                    &mut scratch,
                    &mut out.aggregates,
                    &mut arena,
                );
                if arena.len() > start {
                    frames.push((index, depth, color, start, start, arena.len()));
                }
            }
        }
        let Some(top) = frames.last_mut() else { break };
        if top.4 < top.5 {
            pending = Some((arena[top.4], top.0, top.1 + 1, top.2));
            top.4 += 1;
        } else {
            let (rec, start) = (top.0, top.3);
            frames.pop();
            out.subtree_end[rec as usize] = out.rects.len() as u32;
            arena.truncate(start);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::VecTree;

    #[test]
    fn flame_grows_upward() {
        let mut t = VecTree::new(0);
        let f = t.add_file(VecTree::ROOT, 5, 0);
        let mut cfg = IcicleConfig::new(200.0, 100.0, 1.0);
        cfg.max_depth = 1;
        cfg.row_gap = 0.0;
        cfg.orientation = IcicleOrientation::BottomUp;
        let l = layout_icicle(&t, VecTree::ROOT, &cfg);
        let root = l.rects().get(0).unwrap();
        let child = l.rects().get(1).unwrap();
        assert_eq!((root.y, root.h), (50.0, 50.0));
        assert_eq!((child.y, child.id), (0.0, f));
        assert_eq!(l.hit_test(100.0, 75.0), Some(0));
        assert_eq!(l.hit_test(100.0, 25.0), Some(1));
    }

    #[test]
    fn gaps_between_rows_hit_nothing() {
        let mut t = VecTree::new(0);
        t.add_file(VecTree::ROOT, 5, 0);
        let mut cfg = IcicleConfig::new(200.0, 100.0, 1.0);
        cfg.max_depth = 1;
        cfg.row_gap = 10.0;
        let l = layout_icicle(&t, VecTree::ROOT, &cfg);
        assert_eq!(l.hit_test(100.0, 45.0), None);
        assert_eq!(l.hit_test(100.0, 55.0), Some(1));
        assert_eq!(l.hit_test(250.0, 5.0), None);
    }

    #[test]
    fn degenerate_viewports_are_empty() {
        let t = VecTree::new(0);
        for (w, h) in [(0.0, 0.0), (f32::NAN, 1.0), (10.0, 0.0)] {
            assert!(
                layout_icicle(&t, VecTree::ROOT, &IcicleConfig::new(w, h, 1.0))
                    .rects()
                    .is_empty()
            );
        }
    }
}
