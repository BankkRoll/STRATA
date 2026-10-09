//! [`RectLayout`]: the output of the treemap and icicle layouts, and its
//! picking.
//!
//! # Picking
//!
//! Picking walks the pre-order buffer top-down. From a record, its children
//! are found by hopping over each child's subtree with the `subtree_end`
//! skip table, so a query touches only the children of the directories on
//! the path to the hit, never the whole buffer.
//!
//! Scanning siblings is linear, which matters only for very wide
//! directories (a folder with a million files at 1 px each). Those get a
//! per-directory uniform grid over their children, built once at layout
//! time, so a query costs O(depth × entries per cell) whatever the fan-out.
//! Measured numbers are in `docs/BENCHMARKS.md`.
//!
//! [`HitGrid`] is a whole-layout grid kept as a benchmarked alternative.

use std::collections::HashMap;

use crate::buffer::{AggregateRecord, CushionRecord, LabelRecord, RecordBuf, RectRecord, rd_f32};
use crate::hierarchy::{Hierarchy, Pick, make_pick};

/// Directories with more emitted children than this get a child grid.
const WIDE_DIR: usize = 256;

/// How the rects relate geometrically, which decides how picking descends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RectLayoutKind {
    /// Children nest inside their parent's rect.
    #[default]
    Treemap,
    /// Children sit in the next row and share their parent's x-range.
    Icicle,
}

/// Rectangles in pre-order, plus side tables, ready for WebGL instancing.
///
/// - [`rects`](Self::rects): one [`RectRecord`] per drawn block. Index 0 is
///   the layout root (when anything was emitted).
/// - [`aggregates`](Self::aggregates): one [`AggregateRecord`] per
///   directory that had children folded away by LOD.
/// - [`labels`](Self::labels): [`LabelRecord`] candidates big enough for text.
/// - [`cushions`](Self::cushions): optional [`CushionRecord`] per rect,
///   parallel to `rects`.
///
/// # Example
///
/// ```
/// use strata_layout::{layout_treemap, TreemapConfig, VecTree};
///
/// let mut tree = VecTree::new(0);
/// let f = tree.add_file(VecTree::ROOT, 100, 3);
/// let layout = layout_treemap(&tree, VecTree::ROOT, &TreemapConfig::new(200.0, 100.0, 1.0));
/// let bytes = layout.rects().as_bytes(); // send over a Tauri Channel
/// assert_eq!(bytes.len(), layout.rects().len() * 32);
/// assert_eq!(layout.pick(100.0, 50.0).map(|p| p.id), Some(f));
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RectLayout {
    pub(crate) kind: RectLayoutKind,
    pub(crate) rects: RecordBuf<RectRecord>,
    pub(crate) aggregates: RecordBuf<AggregateRecord>,
    pub(crate) labels: RecordBuf<LabelRecord>,
    pub(crate) cushions: Option<RecordBuf<CushionRecord>>,
    pub(crate) subtree_end: Vec<u32>,
    /// Child grids of wide directories, keyed by the directory's record.
    wide: HashMap<u32, CellIndex>,
}

impl Hierarchy for RectLayout {
    fn node_bytes(&self) -> &[u8] {
        self.rects.as_bytes()
    }
}

impl RectLayout {
    /// Creates an empty layout (reuse it with the `*_into` functions).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Geometry kind (treemap or icicle).
    #[must_use]
    pub fn kind(&self) -> RectLayoutKind {
        self.kind
    }

    /// The main rect buffer.
    #[must_use]
    pub fn rects(&self) -> &RecordBuf<RectRecord> {
        &self.rects
    }

    /// The small-items aggregate side table.
    #[must_use]
    pub fn aggregates(&self) -> &RecordBuf<AggregateRecord> {
        &self.aggregates
    }

    /// Label candidates.
    #[must_use]
    pub fn labels(&self) -> &RecordBuf<LabelRecord> {
        &self.labels
    }

    /// Cushion coefficients, when requested in the config.
    #[must_use]
    pub fn cushions(&self) -> Option<&RecordBuf<CushionRecord>> {
        self.cushions.as_ref()
    }

    /// One past the last descendant of record `i` (pre-order skip pointer).
    /// Record `i`'s descendants are exactly `i + 1 .. subtree_end(i)`.
    #[must_use]
    pub fn subtree_end(&self, i: usize) -> Option<usize> {
        self.subtree_end.get(i).map(|&e| e as usize)
    }

    pub(crate) fn reset(&mut self, kind: RectLayoutKind, cushions: bool) {
        self.kind = kind;
        self.rects.clear();
        self.aggregates.clear();
        self.labels.clear();
        self.subtree_end.clear();
        self.wide.clear();
        match (&mut self.cushions, cushions) {
            (Some(c), true) => c.clear(),
            (slot, true) => *slot = Some(RecordBuf::new()),
            (slot, false) => *slot = None,
        }
    }

    /// Builds child grids for wide directories. Called once at the end of a
    /// nested (treemap) layout; O(records).
    pub(crate) fn index_wide_dirs(&mut self) {
        let n = self.rects.len();
        let mut counts = vec![0u32; n];
        for i in 1..n {
            let p = rd_u32_at(self.rects.raw(), i, 24) as usize;
            if p < n {
                counts[p] += 1;
            }
        }
        let mut entries = Vec::new();
        for (dir, &count) in counts.iter().enumerate() {
            if (count as usize) <= WIDE_DIR {
                continue;
            }
            entries.clear();
            let end = self.subtree_end[dir] as usize;
            let mut c = dir + 1;
            while c < end {
                entries.push((c as u32, self.edges(c)));
                c = self.subtree_end[c] as usize;
            }
            let (x0, y0, x1, y1) = self.edges(dir);
            // Aim for a handful of children per cell.
            let cell = ((x1 - x0) * (y1 - y0) / count as f32 * 4.0).sqrt();
            self.wide
                .insert(dir as u32, CellIndex::build(&entries, cell));
        }
    }

    #[inline]
    fn edges(&self, i: usize) -> (f32, f32, f32, f32) {
        let b = &self.rects.raw()[i * 32..i * 32 + 16];
        let x = rd_f32(b, 0);
        let y = rd_f32(b, 4);
        (x, y, x + rd_f32(b, 8), y + rd_f32(b, 12))
    }

    #[inline]
    fn contains(&self, i: usize, x: f32, y: f32) -> bool {
        let (x0, y0, x1, y1) = self.edges(i);
        x >= x0 && y >= y0 && x < x1 && y < y1
    }

    /// Record index of the deepest rect containing (`x`, `y`), or `None`.
    ///
    /// Points in a directory's padding or header resolve to the directory.
    /// Edges are half-open (left/top inclusive), so shared edges resolve to
    /// exactly one sibling.
    #[must_use]
    pub fn hit_test(&self, x: f32, y: f32) -> Option<u32> {
        if self.rects.is_empty() || !x.is_finite() || !y.is_finite() {
            return None;
        }
        match self.kind {
            RectLayoutKind::Treemap => self.hit_nested(x, y),
            RectLayoutKind::Icicle => self.hit_stacked(x, y),
        }
    }

    fn hit_nested(&self, x: f32, y: f32) -> Option<u32> {
        if !self.contains(0, x, y) {
            return None;
        }
        let mut node = 0usize;
        'descend: loop {
            if let Some(grid) = self.wide.get(&(node as u32)) {
                // Siblings are disjoint, so at most one child in the cell
                // contains the point.
                match grid
                    .cell(x, y)
                    .iter()
                    .find(|&&c| self.contains(c as usize, x, y))
                {
                    Some(&c) => {
                        node = c as usize;
                        continue 'descend;
                    }
                    None => return Some(node as u32),
                }
            }
            let end = self.subtree_end[node] as usize;
            let mut c = node + 1;
            while c < end {
                if self.contains(c, x, y) {
                    node = c;
                    continue 'descend;
                }
                c = self.subtree_end[c] as usize;
            }
            return Some(node as u32);
        }
    }

    fn hit_stacked(&self, x: f32, y: f32) -> Option<u32> {
        let mut node = 0usize;
        loop {
            let (x0, _, x1, _) = self.edges(node);
            if x < x0 || x >= x1 {
                return None;
            }
            if self.contains(node, x, y) {
                return Some(node as u32);
            }
            let end = self.subtree_end[node] as usize;
            let mut c = node + 1;
            let mut next = None;
            while c < end {
                let (cx0, _, cx1, _) = self.edges(c);
                if x >= cx0 && x < cx1 {
                    next = Some(c);
                    break;
                }
                c = self.subtree_end[c] as usize;
            }
            node = next?;
        }
    }

    /// Picks the deepest rect at (`x`, `y`) with its ancestor chain.
    #[must_use]
    pub fn pick(&self, x: f32, y: f32) -> Option<Pick> {
        let i = self.hit_test(x, y)?;
        make_pick(self, i as usize)
    }
}

fn rd_u32_at(b: &[u8], i: usize, off: usize) -> u32 {
    crate::buffer::rd_u32(&b[i * 32..], off)
}

/// Rect edges (x0, y0, x1, y1).
type Edges = (f32, f32, f32, f32);

/// Uniform grid over a set of rects: each rect is listed in every cell it
/// overlaps (compressed-sparse-row storage, entries in input order).
#[derive(Debug, Clone, Default, PartialEq)]
struct CellIndex {
    x0: f32,
    y0: f32,
    cell: f32,
    cols: usize,
    rows: usize,
    starts: Vec<u32>,
    items: Vec<u32>,
}

impl CellIndex {
    /// Hard cap on cell count so a tiny cell size cannot exhaust memory.
    const MAX_CELLS: usize = 1 << 20;

    fn build(entries: &[(u32, Edges)], cell_px: f32) -> Self {
        if entries.is_empty() {
            return Self::default();
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for &(_, (a, b, c, d)) in entries {
            x0 = x0.min(a);
            y0 = y0.min(b);
            x1 = x1.max(c);
            y1 = y1.max(d);
        }
        let (w, h) = ((x1 - x0).max(1.0), (y1 - y0).max(1.0));
        let mut cell = if cell_px.is_finite() && cell_px >= 1.0 {
            cell_px
        } else {
            16.0
        };
        let cap = Self::MAX_CELLS.min(entries.len().saturating_mul(4).max(64));
        while ((w / cell).ceil() as usize).saturating_mul((h / cell).ceil() as usize) > cap {
            cell *= 1.5;
        }
        let cols = ((w / cell).ceil() as usize).max(1);
        let rows = ((h / cell).ceil() as usize).max(1);
        let span = |(a, b, c, d): (f32, f32, f32, f32)| {
            (
                (((a - x0) / cell) as usize).min(cols - 1),
                (((b - y0) / cell) as usize).min(rows - 1),
                (((c - x0) / cell) as usize).min(cols - 1),
                (((d - y0) / cell) as usize).min(rows - 1),
            )
        };
        // Two-pass CSR fill: count, prefix-sum, then place.
        let mut starts = vec![0u32; cols * rows + 1];
        for &(_, e) in entries {
            let (cx0, cy0, cx1, cy1) = span(e);
            for cy in cy0..=cy1 {
                for cx in cx0..=cx1 {
                    starts[cy * cols + cx + 1] += 1;
                }
            }
        }
        for k in 1..starts.len() {
            starts[k] += starts[k - 1];
        }
        let mut items = vec![0u32; starts[cols * rows] as usize];
        let mut fill = starts.clone();
        for &(i, e) in entries {
            let (cx0, cy0, cx1, cy1) = span(e);
            for cy in cy0..=cy1 {
                for cx in cx0..=cx1 {
                    let slot = &mut fill[cy * cols + cx];
                    items[*slot as usize] = i;
                    *slot += 1;
                }
            }
        }
        Self {
            x0,
            y0,
            cell,
            cols,
            rows,
            starts,
            items,
        }
    }

    /// Entries of the cell under (`x`, `y`); empty outside the grid.
    fn cell(&self, x: f32, y: f32) -> &[u32] {
        if self.cols == 0 {
            return &[];
        }
        let fx = (x - self.x0) / self.cell;
        let fy = (y - self.y0) / self.cell;
        if !(fx >= 0.0 && fy >= 0.0) {
            return &[];
        }
        let (cx, cy) = (fx as usize, fy as usize);
        if cx >= self.cols || cy >= self.rows {
            return &[];
        }
        let c = cy * self.cols + cx;
        &self.items[self.starts[c] as usize..self.starts[c + 1] as usize]
    }
}

/// Whole-layout uniform-grid picking index over a [`RectLayout`].
///
/// Every rect is registered in each cell it overlaps; a query scans one
/// cell's list backwards (pre-order means the deepest containing rect comes
/// last). It answers the same queries as [`RectLayout::hit_test`], which is
/// as fast in practice and needs no build step; the grid remains useful
/// for many queries against one layout, such as marquee selection.
///
/// # Example
///
/// ```
/// use strata_layout::{layout_treemap, HitGrid, SyntheticSpec, TreemapConfig, VecTree};
///
/// let tree = VecTree::synthetic(&SyntheticSpec { nodes: 2_000, ..SyntheticSpec::default() });
/// let layout = layout_treemap(&tree, VecTree::ROOT, &TreemapConfig::new(800.0, 600.0, 1.0));
/// let grid = HitGrid::build(&layout, 16.0);
/// assert_eq!(grid.hit_test(&layout, 123.0, 456.0), layout.hit_test(123.0, 456.0));
/// ```
#[derive(Debug, Clone, Default)]
pub struct HitGrid {
    index: CellIndex,
}

impl HitGrid {
    /// Builds a grid with square cells of roughly `cell_px` device pixels.
    #[must_use]
    pub fn build(layout: &RectLayout, cell_px: f32) -> Self {
        let entries: Vec<_> = (0..layout.rects.len())
            .map(|i| (i as u32, layout.edges(i)))
            .collect();
        Self {
            index: CellIndex::build(&entries, cell_px),
        }
    }

    /// Same contract as [`RectLayout::hit_test`]; `layout` must be the one
    /// the grid was built from.
    #[must_use]
    pub fn hit_test(&self, layout: &RectLayout, x: f32, y: f32) -> Option<u32> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        self.index
            .cell(x, y)
            .iter()
            .rev()
            .copied()
            .find(|&i| (i as usize) < layout.rects.len() && layout.contains(i as usize, x, y))
    }

    /// Total cell entries (memory diagnostic).
    #[must_use]
    pub fn entries(&self) -> usize {
        self.index.items.len()
    }
}
