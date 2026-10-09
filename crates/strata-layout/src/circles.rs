//! [`CircleLayout`]: output of the circle-packing and mind-map layouts, and
//! its picking.

use crate::buffer::{AggregateRecord, CircleRecord, LabelRecord, RecordBuf, rd_f32};
use crate::hierarchy::{Hierarchy, Pick, make_pick};

/// How circles relate geometrically, which decides how picking works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CircleLayoutKind {
    /// Circle packing: children nest inside their parent and never overlap.
    #[default]
    Pack,
    /// Mind map: free-standing nodes linked to their parent by edges.
    MindMap,
}

/// Circles in pre-order plus side tables.
///
/// The main buffer holds [`CircleRecord`]s (32 bytes; see
/// [`crate::buffer`]). For the mind map, draw an edge from each record to
/// `parent`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CircleLayout {
    pub(crate) kind: CircleLayoutKind,
    pub(crate) circles: RecordBuf<CircleRecord>,
    pub(crate) aggregates: RecordBuf<AggregateRecord>,
    pub(crate) labels: RecordBuf<LabelRecord>,
    pub(crate) subtree_end: Vec<u32>,
}

impl Hierarchy for CircleLayout {
    fn node_bytes(&self) -> &[u8] {
        self.circles.as_bytes()
    }
}

impl CircleLayout {
    /// Pack or mind map.
    #[must_use]
    pub fn kind(&self) -> CircleLayoutKind {
        self.kind
    }

    /// The circle buffer.
    #[must_use]
    pub fn circles(&self) -> &RecordBuf<CircleRecord> {
        &self.circles
    }

    /// The small-items aggregate side table.
    #[must_use]
    pub fn aggregates(&self) -> &RecordBuf<AggregateRecord> {
        &self.aggregates
    }

    /// Label candidates (leaf circles big enough for text; the box is the
    /// inscribed square).
    #[must_use]
    pub fn labels(&self) -> &RecordBuf<LabelRecord> {
        &self.labels
    }

    /// One past the last descendant of record `i`.
    #[must_use]
    pub fn subtree_end(&self, i: usize) -> Option<usize> {
        self.subtree_end.get(i).map(|&e| e as usize)
    }

    #[inline]
    fn contains(&self, i: usize, x: f64, y: f64) -> bool {
        let b = &self.circles.raw()[i * 32..i * 32 + 12];
        let dx = x - f64::from(rd_f32(b, 0));
        let dy = y - f64::from(rd_f32(b, 4));
        let r = f64::from(rd_f32(b, 8));
        dx * dx + dy * dy < r * r
    }

    /// Record index of the deepest (pack) or top-most (mind map) circle
    /// containing (`x`, `y`).
    #[must_use]
    pub fn hit_test(&self, x: f32, y: f32) -> Option<u32> {
        if self.circles.is_empty() || !x.is_finite() || !y.is_finite() {
            return None;
        }
        let (x, y) = (f64::from(x), f64::from(y));
        match self.kind {
            CircleLayoutKind::Pack => {
                if !self.contains(0, x, y) {
                    return None;
                }
                let mut node = 0usize;
                'descend: loop {
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
            // Mind-map nodes are few (bounded by max_children^depth) and may
            // overlap; later records draw on top, so the last hit wins.
            CircleLayoutKind::MindMap => (0..self.circles.len())
                .rev()
                .find(|&i| self.contains(i, x, y))
                .map(|i| i as u32),
        }
    }

    /// Picks the circle under (`x`, `y`) with its ancestor chain.
    #[must_use]
    pub fn pick(&self, x: f32, y: f32) -> Option<Pick> {
        let i = self.hit_test(x, y)?;
        make_pick(self, i as usize)
    }
}
