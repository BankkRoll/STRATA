//! Circle packing ("bubbles", SPEC §16.2).
//!
//! # Sibling packing (front chain)
//!
//! A port of d3-hierarchy's `packSiblings`, itself based on Wang et al.,
//! "Visualization of large hierarchical data by circle packing" (2006):
//!
//! 1. Place the first two circles tangent at the origin, the third tangent
//!    to both. These three form the initial *front chain*, a cyclic list of
//!    the circles on the pack's outer boundary.
//! 2. For each next circle, place it tangent to the chain pair (`a`, `b`)
//!    closest to the origin.
//! 3. Walk the chain outward from both sides of that pair (alternating by
//!    accumulated radius). If the new circle intersects chain circle `j`,
//!    drop the chain segment between the pair and `j`, make `j` the new
//!    partner, and retry the placement.
//! 4. Otherwise splice the circle into the chain between `a` and `b`, and
//!    pick the chain pair whose tangent point is closest to the origin for
//!    the next circle.
//!
//! Each insertion scans the chain, so the cost is O(n·c) with c the chain
//! length (≈ O(√n) in practice). Larger circles go first (input is sorted by
//! size), which gives the familiar compact packs.
//!
//! # Enclosing circle (Welzl)
//!
//! The smallest circle enclosing the front chain is found with Welzl's
//! move-to-front algorithm over a deterministically shuffled copy (expected
//! O(n)); its basis has at most three circles, solved in closed form. On
//! numeric failure it falls back to a bounding circle around the centroid,
//! which is larger but always valid.
//!
//! # Hierarchy (top-down)
//!
//! Each directory packs its children with radii `√(size / largest)`, then
//! scales the pack to fit its own circle minus padding. Working top-down
//! (rather than d3's bottom-up radii) lets LOD and the depth limit skip
//! everything too small to see, so the cost tracks what is on screen.
//! Children whose circle could not reach `min_px` in diameter even at the
//! densest possible scale are folded into an aggregate circle before
//! packing; the scale bound is `R / √Σr²`, because a pack's enclosing radius
//! is at least the radius of a disc of the same total area.

use crate::buffer::{AggregateRecord, CircleRecord, LabelRecord, NO_INDEX, NodeFlags};
use crate::circles::{CircleLayout, CircleLayoutKind};
use crate::geom::{R64, Rect, sane_len};
use crate::source::{LayoutSource, NodeId, SplitMix64};

/// A circle in pack-local coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Circle {
    pub x: f64,
    pub y: f64,
    pub r: f64,
}

/// Places `c` tangent to both `a` and `b`.
fn place(b: Circle, a: Circle, c: &mut Circle) {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let d2 = dx * dx + dy * dy;
    if d2 > 0.0 {
        let a2 = (a.r + c.r) * (a.r + c.r);
        let b2 = (b.r + c.r) * (b.r + c.r);
        if a2 > b2 {
            let x = (d2 + b2 - a2) / (2.0 * d2);
            let y = (b2 / d2 - x * x).max(0.0).sqrt();
            c.x = b.x - x * dx - y * dy;
            c.y = b.y - x * dy + y * dx;
        } else {
            let x = (d2 + a2 - b2) / (2.0 * d2);
            let y = (a2 / d2 - x * x).max(0.0).sqrt();
            c.x = a.x + x * dx - y * dy;
            c.y = a.y + x * dy + y * dx;
        }
    } else {
        c.x = a.x + c.r;
        c.y = a.y;
    }
}

fn intersects(a: Circle, b: Circle) -> bool {
    let dr = a.r + b.r - 1e-6;
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    dr > 0.0 && dr * dr > dx * dx + dy * dy
}

/// Packs `c` (radii set, positions overwritten) around the origin and
/// recenters the result on its enclosing circle. Returns that circle's
/// radius. `next`/`prev` are scratch.
pub(crate) fn pack_siblings(c: &mut [Circle], next: &mut Vec<usize>, prev: &mut Vec<usize>) -> f64 {
    let n = c.len();
    if n == 0 {
        return 0.0;
    }
    c[0].x = 0.0;
    c[0].y = 0.0;
    if n == 1 {
        return c[0].r;
    }
    c[0].x = -c[1].r;
    c[1].x = c[0].r;
    c[1].y = 0.0;
    if n == 2 {
        let e = Circle {
            x: (c[0].x + c[1].x + (c[1].r - c[0].r)) / 2.0,
            y: 0.0,
            r: c[0].r + c[1].r,
        };
        for p in c.iter_mut() {
            p.x -= e.x;
        }
        return e.r;
    }
    let (c0, c1) = (c[0], c[1]);
    place(c1, c0, &mut c[2]);
    next.clear();
    prev.clear();
    next.resize(n, 0);
    prev.resize(n, 0);
    let (mut a, mut b) = (0usize, 1usize);
    next[a] = b;
    prev[2] = b;
    next[b] = 2;
    prev[a] = 2;
    next[2] = a;
    prev[b] = a;

    let score = |c: &[Circle], next: &[usize], node: usize| {
        let p = c[node];
        let q = c[next[node]];
        let ab = p.r + q.r;
        let dx = (p.x * q.r + q.x * p.r) / ab;
        let dy = (p.y * q.r + q.y * p.r) / ab;
        dx * dx + dy * dy
    };

    let mut i = 3;
    let mut retries = 0usize;
    while i < n {
        let (ca, cb) = (c[a], c[b]);
        place(ca, cb, &mut c[i]);
        let ci = c[i];
        let (mut j, mut k) = (next[b], prev[a]);
        let (mut sj, mut sk) = (cb.r, ca.r);
        let mut retry = false;
        // Bounded retries: with finite positive radii the chain shrinks on
        // every retry, but the cap guarantees termination even if rounding
        // ever stops that.
        if retries <= n + 8 {
            loop {
                if sj <= sk {
                    if intersects(c[j], ci) {
                        b = j;
                        next[a] = b;
                        prev[b] = a;
                        retry = true;
                        break;
                    }
                    sj += c[j].r;
                    j = next[j];
                } else {
                    if intersects(c[k], ci) {
                        a = k;
                        next[a] = b;
                        prev[b] = a;
                        retry = true;
                        break;
                    }
                    sk += c[k].r;
                    k = prev[k];
                }
                if j == next[k] {
                    break;
                }
            }
        }
        if retry {
            retries += 1;
            continue;
        }
        retries = 0;
        prev[i] = a;
        next[i] = b;
        next[a] = i;
        prev[b] = i;
        b = i;
        let mut aa = score(c, next, a);
        let mut cur = next[i];
        while cur != b {
            let s = score(c, next, cur);
            if s < aa {
                a = cur;
                aa = s;
            }
            cur = next[cur];
        }
        b = next[a];
        i += 1;
    }

    let mut chain = vec![c[b]];
    let mut cur = next[b];
    while cur != b && chain.len() <= n {
        chain.push(c[cur]);
        cur = next[cur];
    }
    let e = enclose(&mut chain, c);
    for p in c.iter_mut() {
        p.x -= e.x;
        p.y -= e.y;
    }
    e.r
}

/// Smallest circle enclosing `chain` (shuffled in place), checked against
/// every circle in `all`; falls back to a centroid bound on failure.
fn enclose(chain: &mut [Circle], all: &[Circle]) -> Circle {
    let mut rng = SplitMix64::new(0x00C1_4C1E);
    for i in (1..chain.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        chain.swap(i, j);
    }
    let welzl = welzl(chain).filter(|e| {
        e.r.is_finite()
            && e.x.is_finite()
            && e.y.is_finite()
            && all.iter().all(|p| {
                let d = ((p.x - e.x).powi(2) + (p.y - e.y).powi(2)).sqrt() + p.r;
                d <= e.r * (1.0 + 1e-9) + 1e-9
            })
    });
    welzl.unwrap_or_else(|| {
        let n = all.len().max(1) as f64;
        let x = all.iter().map(|p| p.x).sum::<f64>() / n;
        let y = all.iter().map(|p| p.y).sum::<f64>() / n;
        let r = all
            .iter()
            .map(|p| ((p.x - x).powi(2) + (p.y - y).powi(2)).sqrt() + p.r)
            .fold(0.0, f64::max);
        Circle { x, y, r }
    })
}

fn welzl(circles: &[Circle]) -> Option<Circle> {
    let mut basis: Vec<Circle> = Vec::with_capacity(3);
    let mut e: Option<Circle> = None;
    let mut i = 0;
    let mut steps = 0usize;
    let cap = circles.len().saturating_mul(64).max(64);
    while i < circles.len() {
        steps += 1;
        if steps > cap {
            return None;
        }
        let p = circles[i];
        if e.is_some_and(|e| encloses_weak(e, p)) {
            i += 1;
        } else {
            basis = extend_basis(&basis, p)?;
            e = Some(enclose_basis(&basis)?);
            i = 0;
        }
    }
    e
}

fn extend_basis(b: &[Circle], p: Circle) -> Option<Vec<Circle>> {
    if encloses_weak_all(p, b) {
        return Some(vec![p]);
    }
    for &bi in b {
        if encloses_not(p, bi) && encloses_weak_all(enclose2(bi, p), b) {
            return Some(vec![bi, p]);
        }
    }
    for i in 0..b.len().saturating_sub(1) {
        for j in i + 1..b.len() {
            let (bi, bj) = (b[i], b[j]);
            if encloses_not(enclose2(bi, bj), p)
                && encloses_not(enclose2(bi, p), bj)
                && encloses_not(enclose2(bj, p), bi)
                && let Some(e3) = enclose3(bi, bj, p)
                && encloses_weak_all(e3, b)
            {
                return Some(vec![bi, bj, p]);
            }
        }
    }
    None
}

fn encloses_not(a: Circle, b: Circle) -> bool {
    let dr = a.r - b.r;
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    dr < 0.0 || dr * dr < dx * dx + dy * dy
}

fn encloses_weak(a: Circle, b: Circle) -> bool {
    let dr = a.r - b.r + a.r.max(b.r).max(1.0) * 1e-9;
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    dr > 0.0 && dr * dr > dx * dx + dy * dy
}

fn encloses_weak_all(a: Circle, b: &[Circle]) -> bool {
    b.iter().all(|&p| encloses_weak(a, p))
}

fn enclose_basis(b: &[Circle]) -> Option<Circle> {
    match b {
        [a] => Some(*a),
        [a, b] => Some(enclose2(*a, *b)),
        [a, b, c] => enclose3(*a, *b, *c),
        _ => None,
    }
}

fn enclose2(a: Circle, b: Circle) -> Circle {
    let (x21, y21, r21) = (b.x - a.x, b.y - a.y, b.r - a.r);
    let l = (x21 * x21 + y21 * y21).sqrt();
    if l == 0.0 {
        return if a.r >= b.r { a } else { b };
    }
    Circle {
        x: (a.x + b.x + x21 / l * r21) / 2.0,
        y: (a.y + b.y + y21 / l * r21) / 2.0,
        r: (l + a.r + b.r) / 2.0,
    }
}

fn enclose3(a: Circle, b: Circle, c: Circle) -> Option<Circle> {
    let (x1, y1, r1) = (a.x, a.y, a.r);
    let (x2, y2, r2) = (b.x, b.y, b.r);
    let (x3, y3, r3) = (c.x, c.y, c.r);
    let a2 = x1 - x2;
    let a3 = x1 - x3;
    let b2 = y1 - y2;
    let b3 = y1 - y3;
    let c2 = r2 - r1;
    let c3 = r3 - r1;
    let d1 = x1 * x1 + y1 * y1 - r1 * r1;
    let d2 = d1 - x2 * x2 - y2 * y2 + r2 * r2;
    let d3 = d1 - x3 * x3 - y3 * y3 + r3 * r3;
    let ab = a3 * b2 - a2 * b3;
    if ab == 0.0 {
        return None;
    }
    let xa = (b2 * d3 - b3 * d2) / (ab * 2.0) - x1;
    let xb = (b3 * c2 - b2 * c3) / ab;
    let ya = (a3 * d2 - a2 * d3) / (ab * 2.0) - y1;
    let yb = (a2 * c3 - a3 * c2) / ab;
    let qa = xb * xb + yb * yb - 1.0;
    let qb = 2.0 * (r1 + xa * xb + ya * yb);
    let qc = xa * xa + ya * ya - r1 * r1;
    let r = -(if qa.abs() > 1e-6 {
        (qb + (qb * qb - 4.0 * qa * qc).sqrt()) / (2.0 * qa)
    } else {
        qc / qb
    });
    let e = Circle {
        x: x1 + xa + xb * r,
        y: y1 + ya + yb * r,
        r,
    };
    (e.x.is_finite() && e.y.is_finite() && e.r.is_finite()).then_some(e)
}

/// Circle-packing settings, in device pixels.
///
/// # Example
///
/// ```
/// use strata_layout::PackConfig;
/// let cfg = PackConfig::new(1000.0, 800.0, 2.0);
/// assert_eq!(cfg.padding, 6.0);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct PackConfig {
    /// Canvas area; the root circle is centered and inscribed.
    pub viewport: Rect,
    /// Gap between the root circle and the viewport edge.
    pub margin: f32,
    /// Gap between a directory's circle and the pack of its children.
    pub padding: f32,
    /// Gap between sibling circles.
    pub sibling_gap: f32,
    /// LOD threshold: minimum circle diameter.
    pub min_px: f32,
    /// Deepest level laid out (root = 0).
    pub max_depth: u16,
    /// Minimum label box side (the inscribed square of a leaf circle).
    pub label_min_size: f32,
}

impl PackConfig {
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
            margin: 4.0 * s,
            padding: 3.0 * s,
            sibling_gap: 1.0 * s,
            min_px: 2.0,
            max_depth: 64,
            label_min_size: 28.0 * s,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Item {
    id: NodeId,
    size: u64,
    c: Circle,
    agg: u32,
}

#[derive(Debug, Default)]
struct Scratch {
    kids: Vec<(NodeId, u64)>,
    circles: Vec<Circle>,
    next: Vec<usize>,
    prev: Vec<usize>,
}

/// Lays out the subtree under `root` as nested packed circles.
///
/// # Example
///
/// ```
/// use strata_layout::{layout_pack, PackConfig, VecTree};
///
/// let mut tree = VecTree::new(0);
/// for size in [400, 100, 100] {
///     tree.add_file(VecTree::ROOT, size, 0);
/// }
/// let l = layout_pack(&tree, VecTree::ROOT, &PackConfig::new(600.0, 600.0, 1.0));
/// assert_eq!(l.circles().len(), 4);
/// let (big, small) = (l.circles().get(1).unwrap(), l.circles().get(2).unwrap());
/// assert!(big.r > small.r);
/// ```
#[must_use]
pub fn layout_pack<S: LayoutSource + ?Sized>(
    src: &S,
    root: NodeId,
    cfg: &PackConfig,
) -> CircleLayout {
    let mut out = CircleLayout {
        kind: CircleLayoutKind::Pack,
        ..CircleLayout::default()
    };
    let vp = R64::from_rect(cfg.viewport);
    let min_px = sane_len(cfg.min_px).max(1e-3);
    let pad = sane_len(cfg.padding);
    let gap = sane_len(cfg.sibling_gap);
    let label_min = sane_len(cfg.label_min_size);
    let radius = vp.w().min(vp.h()) / 2.0 - sane_len(cfg.margin);
    if !(radius.is_finite() && 2.0 * radius >= min_px) {
        return out;
    }
    let mut scratch = Scratch::default();
    let mut arena: Vec<Item> = Vec::new();
    let mut frames: Vec<(u32, u16, u32, usize, usize, usize)> = Vec::new();
    let mut pending = Some((
        Item {
            id: root,
            size: src.size(root),
            c: Circle {
                x: (vp.x0 + vp.x1) / 2.0,
                y: (vp.y0 + vp.y1) / 2.0,
                r: radius,
            },
            agg: NO_INDEX,
        },
        NO_INDEX,
        0u16,
        0u32,
    ));
    loop {
        if let Some((it, parent, depth, parent_color)) = pending.take() {
            let is_agg = it.agg != NO_INDEX;
            let is_dir = !is_agg && src.is_dir(it.id);
            let content = it.c.r - pad;
            let descend = is_dir && depth < cfg.max_depth && 2.0 * content >= min_px;
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
            let color = if is_agg {
                parent_color
            } else {
                src.color_key(it.id)
            };
            let index = out.circles.push(CircleRecord {
                cx: it.c.x as f32,
                cy: it.c.y as f32,
                r: it.c.r as f32,
                aux: 0.0,
                id: it.id,
                color_key: color,
                parent,
                depth,
                flags,
            });
            out.subtree_end.push(index + 1);
            if is_agg && let Some(mut a) = out.aggregates.get(it.agg as usize) {
                a.record = index;
                out.aggregates.set(it.agg as usize, a);
            }
            let side = it.c.r * std::f64::consts::SQRT_2;
            if !descend && side >= label_min {
                let h = side / 2.0;
                out.labels.push(LabelRecord {
                    record: index,
                    id: it.id,
                    x: (it.c.x - h) as f32,
                    y: (it.c.y - h) as f32,
                    w: side as f32,
                    h: side as f32,
                    size: it.size,
                });
            }
            if descend {
                let start = arena.len();
                pack_children(
                    src,
                    it.id,
                    index,
                    Circle { r: content, ..it.c },
                    min_px,
                    gap,
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
            out.subtree_end[rec as usize] = out.circles.len() as u32;
            arena.truncate(start);
        }
    }
    out
}

/// Packs `dir`'s children into `content` and appends them to `arena`.
#[allow(clippy::too_many_arguments)]
fn pack_children<S: LayoutSource + ?Sized>(
    src: &S,
    dir: NodeId,
    dir_rec: u32,
    content: Circle,
    min_px: f64,
    gap: f64,
    s: &mut Scratch,
    aggregates: &mut crate::buffer::RecordBuf<AggregateRecord>,
    arena: &mut Vec<Item>,
) {
    s.kids.clear();
    src.children(dir, &mut s.kids);
    let all = s.kids.len();
    s.kids.retain(|&(_, z)| z > 0);
    s.kids
        .sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut cut = 0;
    let mut tail: u128 = 0;
    let mut dropped: Vec<usize> = Vec::new();
    let mut agg_kept = false;
    if let Some(&(_, largest)) = s.kids.first() {
        let largest = largest as f64;
        let rel = |z: u64| (z as f64 / largest).sqrt();
        let sum_r2: f64 = s.kids.iter().map(|&(_, z)| z as f64 / largest).sum();
        let max_scale = content.r / sum_r2.sqrt();
        cut = s
            .kids
            .partition_point(|&(_, z)| 2.0 * rel(z) * max_scale - gap >= min_px);
        tail = s.kids[cut..].iter().map(|&(_, z)| u128::from(z)).sum();
        s.circles.clear();
        s.circles.extend(s.kids[..cut].iter().map(|&(_, z)| Circle {
            r: rel(z),
            ..Circle::default()
        }));
        if tail > 0 {
            s.circles.push(Circle {
                r: (tail as f64 / largest).sqrt(),
                ..Circle::default()
            });
        }
        let enclosing = pack_siblings(&mut s.circles, &mut s.next, &mut s.prev);
        let scale = if enclosing > 0.0 {
            content.r / enclosing
        } else {
            0.0
        };
        for p in s.circles.iter_mut() {
            p.x = content.x + p.x * scale;
            p.y = content.y + p.y * scale;
            p.r = (p.r * scale - gap / 2.0).max(0.0);
        }
        for i in 0..cut {
            if 2.0 * s.circles[i].r < min_px {
                dropped.push(i);
            }
        }
        agg_kept = tail > 0 && 2.0 * s.circles[cut].r >= min_px;
    }
    let dropped_bytes: u128 = dropped.iter().map(|&i| u128::from(s.kids[i].1)).sum();
    let excluded = all - cut + dropped.len();
    let agg = if excluded > 0 {
        aggregates.push(AggregateRecord {
            record: NO_INDEX,
            parent: dir_rec,
            dir_id: dir,
            count: u32::try_from(excluded).unwrap_or(u32::MAX),
            bytes: u64::try_from(tail + dropped_bytes).unwrap_or(u64::MAX),
        })
    } else {
        NO_INDEX
    };
    let mut d = dropped.iter().peekable();
    for i in 0..cut {
        if d.peek() == Some(&&i) {
            d.next();
            continue;
        }
        let (id, size) = s.kids[i];
        arena.push(Item {
            id,
            size,
            c: s.circles[i],
            agg: NO_INDEX,
        });
    }
    if agg_kept {
        arena.push(Item {
            id: dir,
            size: u64::try_from(tail).unwrap_or(u64::MAX),
            c: s.circles[cut],
            agg,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlaps(a: Circle, b: Circle) -> bool {
        let d = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
        d < a.r + b.r - 1e-6 * (a.r + b.r)
    }

    #[test]
    fn siblings_never_overlap_and_are_enclosed() {
        let mut rng = SplitMix64::new(3);
        for n in [1usize, 2, 3, 4, 7, 50, 400] {
            let mut c: Vec<Circle> = (0..n)
                .map(|_| Circle {
                    r: 0.01 + rng.unit(),
                    ..Circle::default()
                })
                .collect();
            c.sort_by(|a, b| b.r.total_cmp(&a.r));
            let (mut nx, mut pv) = (Vec::new(), Vec::new());
            let e = pack_siblings(&mut c, &mut nx, &mut pv);
            for i in 0..n {
                let d = (c[i].x.powi(2) + c[i].y.powi(2)).sqrt();
                assert!(d + c[i].r <= e * (1.0 + 1e-6) + 1e-9, "n={n} i={i}");
                for j in i + 1..n {
                    assert!(!overlaps(c[i], c[j]), "n={n} {i} {j}");
                }
            }
        }
    }

    #[test]
    fn equal_circles_pack_densely() {
        let mut c = vec![
            Circle {
                r: 1.0,
                ..Circle::default()
            };
            200
        ];
        let e = pack_siblings(&mut c, &mut Vec::new(), &mut Vec::new());
        // 200 unit discs fill ~200 units of area; a decent pack stays well
        // under twice the ideal radius.
        assert!(e < 2.0 * 200f64.sqrt(), "{e}");
    }

    #[test]
    fn enclose_handles_degenerate_bases() {
        let a = Circle {
            x: 0.0,
            y: 0.0,
            r: 1.0,
        };
        assert_eq!(enclose2(a, a), a);
        // Collinear centres make the 3-circle system singular.
        let b = Circle { x: 1.0, ..a };
        let c = Circle { x: 2.0, ..a };
        assert!(enclose3(a, b, c).is_none());
        let e = enclose(&mut [a, b, c], &[a, b, c]);
        assert!((e.x - 1.0).abs() < 1e-9 && (e.r - 2.0).abs() < 1e-9);
    }
}
