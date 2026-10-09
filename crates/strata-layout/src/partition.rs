//! Proportional 1-D slicing shared by the sunburst (angles) and the icicle
//! (x-ranges), including the LOD cut and the aggregate side record.
//!
//! Children are sorted by size descending (id tie-break) and laid out from
//! `lo` with a running cursor; the last emitted slice snaps to `hi` when
//! nothing follows it, so siblings tile the span exactly. Children whose
//! slice would measure less than `min_px` on screen are folded into one
//! aggregate slice at the end. Cost: O(n log n) for the sort.

use crate::buffer::{AggregateRecord, NO_INDEX, RecordBuf};
use crate::source::{LayoutSource, NodeId};

/// One child slice of a parent's span.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Slice {
    pub id: NodeId,
    pub size: u64,
    pub lo: f64,
    pub hi: f64,
    /// Aggregate side-table index, or `NO_INDEX` for a real entry.
    pub agg: u32,
}

/// Reusable buffers for [`partition`].
#[derive(Debug, Default)]
pub(crate) struct Scratch {
    kids: Vec<(NodeId, u64)>,
}

/// Inputs describing the parent span.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Span {
    pub dir: NodeId,
    pub dir_rec: u32,
    pub lo: f64,
    pub hi: f64,
    /// Converts span units to on-screen pixels for the LOD test (the outer
    /// radius for angles, 1 for x-ranges).
    pub px_per_unit: f64,
    pub min_px: f64,
}

/// Slices `span.dir`'s children over `[lo, hi]`, appending to `out` and
/// recording any excluded children in `aggregates`.
pub(crate) fn partition<S: LayoutSource + ?Sized>(
    src: &S,
    span: Span,
    scratch: &mut Scratch,
    aggregates: &mut RecordBuf<AggregateRecord>,
    out: &mut Vec<Slice>,
) {
    let kids = &mut scratch.kids;
    kids.clear();
    src.children(span.dir, kids);
    let all = kids.len();
    kids.retain(|&(_, s)| s > 0);
    kids.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let total: u128 = kids.iter().map(|&(_, s)| u128::from(s)).sum();
    let width = span.hi - span.lo;
    let (k, cut) = if total > 0 && width > 0.0 {
        let k = width / total as f64;
        let px = k * span.px_per_unit;
        (
            k,
            kids.partition_point(|&(_, s)| s as f64 * px >= span.min_px),
        )
    } else {
        (0.0, 0)
    };
    let tail: u128 = kids[cut..].iter().map(|&(_, s)| u128::from(s)).sum();
    let excluded = all - cut;
    let agg = if excluded > 0 {
        aggregates.push(AggregateRecord {
            record: NO_INDEX,
            parent: span.dir_rec,
            dir_id: span.dir,
            count: u32::try_from(excluded).unwrap_or(u32::MAX),
            bytes: u64::try_from(tail).unwrap_or(u64::MAX),
        })
    } else {
        NO_INDEX
    };
    let mut cursor = span.lo;
    for (i, &(id, size)) in kids[..cut].iter().enumerate() {
        let hi = if i + 1 == cut && tail == 0 {
            span.hi
        } else {
            (cursor + size as f64 * k).min(span.hi)
        };
        out.push(Slice {
            id,
            size,
            lo: cursor,
            hi,
            agg: NO_INDEX,
        });
        cursor = hi;
    }
    if tail > 0 && (span.hi - cursor) * span.px_per_unit >= span.min_px {
        out.push(Slice {
            id: span.dir,
            size: u64::try_from(tail).unwrap_or(u64::MAX),
            lo: cursor,
            hi: span.hi,
            agg,
        });
    }
}
