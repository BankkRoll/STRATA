//! The input side of every layout: the [`LayoutSource`] trait and the
//! in-crate [`VecTree`] implementation used by tests, benches and demos.
//!
//! Layout never owns the file tree. It asks a source for one directory's
//! children at a time, so the index (`strata-index`) can answer from its
//! struct-of-arrays storage without materializing anything, and layout only
//! touches the part of the tree that ends up on screen.

/// Identifier of one entry, as handed out by the [`LayoutSource`].
///
/// Ids are `u32` because the index addresses entries with `u32` (SPEC §9.1)
/// and because they are written verbatim into the binary output buffers.
pub type NodeId = u32;

/// Read-only view of a sized tree that layouts consume.
///
/// The index implements this for the live tree. Sizes are whatever the
/// active `strata_core::SizeMode` says (allocated or logical); layout
/// only needs them to be consistent between a parent and its children.
///
/// # Example
///
/// ```
/// use strata_layout::{LayoutSource, NodeId, VecTree};
///
/// let mut tree = VecTree::new(0);
/// let docs = tree.add_dir(VecTree::ROOT, 1);
/// tree.add_file(docs, 4096, 2);
///
/// let mut kids = Vec::new();
/// tree.children(VecTree::ROOT, &mut kids);
/// assert_eq!(kids, vec![(docs, 4096)]);
/// assert!(tree.is_dir(docs));
/// ```
pub trait LayoutSource {
    /// Size of `id` in bytes, in the active size mode.
    fn size(&self, id: NodeId) -> u64;

    /// Appends `(child, size)` for every direct child of `id` to `out`.
    ///
    /// Order does not matter: layouts sort by size (descending) with the id as
    /// a tie-break, so the output is deterministic regardless. Files append
    /// nothing. Callers clear `out` beforehand.
    fn children(&self, id: NodeId, out: &mut Vec<(NodeId, u64)>);

    /// Whether `id` is a directory (drawn with padding and a header strip,
    /// and descended into).
    fn is_dir(&self, id: NodeId) -> bool;

    /// Opaque color key copied into the output records.
    ///
    /// Typically a category, extension bucket, age bucket or app id; the
    /// frontend maps it to a palette entry. Layout never interprets it.
    fn color_key(&self, id: NodeId) -> u32;
}

impl<T: LayoutSource + ?Sized> LayoutSource for &T {
    fn size(&self, id: NodeId) -> u64 {
        (**self).size(id)
    }
    fn children(&self, id: NodeId, out: &mut Vec<(NodeId, u64)>) {
        (**self).children(id, out);
    }
    fn is_dir(&self, id: NodeId) -> bool {
        (**self).is_dir(id)
    }
    fn color_key(&self, id: NodeId) -> u32 {
        (**self).color_key(id)
    }
}

#[derive(Debug, Clone)]
struct VecNode {
    parent: NodeId,
    size: u64,
    dir: bool,
    color_key: u32,
    children: Vec<NodeId>,
}

/// A simple owned tree implementing [`LayoutSource`].
///
/// Node 0 is the root directory. Ids are dense and assigned in creation
/// order, so a parent always has a smaller id than its children. Directory
/// sizes are maintained incrementally as files are added.
///
/// # Example
///
/// ```
/// use strata_layout::{LayoutSource, VecTree};
///
/// let mut tree = VecTree::new(0);
/// let a = tree.add_dir(VecTree::ROOT, 0);
/// tree.add_file(a, 100, 0);
/// tree.add_file(VecTree::ROOT, 50, 0);
/// assert_eq!(tree.size(VecTree::ROOT), 150);
/// assert_eq!(tree.size(a), 100);
/// ```
#[derive(Debug, Clone)]
pub struct VecTree {
    nodes: Vec<VecNode>,
}

impl VecTree {
    /// Id of the root directory.
    pub const ROOT: NodeId = 0;

    /// Creates a tree holding only an empty root directory.
    #[must_use]
    pub fn new(root_color_key: u32) -> Self {
        Self {
            nodes: vec![VecNode {
                parent: Self::ROOT,
                size: 0,
                dir: true,
                color_key: root_color_key,
                children: Vec::new(),
            }],
        }
    }

    /// Number of nodes, including the root.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Always `false`: the root always exists.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Parent of `id` (the root is its own parent), or `None` for an unknown id.
    #[must_use]
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes.get(id as usize).map(|n| n.parent)
    }

    /// Adds an empty directory under `parent` and returns its id.
    ///
    /// # Panics
    ///
    /// Panics if `parent` is not an existing id.
    pub fn add_dir(&mut self, parent: NodeId, color_key: u32) -> NodeId {
        self.push(parent, 0, true, color_key)
    }

    /// Adds a file of `size` bytes under `parent` and returns its id. Every
    /// ancestor's size grows by `size`.
    ///
    /// # Panics
    ///
    /// Panics if `parent` is not an existing id.
    pub fn add_file(&mut self, parent: NodeId, size: u64, color_key: u32) -> NodeId {
        self.push(parent, size, false, color_key)
    }

    fn push(&mut self, parent: NodeId, size: u64, dir: bool, color_key: u32) -> NodeId {
        assert!(
            (parent as usize) < self.nodes.len(),
            "VecTree: parent {parent} does not exist"
        );
        let id = NodeId::try_from(self.nodes.len()).expect("VecTree holds at most u32::MAX nodes");
        self.nodes.push(VecNode {
            parent,
            size,
            dir,
            color_key,
            children: Vec::new(),
        });
        // Adding a child turns a file into a directory; layouts rely on
        // `is_dir` to decide whether to descend.
        self.nodes[parent as usize].dir = true;
        self.nodes[parent as usize].children.push(id);
        let mut p = parent;
        loop {
            let node = &mut self.nodes[p as usize];
            node.size = node.size.saturating_add(size);
            if p == Self::ROOT {
                break;
            }
            p = node.parent;
        }
        id
    }

    /// Permutes every child list with a seeded shuffle without changing ids
    /// or sizes. Used to check that layouts do not depend on input order.
    pub fn shuffle_children(&mut self, seed: u64) {
        let mut rng = SplitMix64::new(seed);
        for node in &mut self.nodes {
            let c = &mut node.children;
            for i in (1..c.len()).rev() {
                let j = rng.below(i as u64 + 1) as usize;
                c.swap(i, j);
            }
        }
    }

    /// Builds a pseudo-random tree for benchmarks and demos.
    ///
    /// Directories are expanded breadth-first until `spec.nodes` nodes exist.
    /// File sizes are log-uniform between 1 B and 4 GiB, with a small share of
    /// zero-byte files, which resembles real volumes closely enough to
    /// exercise LOD.
    ///
    /// # Example
    ///
    /// ```
    /// use strata_layout::{SyntheticSpec, VecTree};
    ///
    /// let tree = VecTree::synthetic(&SyntheticSpec { nodes: 1_000, ..SyntheticSpec::default() });
    /// assert_eq!(tree.len(), 1_000);
    /// ```
    #[must_use]
    pub fn synthetic(spec: &SyntheticSpec) -> Self {
        let mut rng = SplitMix64::new(spec.seed);
        let mut tree = Self::new(0);
        let target = spec.nodes.max(1);
        let mut queue = std::collections::VecDeque::new();
        queue.push_back((Self::ROOT, 0u16));
        let fanout = u64::from(spec.mean_fanout.max(1));
        while tree.len() < target {
            let Some((dir, depth)) = queue.pop_front() else {
                // Every directory got its children but the budget is not
                // spent: keep adding files under the root's subtree.
                queue.push_back((Self::ROOT, 0));
                continue;
            };
            let kids = 1 + rng.below(2 * fanout);
            for _ in 0..kids {
                if tree.len() >= target {
                    break;
                }
                let color = rng.below(15) as u32;
                let make_dir = depth + 1 < spec.max_depth && rng.unit() < f64::from(spec.dir_ratio);
                if make_dir {
                    let d = tree.add_dir(dir, color);
                    queue.push_back((d, depth + 1));
                } else {
                    let size = if rng.unit() < 0.02 {
                        0
                    } else {
                        (rng.unit() * (4.0 * 1024.0 * 1024.0 * 1024.0f64).ln()).exp() as u64
                    };
                    tree.add_file(dir, size, color);
                }
            }
        }
        tree
    }
}

impl LayoutSource for VecTree {
    fn size(&self, id: NodeId) -> u64 {
        self.nodes.get(id as usize).map_or(0, |n| n.size)
    }

    fn children(&self, id: NodeId, out: &mut Vec<(NodeId, u64)>) {
        if let Some(n) = self.nodes.get(id as usize) {
            out.extend(n.children.iter().map(|&c| (c, self.nodes[c as usize].size)));
        }
    }

    fn is_dir(&self, id: NodeId) -> bool {
        self.nodes.get(id as usize).is_some_and(|n| n.dir)
    }

    fn color_key(&self, id: NodeId) -> u32 {
        self.nodes.get(id as usize).map_or(0, |n| n.color_key)
    }
}

/// Shape of a [`VecTree::synthetic`] tree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SyntheticSpec {
    /// Total node count, including the root.
    pub nodes: usize,
    /// Mean number of children per directory.
    pub mean_fanout: u32,
    /// Probability that a new child is a directory (while above `max_depth`).
    pub dir_ratio: f32,
    /// Deepest level at which directories are still created.
    pub max_depth: u16,
    /// PRNG seed; the same spec always yields the same tree.
    pub seed: u64,
}

impl Default for SyntheticSpec {
    fn default() -> Self {
        Self {
            nodes: 10_000,
            mean_fanout: 24,
            dir_ratio: 0.2,
            max_depth: 10,
            seed: 0x5EED,
        }
    }
}

/// Tiny deterministic PRNG so the crate needs no `rand` dependency.
#[derive(Debug, Clone)]
pub(crate) struct SplitMix64(u64);

impl SplitMix64 {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`); the modulo bias is irrelevant here.
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    /// Uniform in `[0, 1)`.
    pub(crate) fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_propagate_to_every_ancestor() {
        let mut t = VecTree::new(0);
        let a = t.add_dir(VecTree::ROOT, 0);
        let b = t.add_dir(a, 0);
        t.add_file(b, 7, 0);
        t.add_file(a, 3, 0);
        assert_eq!(t.size(b), 7);
        assert_eq!(t.size(a), 10);
        assert_eq!(t.size(VecTree::ROOT), 10);
    }

    #[test]
    fn unknown_ids_are_harmless() {
        let t = VecTree::new(0);
        let mut out = Vec::new();
        t.children(99, &mut out);
        assert!(out.is_empty());
        assert_eq!(t.size(99), 0);
        assert!(!t.is_dir(99));
    }

    #[test]
    fn synthetic_is_deterministic_and_deep() {
        let spec = SyntheticSpec {
            nodes: 5_000,
            ..SyntheticSpec::default()
        };
        let a = VecTree::synthetic(&spec);
        let b = VecTree::synthetic(&spec);
        assert_eq!(a.len(), 5_000);
        let sizes_a: Vec<u64> = (0..a.len() as u32).map(|i| a.size(i)).collect();
        let sizes_b: Vec<u64> = (0..b.len() as u32).map(|i| b.size(i)).collect();
        assert_eq!(sizes_a, sizes_b);
    }

    #[test]
    fn shuffle_keeps_child_sets() {
        let mut t = VecTree::synthetic(&SyntheticSpec {
            nodes: 500,
            ..SyntheticSpec::default()
        });
        let mut before = Vec::new();
        t.children(VecTree::ROOT, &mut before);
        t.shuffle_children(7);
        let mut after = Vec::new();
        t.children(VecTree::ROOT, &mut after);
        before.sort_unstable();
        after.sort_unstable();
        assert_eq!(before, after);
    }
}
