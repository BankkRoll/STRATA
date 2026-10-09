//! Matching two layouts so the frontend can tween between them: animated
//! drill-down / drill-up zooms and live size changes.
//!
//! Records are matched by [`NodeKey`] (entry id + aggregate bit). Entries in
//! both layouts move from their old to their new geometry. For entries
//! present in only one layout, geometry is extrapolated through the *camera*
//! transform between the two layouts, found from the root they share:
//!
//! - drilling into `D`: `D` was a block in the old layout and fills the new
//!   one, so the camera maps old-`D` onto new-`D`. Its siblings fly off
//!   screen along that zoom, and new deep entries grow out of where they
//!   would have been inside old-`D`.
//! - going up is the mirror image.
//! - same root (live updates): the camera is the identity, so new entries
//!   fade in place and removed ones fade out in place.

use std::collections::HashMap;

use crate::buffer::{NO_INDEX, NodeFlags, Record, RecordBuf, rd_f32, rd_u16, rd_u32, wr};
use crate::geom::Rect;
use crate::hierarchy::{Hierarchy, NodeKey};
use crate::rects::RectLayout;

/// What happens to a record during a transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum TransitionKind {
    /// Present in both layouts: tween geometry.
    Stay = 0,
    /// Only in the new layout: fade in while moving.
    Appear = 1,
    /// Only in the old layout: fade out while moving.
    Disappear = 2,
}

impl TransitionKind {
    fn from_u16(v: u16) -> Self {
        match v {
            1 => Self::Appear,
            2 => Self::Disappear,
            _ => Self::Stay,
        }
    }
}

/// One matched pair of record indices. At least one side is `Some`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordPair {
    /// Index in the old layout's main buffer.
    pub old: Option<u32>,
    /// Index in the new layout's main buffer.
    pub new: Option<u32>,
}

impl RecordPair {
    /// The transition kind implied by which sides are present.
    #[must_use]
    pub fn kind(&self) -> TransitionKind {
        match (self.old, self.new) {
            (Some(_), Some(_)) => TransitionKind::Stay,
            (None, _) => TransitionKind::Appear,
            (_, None) => TransitionKind::Disappear,
        }
    }
}

/// Matches records of any two layouts of the same kind by id.
///
/// Output order: disappearing old records (old pre-order), then every new
/// record (new pre-order), so drawing in order keeps the new layout on top.
/// Works for rects, arcs and circles alike; the frontend tweens whatever
/// geometry the record type carries.
///
/// # Example
///
/// ```
/// use strata_layout::{layout_sunburst, match_records, SunburstConfig, TransitionKind, VecTree};
///
/// let mut tree = VecTree::new(0);
/// let a = tree.add_file(VecTree::ROOT, 10, 0);
/// let cfg = SunburstConfig::new(400.0, 400.0, 1.0);
/// let old = layout_sunburst(&tree, VecTree::ROOT, &cfg);
/// tree.add_file(VecTree::ROOT, 10, 0);
/// let new = layout_sunburst(&tree, VecTree::ROOT, &cfg);
/// let pairs = match_records(&old, &new);
/// assert_eq!(pairs.iter().filter(|p| p.kind() == TransitionKind::Appear).count(), 1);
/// ```
#[must_use]
pub fn match_records<A: Hierarchy + ?Sized, B: Hierarchy + ?Sized>(
    old: &A,
    new: &B,
) -> Vec<RecordPair> {
    let new_index = index_by_key(new);
    let old_index = index_by_key(old);
    let mut out = Vec::with_capacity(new.len() + old.len() / 4);
    for i in 0..old.len() {
        if let Some(k) = old.key(i)
            && !new_index.contains_key(&k)
        {
            out.push(RecordPair {
                old: Some(i as u32),
                new: None,
            });
        }
    }
    for j in 0..new.len() {
        let old = new.key(j).and_then(|k| old_index.get(&k).copied());
        out.push(RecordPair {
            old,
            new: Some(j as u32),
        });
    }
    out
}

fn index_by_key<H: Hierarchy + ?Sized>(h: &H) -> HashMap<NodeKey, u32> {
    let mut m = HashMap::with_capacity(h.len());
    for i in 0..h.len() {
        if let Some(k) = h.key(i) {
            m.insert(k, i as u32);
        }
    }
    m
}

/// One tween instance (48 bytes).
///
/// Layout: `from.{x,y,w,h}:f32@0..16 to.{x,y,w,h}:f32@16..32 id:u32@32
/// old_index:u32@36 new_index:u32@40 kind:u16@44 flags:u16@46`. Missing
/// indices are [`NO_INDEX`]; `flags` are the new record's (old's when
/// disappearing).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransitionRecord {
    /// Geometry at t = 0.
    pub from: Rect,
    /// Geometry at t = 1.
    pub to: Rect,
    /// Entry id (directory id for aggregates).
    pub id: u32,
    /// Index in the old rect buffer, or [`NO_INDEX`].
    pub old_index: u32,
    /// Index in the new rect buffer, or [`NO_INDEX`].
    pub new_index: u32,
    /// Stay / appear / disappear.
    pub kind: TransitionKind,
    /// Record flags.
    pub flags: NodeFlags,
}

impl Record for TransitionRecord {
    const STRIDE: usize = 48;
    fn write(&self, out: &mut [u8]) {
        for (k, v) in [
            self.from.x,
            self.from.y,
            self.from.w,
            self.from.h,
            self.to.x,
            self.to.y,
            self.to.w,
            self.to.h,
        ]
        .into_iter()
        .enumerate()
        {
            wr(out, k * 4, &v.to_le_bytes());
        }
        wr(out, 32, &self.id.to_le_bytes());
        wr(out, 36, &self.old_index.to_le_bytes());
        wr(out, 40, &self.new_index.to_le_bytes());
        wr(out, 44, &(self.kind as u16).to_le_bytes());
        wr(out, 46, &self.flags.bits().to_le_bytes());
    }
    fn read(b: &[u8]) -> Self {
        let f = |o| rd_f32(b, o);
        Self {
            from: Rect::new(f(0), f(4), f(8), f(12)),
            to: Rect::new(f(16), f(20), f(24), f(28)),
            id: rd_u32(b, 32),
            old_index: rd_u32(b, 36),
            new_index: rd_u32(b, 40),
            kind: TransitionKind::from_u16(rd_u16(b, 44)),
            flags: NodeFlags::from_bits(rd_u16(b, 46)),
        }
    }
}

/// Axis-aligned affine map `p ↦ p·s + t`, per axis.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Camera {
    sx: f64,
    sy: f64,
    tx: f64,
    ty: f64,
}

impl Camera {
    const IDENTITY: Self = Self {
        sx: 1.0,
        sy: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    /// Map taking rect `a` onto rect `b`, if `a` is non-degenerate.
    fn between(a: Rect, b: Rect) -> Option<Self> {
        if !(a.w > 0.0 && a.h > 0.0 && b.is_valid()) {
            return None;
        }
        let sx = f64::from(b.w) / f64::from(a.w);
        let sy = f64::from(b.h) / f64::from(a.h);
        Some(Self {
            sx,
            sy,
            tx: f64::from(b.x) - f64::from(a.x) * sx,
            ty: f64::from(b.y) - f64::from(a.y) * sy,
        })
    }

    fn inverse(&self) -> Self {
        Self {
            sx: 1.0 / self.sx,
            sy: 1.0 / self.sy,
            tx: -self.tx / self.sx,
            ty: -self.ty / self.sy,
        }
    }

    fn apply(&self, r: Rect) -> Rect {
        let out = Rect::new(
            (f64::from(r.x) * self.sx + self.tx) as f32,
            (f64::from(r.y) * self.sy + self.ty) as f32,
            (f64::from(r.w) * self.sx) as f32,
            (f64::from(r.h) * self.sy) as f32,
        );
        if out.is_valid() { out } else { r }
    }
}

/// Builds the tween buffer between two rect layouts (treemap or icicle).
///
/// # Example
///
/// ```
/// use strata_layout::{layout_treemap, transition_rects, TransitionKind, TreemapConfig, VecTree};
///
/// let mut tree = VecTree::new(0);
/// let d = tree.add_dir(VecTree::ROOT, 0);
/// tree.add_file(d, 100, 0);
/// tree.add_file(VecTree::ROOT, 100, 0);
/// let cfg = TreemapConfig::new(800.0, 600.0, 1.0);
/// let before = layout_treemap(&tree, VecTree::ROOT, &cfg);
/// let after = layout_treemap(&tree, d, &cfg); // drill into `d`
/// let tw = transition_rects(&before, &after);
/// let d_tween = tw.iter().find(|t| t.id == d).unwrap();
/// assert_eq!(d_tween.kind, TransitionKind::Stay);
/// assert_eq!(d_tween.to.w, 800.0);
/// ```
#[must_use]
pub fn transition_rects(old: &RectLayout, new: &RectLayout) -> RecordBuf<TransitionRecord> {
    let camera = camera(old, new);
    let inverse = camera.inverse();
    let mut out = RecordBuf::new();
    let rect_of = |l: &RectLayout, i: Option<u32>| i.and_then(|i| l.rects().get(i as usize));
    for pair in match_records(old, new) {
        let o = rect_of(old, pair.old);
        let n = rect_of(new, pair.new);
        let (from, to, id, flags) = match (o, n) {
            (Some(o), Some(n)) => (o.rect(), n.rect(), n.id, n.flags),
            (None, Some(n)) => (inverse.apply(n.rect()), n.rect(), n.id, n.flags),
            (Some(o), None) => (o.rect(), camera.apply(o.rect()), o.id, o.flags),
            (None, None) => continue,
        };
        out.push(TransitionRecord {
            from,
            to,
            id,
            old_index: pair.old.unwrap_or(NO_INDEX),
            new_index: pair.new.unwrap_or(NO_INDEX),
            kind: pair.kind(),
            flags,
        });
    }
    out
}

/// Camera from old to new: anchored on the new root inside the old layout,
/// else the old root inside the new layout, else identity.
fn camera(old: &RectLayout, new: &RectLayout) -> Camera {
    let (Some(old_root), Some(new_root)) = (old.rects().get(0), new.rects().get(0)) else {
        return Camera::IDENTITY;
    };
    let find = |l: &RectLayout, id: u32| {
        l.rects()
            .iter()
            .find(|r| r.id == id && !r.flags.contains(NodeFlags::AGGREGATE))
    };
    if old_root.id == new_root.id {
        return Camera::between(old_root.rect(), new_root.rect()).unwrap_or(Camera::IDENTITY);
    }
    if let Some(r) = find(old, new_root.id) {
        return Camera::between(r.rect(), new_root.rect()).unwrap_or(Camera::IDENTITY);
    }
    if let Some(r) = find(new, old_root.id) {
        return Camera::between(old_root.rect(), r.rect()).unwrap_or(Camera::IDENTITY);
    }
    Camera::IDENTITY
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::VecTree;
    use crate::treemap::{TreemapConfig, layout_treemap};

    fn tree() -> (VecTree, u32, u32, u32) {
        let mut t = VecTree::new(0);
        let d = t.add_dir(VecTree::ROOT, 0);
        let inner = t.add_dir(d, 0);
        let deep = t.add_file(inner, 300, 0);
        t.add_file(d, 100, 0);
        t.add_file(VecTree::ROOT, 400, 0);
        (t, d, inner, deep)
    }

    #[test]
    fn drill_down_zooms_siblings_away_and_grows_new_entries() {
        let (t, d, _, _) = tree();
        let mut cfg = TreemapConfig::new(800.0, 600.0, 1.0);
        cfg.max_depth = 1;
        let before = layout_treemap(&t, VecTree::ROOT, &cfg);
        let after = layout_treemap(&t, d, &cfg);
        let tw: Vec<_> = transition_rects(&before, &after).iter().collect();

        let d_old = before.rects().iter().find(|r| r.id == d).unwrap();
        let root_t = tw.iter().find(|t| t.id == VecTree::ROOT).unwrap();
        assert_eq!(root_t.kind, TransitionKind::Disappear);
        // The old root grows by the same factor `d` grows to fill the view.
        let zoom = 800.0 / d_old.w;
        assert!((root_t.to.w - 800.0 * zoom).abs() < 1e-2);

        for t in tw.iter().filter(|t| t.kind == TransitionKind::Appear) {
            // Appearing children start inside `d`'s old block.
            assert!(t.from.x >= d_old.x - 1e-3 && t.from.x + t.from.w <= d_old.x + d_old.w + 1e-3);
            assert!(t.from.w < t.to.w);
        }
    }

    #[test]
    fn identical_layouts_are_all_stay() {
        let (t, ..) = tree();
        let cfg = TreemapConfig::new(640.0, 480.0, 1.0);
        let a = layout_treemap(&t, VecTree::ROOT, &cfg);
        let tw = transition_rects(&a, &a);
        assert_eq!(tw.len(), a.rects().len());
        for t in tw.iter() {
            assert_eq!(t.kind, TransitionKind::Stay);
            assert_eq!(t.from, t.to);
        }
    }

    #[test]
    fn live_growth_fades_in_place() {
        let (mut t, d, ..) = tree();
        let cfg = TreemapConfig::new(640.0, 480.0, 1.0);
        let a = layout_treemap(&t, VecTree::ROOT, &cfg);
        let f = t.add_file(d, 500, 0);
        let b = layout_treemap(&t, VecTree::ROOT, &cfg);
        let tw = transition_rects(&a, &b);
        let n = tw.iter().find(|t| t.id == f).unwrap();
        assert_eq!(n.kind, TransitionKind::Appear);
        assert_eq!(n.from, n.to);
        assert_eq!(n.old_index, NO_INDEX);
    }

    #[test]
    fn record_round_trips() {
        let r = TransitionRecord {
            from: Rect::new(1.0, 2.0, 3.0, 4.0),
            to: Rect::new(5.0, 6.0, 7.0, 8.0),
            id: 9,
            old_index: NO_INDEX,
            new_index: 3,
            kind: TransitionKind::Appear,
            flags: NodeFlags::DIR,
        };
        let mut b = RecordBuf::new();
        b.push(r);
        assert_eq!(b.as_bytes().len(), 48);
        assert_eq!(&b.as_bytes()[44..46], &[1, 0]);
        assert_eq!(b.get(0), Some(r));
    }

    #[test]
    fn empty_layouts_produce_nothing() {
        let e = RectLayout::new();
        assert!(transition_rects(&e, &e).is_empty());
    }
}
