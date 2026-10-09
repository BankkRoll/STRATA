//! Mind map: a depth-limited radial node-link tree (SPEC §16.2).
//!
//! # Algorithm
//!
//! 1. Collect the visible tree in pre-order: each directory shows its
//!    `max_children` largest children (size desc, id tie-break); the rest,
//!    plus zero-size children, become one "N more" aggregate node.
//! 2. Give every leaf weight 1 and every inner node the sum of its
//!    children's weights (one reverse pass over the pre-order list).
//! 3. Split angles top-down: the root owns the full circle and each node
//!    hands its children consecutive wedges proportional to their weight,
//!    so subtrees never cross. A node sits at the middle of its wedge, on
//!    the ring for its depth.
//! 4. Node radius grows with `√(size / root size)` between the configured
//!    bounds.
//!
//! Cost is O(V log V) for V visible nodes, and V ≤ Σ max_childrenᵈ.

use crate::buffer::{AggregateRecord, CircleRecord, NO_INDEX, NodeFlags, RecordBuf};
use crate::circles::{CircleLayout, CircleLayoutKind};
use crate::geom::{R64, Rect, sane_len};
use crate::source::{LayoutSource, NodeId};

const TAU: f64 = std::f64::consts::TAU;

/// Mind-map settings, in device pixels.
///
/// # Example
///
/// ```
/// use strata_layout::MindMapConfig;
/// let cfg = MindMapConfig::new(1280.0, 720.0, 1.0);
/// assert_eq!((cfg.max_depth, cfg.max_children), (3, 12));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct MindMapConfig {
    /// Canvas area; the root sits at its center.
    pub viewport: Rect,
    /// Deepest ring shown (root = 0).
    pub max_depth: u16,
    /// Children shown per node before the rest fold into "N more".
    pub max_children: u16,
    /// Radius of the smallest node.
    pub node_min_radius: f32,
    /// Radius of a node as large as the root.
    pub node_max_radius: f32,
    /// Space between the outermost nodes and the viewport edge.
    pub margin: f32,
}

impl MindMapConfig {
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
            max_depth: 3,
            max_children: 12,
            node_min_radius: 3.0 * s,
            node_max_radius: 24.0 * s,
            margin: 8.0 * s,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Node {
    id: NodeId,
    size: u64,
    parent: u32,
    depth: u16,
    flags: NodeFlags,
    agg: u32,
    weight: f64,
}

/// Lays out the subtree under `root` as a radial mind map.
///
/// Picking returns the top-most node whose circle contains the point; draw
/// an edge from every record to its `parent`.
///
/// # Example
///
/// ```
/// use strata_layout::{layout_mindmap, MindMapConfig, NodeFlags, VecTree};
///
/// let mut tree = VecTree::new(0);
/// for i in 0..20 {
///     tree.add_file(VecTree::ROOT, 100 + i, 0);
/// }
/// let l = layout_mindmap(&tree, VecTree::ROOT, &MindMapConfig::new(800.0, 800.0, 1.0));
/// // Root + 12 largest + one "8 more" node.
/// assert_eq!(l.circles().len(), 14);
/// assert!(l.circles().get(13).unwrap().flags.contains(NodeFlags::AGGREGATE));
/// assert_eq!(l.aggregates().get(0).unwrap().count, 8);
/// ```
#[must_use]
pub fn layout_mindmap<S: LayoutSource + ?Sized>(
    src: &S,
    root: NodeId,
    cfg: &MindMapConfig,
) -> CircleLayout {
    let mut out = CircleLayout {
        kind: CircleLayoutKind::MindMap,
        ..CircleLayout::default()
    };
    let vp = R64::from_rect(cfg.viewport);
    let min_r = sane_len(cfg.node_min_radius);
    let max_r = sane_len(cfg.node_max_radius).max(min_r);
    let radius = vp.w().min(vp.h()) / 2.0 - sane_len(cfg.margin) - max_r;
    if !(radius.is_finite() && radius > 0.0) {
        return out;
    }
    let keep = usize::from(cfg.max_children.max(1));
    let (nodes, subtree_end) = collect(src, root, cfg.max_depth, keep, &mut out.aggregates);

    let mut nodes = nodes;
    for i in (1..nodes.len()).rev() {
        let w = nodes[i].weight;
        let p = nodes[i].parent as usize;
        nodes[p].weight += w;
    }
    // Leaves started at 1 and inner nodes at 0, so every weight is now the
    // number of visible leaves below (or 1).

    let deepest = nodes.iter().map(|n| n.depth).max().unwrap_or(0).max(1);
    let ring = radius / f64::from(deepest);
    let root_size = nodes.first().map_or(0, |n| n.size) as f64;
    let center = ((vp.x0 + vp.x1) / 2.0, (vp.y0 + vp.y1) / 2.0);
    // (wedge start, wedge end, next free angle for children)
    let mut wedge: Vec<(f64, f64, f64)> = Vec::with_capacity(nodes.len());
    for (i, n) in nodes.iter().enumerate() {
        let (lo, hi) = if i == 0 {
            (0.0, TAU)
        } else {
            let p = n.parent as usize;
            let (plo, phi, cursor) = wedge[p];
            let span = (phi - plo) * n.weight / nodes[p].weight.max(1.0);
            wedge[p].2 = cursor + span;
            (cursor, cursor + span)
        };
        wedge.push((lo, hi, lo));
        let mid = (lo + hi) / 2.0;
        let dist = ring * f64::from(n.depth);
        let share = if root_size > 0.0 {
            (n.size as f64 / root_size).clamp(0.0, 1.0).sqrt()
        } else {
            0.0
        };
        let r = min_r + (max_r - min_r) * share;
        let color = if n.agg == NO_INDEX {
            src.color_key(n.id)
        } else {
            out.circles
                .get(n.parent as usize)
                .map_or(0, |p| p.color_key)
        };
        let index = out.circles.push(CircleRecord {
            cx: (center.0 + dist * mid.sin()) as f32,
            cy: (center.1 - dist * mid.cos()) as f32,
            r: r as f32,
            aux: mid as f32,
            id: n.id,
            color_key: color,
            parent: n.parent,
            depth: n.depth,
            flags: n.flags,
        });
        if n.agg != NO_INDEX
            && let Some(mut a) = out.aggregates.get(n.agg as usize)
        {
            a.record = index;
            out.aggregates.set(n.agg as usize, a);
        }
    }
    out.subtree_end = subtree_end;
    out
}

/// Builds the visible tree in pre-order with subtree skip pointers.
fn collect<S: LayoutSource + ?Sized>(
    src: &S,
    root: NodeId,
    max_depth: u16,
    keep: usize,
    aggregates: &mut RecordBuf<AggregateRecord>,
) -> (Vec<Node>, Vec<u32>) {
    let mut nodes: Vec<Node> = Vec::new();
    let mut ends: Vec<u32> = Vec::new();
    let mut kids: Vec<(NodeId, u64)> = Vec::new();
    // Pending children per open node: (record, arena start, next, end).
    let mut arena: Vec<(NodeId, u64, u32)> = Vec::new();
    let mut frames: Vec<(u32, usize, usize, usize)> = Vec::new();
    let mut pending = Some((root, src.size(root), NO_INDEX, 0u16, NO_INDEX));
    loop {
        if let Some((id, size, parent, depth, agg)) = pending.take() {
            let is_agg = agg != NO_INDEX;
            let is_dir = !is_agg && src.is_dir(id);
            let descend = is_dir && depth < max_depth;
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
            let index = nodes.len() as u32;
            nodes.push(Node {
                id,
                size,
                parent,
                depth,
                flags,
                agg,
                weight: 1.0,
            });
            ends.push(index + 1);
            if descend {
                kids.clear();
                src.children(id, &mut kids);
                let all = kids.len();
                kids.retain(|&(_, s)| s > 0);
                kids.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                let shown = kids.len().min(keep);
                let start = arena.len();
                arena.extend(kids[..shown].iter().map(|&(c, s)| (c, s, NO_INDEX)));
                if all > shown {
                    let bytes: u128 = kids[shown..].iter().map(|&(_, s)| u128::from(s)).sum();
                    let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
                    let a = aggregates.push(AggregateRecord {
                        record: NO_INDEX,
                        parent: index,
                        dir_id: id,
                        count: u32::try_from(all - shown).unwrap_or(u32::MAX),
                        bytes,
                    });
                    arena.push((id, bytes, a));
                }
                if arena.len() > start {
                    // Inner nodes get their weight from their leaves.
                    nodes[index as usize].weight = 0.0;
                    frames.push((index, start, start, arena.len()));
                }
            }
        }
        let Some(top) = frames.last_mut() else { break };
        if top.2 < top.3 {
            let (id, size, agg) = arena[top.2];
            top.2 += 1;
            let depth = nodes[top.0 as usize].depth + 1;
            pending = Some((id, size, top.0, depth, agg));
        } else {
            let (rec, start) = (top.0, top.1);
            frames.pop();
            ends[rec as usize] = nodes.len() as u32;
            arena.truncate(start);
        }
    }
    (nodes, ends)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hierarchy::Hierarchy;
    use crate::source::VecTree;

    #[test]
    fn wedges_nest_and_nodes_sit_on_rings() {
        let t = VecTree::synthetic(&crate::SyntheticSpec {
            nodes: 3_000,
            ..Default::default()
        });
        let cfg = MindMapConfig::new(1000.0, 1000.0, 1.0);
        let l = layout_mindmap(&t, VecTree::ROOT, &cfg);
        assert!(l.len() > 1);
        let recs: Vec<CircleRecord> = l.circles().iter().collect();
        let (cx, cy) = (recs[0].cx, recs[0].cy);
        let ring = recs
            .iter()
            .skip(1)
            .find(|r| r.depth == 1)
            .map(|r| ((r.cx - cx).powi(2) + (r.cy - cy).powi(2)).sqrt());
        for r in &recs {
            assert!(r.cx.is_finite() && r.cy.is_finite() && r.r > 0.0);
            assert!(r.depth <= cfg.max_depth);
            let d = ((r.cx - cx).powi(2) + (r.cy - cy).powi(2)).sqrt();
            assert!((d - ring.unwrap() * f32::from(r.depth)).abs() < 0.05);
            assert!(r.cx - r.r >= 0.0 && r.cx + r.r <= 1000.0);
        }
        // Every node's own record is the top-most hit at its center unless
        // a later node covers it.
        let last = recs.len() - 1;
        let p = l.pick(recs[last].cx, recs[last].cy).unwrap();
        assert_eq!(p.index as usize, last);
        assert_eq!(p.ancestors.len(), recs[last].depth as usize);
    }

    #[test]
    fn single_node_and_empty_viewport() {
        let t = VecTree::new(0);
        let l = layout_mindmap(&t, VecTree::ROOT, &MindMapConfig::new(300.0, 300.0, 1.0));
        assert_eq!(l.len(), 1);
        assert_eq!(l.pick(150.0, 150.0).unwrap().id, VecTree::ROOT);
        let e = layout_mindmap(&t, VecTree::ROOT, &MindMapConfig::new(0.0, 0.0, 1.0));
        assert!(e.is_empty());
    }
}
