//! Read access to the tree structure encoded in any layout's main buffer.
//!
//! Rect, arc and circle records all share the same 16-byte tail
//! (`id, color_key, parent, depth, flags` at offset 16), so parent chains,
//! picking results and transition matching work the same for every view.

use crate::buffer::{NO_INDEX, NodeFlags, rd_u16, rd_u32};
use crate::source::NodeId;

/// Stride shared by all node records.
pub(crate) const NODE_STRIDE: usize = 32;

/// Identity of a record for matching across layouts: the entry id plus
/// whether it is an aggregate (an aggregate carries its directory's id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeKey {
    /// Entry id (directory id for aggregates).
    pub id: NodeId,
    /// Whether the record is a "N small items" aggregate.
    pub aggregate: bool,
}

/// A layout whose main buffer holds 32-byte node records in pre-order.
///
/// Implemented by [`RectLayout`](crate::RectLayout),
/// [`ArcLayout`](crate::ArcLayout) and [`CircleLayout`](crate::CircleLayout).
///
/// # Example
///
/// ```
/// use strata_layout::{layout_treemap, Hierarchy, TreemapConfig, VecTree};
///
/// let mut tree = VecTree::new(0);
/// let d = tree.add_dir(VecTree::ROOT, 0);
/// let f = tree.add_file(d, 10, 0);
/// let layout = layout_treemap(&tree, VecTree::ROOT, &TreemapConfig::new(400.0, 300.0, 1.0));
/// let leaf = (0..layout.len()).find(|&i| layout.id(i) == Some(f)).unwrap();
/// let mut chain = Vec::new();
/// layout.ancestor_ids(leaf, &mut chain);
/// assert_eq!(chain, vec![VecTree::ROOT, d]);
/// ```
pub trait Hierarchy {
    /// The encoded main buffer (32-byte records).
    fn node_bytes(&self) -> &[u8];

    /// Number of records.
    fn len(&self) -> usize {
        self.node_bytes().len() / NODE_STRIDE
    }

    /// Whether the layout is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Entry id of record `i`.
    fn id(&self, i: usize) -> Option<NodeId> {
        tail(self.node_bytes(), i).map(|t| t.0)
    }

    /// Parent record index of record `i` (`None` for the root).
    fn parent(&self, i: usize) -> Option<usize> {
        tail(self.node_bytes(), i).and_then(|t| (t.1 != NO_INDEX).then_some(t.1 as usize))
    }

    /// Depth of record `i`.
    fn depth(&self, i: usize) -> Option<u16> {
        tail(self.node_bytes(), i).map(|t| t.2)
    }

    /// Flags of record `i`.
    fn flags(&self, i: usize) -> Option<NodeFlags> {
        tail(self.node_bytes(), i).map(|t| t.3)
    }

    /// Matching key of record `i`.
    fn key(&self, i: usize) -> Option<NodeKey> {
        tail(self.node_bytes(), i).map(|t| NodeKey {
            id: t.0,
            aggregate: t.3.contains(NodeFlags::AGGREGATE),
        })
    }

    /// Writes the ids of every ancestor of record `i` into `out`,
    /// root first, excluding `i` itself. `out` is cleared first.
    fn ancestor_ids(&self, i: usize, out: &mut Vec<NodeId>) {
        out.clear();
        let mut cur = self.parent(i);
        // Parent indices always point backwards in pre-order; the bound
        // guards against a corrupted buffer looping forever.
        let mut guard = self.len();
        while let Some(p) = cur {
            if guard == 0 {
                break;
            }
            guard -= 1;
            if let Some(id) = self.id(p) {
                out.push(id);
            }
            cur = self.parent(p);
        }
        out.reverse();
    }
}

/// Reads `(id, parent, depth, flags)` of record `i`.
fn tail(b: &[u8], i: usize) -> Option<(NodeId, u32, u16, NodeFlags)> {
    let s = i.checked_mul(NODE_STRIDE)?;
    let r = b.get(s..s + NODE_STRIDE)?;
    Some((
        rd_u32(r, 16),
        rd_u32(r, 24),
        rd_u16(r, 28),
        NodeFlags::from_bits(rd_u16(r, 30)),
    ))
}

/// Result of picking a point.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pick {
    /// Record index of the deepest record under the point.
    pub index: u32,
    /// Its entry id (the directory id when `flags` has `AGGREGATE`).
    pub id: NodeId,
    /// Its flags.
    pub flags: NodeFlags,
    /// Ancestor ids, root first, excluding `id`.
    pub ancestors: Vec<NodeId>,
}

pub(crate) fn make_pick<H: Hierarchy + ?Sized>(h: &H, index: usize) -> Option<Pick> {
    let id = h.id(index)?;
    let flags = h.flags(index)?;
    let mut ancestors = Vec::new();
    h.ancestor_ids(index, &mut ancestors);
    Some(Pick {
        index: index as u32,
        id,
        flags,
        ancestors,
    })
}
