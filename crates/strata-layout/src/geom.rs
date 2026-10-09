//! Geometry primitives: the public [`Rect`] and [`ViewTransform`], plus the
//! internal edge-based `f64` rectangle every layout computes in.
//!
//! Layouts work in `f64` and convert to `f32` only when writing records. Deep
//! visual zoom puts the layout root at coordinates in the millions, where
//! `f32` would lose whole pixels.

/// Axis-aligned rectangle in device pixels (`x`, `y` is the top-left corner).
///
/// # Example
///
/// ```
/// use strata_layout::Rect;
/// let r = Rect::new(10.0, 20.0, 30.0, 40.0);
/// assert!(r.contains(10.0, 20.0));
/// assert!(!r.contains(40.0, 20.0)); // right edge is exclusive
/// assert_eq!(r.area(), 1200.0);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width (never negative in layout output).
    pub w: f32,
    /// Height (never negative in layout output).
    pub h: f32,
}

impl Rect {
    /// Creates a rectangle from its top-left corner and size.
    #[must_use]
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// Area in square pixels.
    #[must_use]
    pub fn area(&self) -> f32 {
        self.w * self.h
    }

    /// Half-open containment: left/top edges inclusive, right/bottom
    /// exclusive, so a point on a shared sibling edge belongs to exactly one.
    #[must_use]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }

    /// Whether every component is finite and the size is non-negative.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.w.is_finite()
            && self.h.is_finite()
            && self.w >= 0.0
            && self.h >= 0.0
    }
}

/// Visual-zoom transform from layout space to screen space:
/// `screen = layout * scale + (tx, ty)`.
///
/// The frontend zooms visually on the GPU while the wheel moves, then asks
/// for a relayout with the same transform so deeper levels appear. Layout is
/// computed directly in screen space, which keeps LOD thresholds in real
/// device pixels and lets off-screen subtrees be skipped.
///
/// # Example
///
/// ```
/// use strata_layout::ViewTransform;
/// // Zoom 4x around the screen point (100, 50).
/// let v = ViewTransform::zoom_about(4.0, 100.0, 50.0);
/// assert_eq!(v.apply(100.0, 50.0), (100.0, 50.0));
/// assert_eq!(v.apply(101.0, 50.0), (104.0, 50.0));
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewTransform {
    /// Uniform scale factor (`1.0` = no zoom). Non-finite or non-positive
    /// values are treated as `1.0`.
    pub scale: f64,
    /// Horizontal translation in device pixels.
    pub tx: f64,
    /// Vertical translation in device pixels.
    pub ty: f64,
}

impl Default for ViewTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl ViewTransform {
    /// No zoom, no pan.
    pub const IDENTITY: Self = Self {
        scale: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    /// Zoom by `scale` keeping the screen point (`cx`, `cy`) fixed.
    #[must_use]
    pub fn zoom_about(scale: f64, cx: f64, cy: f64) -> Self {
        Self {
            scale,
            tx: cx - cx * scale,
            ty: cy - cy * scale,
        }
    }

    /// Maps a layout-space point to screen space.
    #[must_use]
    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        let s = self.sanitized();
        (x * s.scale + s.tx, y * s.scale + s.ty)
    }

    /// Replaces non-finite components so a bad transform from the UI can
    /// never produce NaN geometry.
    pub(crate) fn sanitized(&self) -> Self {
        let scale = if self.scale.is_finite() && self.scale > 0.0 {
            self.scale
        } else {
            1.0
        };
        Self {
            scale,
            tx: if self.tx.is_finite() { self.tx } else { 0.0 },
            ty: if self.ty.is_finite() { self.ty } else { 0.0 },
        }
    }

    pub(crate) fn apply_rect(&self, r: Rect) -> R64 {
        let r = R64::from_rect(r);
        let (x0, y0) = self.apply(r.x0, r.y0);
        let (x1, y1) = self.apply(r.x1, r.y1);
        R64 { x0, y0, x1, y1 }
    }
}

/// Edge-based `f64` rectangle. Siblings computed from a shared cursor share
/// edges exactly, so converting edges (not widths) to `f32` keeps neighbours
/// gap- and overlap-free after rounding.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct R64 {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl R64 {
    pub(crate) const EMPTY: Self = Self {
        x0: 0.0,
        y0: 0.0,
        x1: 0.0,
        y1: 0.0,
    };

    /// Converts a public rect, mapping NaN/inf/negative sizes to empty.
    pub(crate) fn from_rect(r: Rect) -> Self {
        if !r.is_valid() {
            return Self::EMPTY;
        }
        let (x, y) = (f64::from(r.x), f64::from(r.y));
        Self {
            x0: x,
            y0: y,
            x1: x + f64::from(r.w),
            y1: y + f64::from(r.h),
        }
    }

    pub(crate) fn w(&self) -> f64 {
        self.x1 - self.x0
    }

    pub(crate) fn h(&self) -> f64 {
        self.y1 - self.y0
    }

    pub(crate) fn area(&self) -> f64 {
        self.w().max(0.0) * self.h().max(0.0)
    }

    pub(crate) fn is_finite(&self) -> bool {
        self.x0.is_finite() && self.y0.is_finite() && self.x1.is_finite() && self.y1.is_finite()
    }

    /// Intersection, or `None` when it has no area.
    pub(crate) fn intersect(&self, o: &Self) -> Option<Self> {
        let r = Self {
            x0: self.x0.max(o.x0),
            y0: self.y0.max(o.y0),
            x1: self.x1.min(o.x1),
            y1: self.y1.min(o.y1),
        };
        (r.x1 > r.x0 && r.y1 > r.y0).then_some(r)
    }

    /// Shrinks by `l`, `t`, `r`, `b`; collapses to zero size instead of
    /// inverting when the insets exceed the size.
    pub(crate) fn inset(&self, l: f64, t: f64, r: f64, b: f64) -> Self {
        let x0 = self.x0 + l;
        let y0 = self.y0 + t;
        Self {
            x0,
            y0,
            x1: (self.x1 - r).max(x0),
            y1: (self.y1 - b).max(y0),
        }
    }

    /// Converts edges to `f32` and derives the size from the rounded edges.
    pub(crate) fn to_rect(self) -> Rect {
        let x0 = self.x0 as f32;
        let y0 = self.y0 as f32;
        let x1 = self.x1 as f32;
        let y1 = self.y1 as f32;
        Rect {
            x: x0,
            y: y0,
            w: (x1 - x0).max(0.0),
            h: (y1 - y0).max(0.0),
        }
    }
}

/// Clamps a user-provided length to a finite, non-negative value.
pub(crate) fn sane_len(v: f32) -> f64 {
    if v.is_finite() && v > 0.0 {
        f64::from(v)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_rects_become_empty() {
        assert_eq!(
            R64::from_rect(Rect::new(f32::NAN, 0.0, 1.0, 1.0)),
            R64::EMPTY
        );
        assert_eq!(R64::from_rect(Rect::new(0.0, 0.0, -1.0, 1.0)), R64::EMPTY);
    }

    #[test]
    fn inset_never_inverts() {
        let r = R64 {
            x0: 0.0,
            y0: 0.0,
            x1: 4.0,
            y1: 4.0,
        };
        let i = r.inset(3.0, 3.0, 3.0, 3.0);
        assert_eq!(i.w(), 0.0);
        assert_eq!(i.h(), 0.0);
    }

    #[test]
    fn bad_transform_is_identity() {
        let v = ViewTransform {
            scale: f64::NAN,
            tx: f64::INFINITY,
            ty: 1.0,
        };
        assert_eq!(v.apply(2.0, 3.0), (2.0, 4.0));
    }

    #[test]
    fn shared_edges_survive_f32_rounding() {
        let a = R64 {
            x0: 0.1,
            y0: 0.0,
            x1: 1_234.567_891,
            y1: 1.0,
        };
        let b = R64 {
            x0: a.x1,
            y0: 0.0,
            x1: 2000.0,
            y1: 1.0,
        };
        let (ra, rb) = (a.to_rect(), b.to_rect());
        assert_eq!(rb.x, a.x1 as f32);
        assert!((ra.x + ra.w - rb.x).abs() <= f32::EPSILON * 2048.0);
    }
}
