//! Cushion treemap coefficients (van Wijk & van de Wetering, 1999), the
//! classic SequoiaView / WinDirStat look.
//!
//! Every nesting level adds a parabolic ridge along both axes of its rect, so
//! a rect's height field is the sum of its own ridge and all its ancestors':
//!
//! ```text
//! z(x, y) = kx2·x² + kx1·x + ky2·y² + ky1·y
//! ```
//!
//! A ridge of height `h` over `[x1, x2]` adds `kx1 += 4h(x1+x2)/(x2−x1)` and
//! `kx2 −= 4h/(x2−x1)` (and the same for `y`), with `h = height·falloffᵈ` at
//! depth `d`. Only the four summed coefficients per rect are needed, so they
//! ship as an optional parallel buffer ([`CushionRecord`], 16 bytes per rect)
//! and the flat style pays nothing.
//!
//! # Shading (fragment shader)
//!
//! With `(x, y)` the fragment's device-pixel center, in the same space as the
//! rect buffer:
//!
//! ```text
//! nx = -(2·kx2·x + kx1)
//! ny = -(2·ky2·y + ky1)
//! cosa = (nx·Lx + ny·Ly + Lz) / sqrt(nx² + ny² + 1)
//! I = Ia + (1 − Ia)·max(0, cosa)        // multiply the base color by I
//! ```
//!
//! WinDirStat's defaults are `L = normalize(−1, −1, 10)` and `Ia = 0.15`.

use crate::buffer::CushionRecord;
use crate::geom::R64;

/// Cushion surface parameters.
///
/// # Example
///
/// ```
/// use strata_layout::{CushionParams, TreemapConfig};
/// let mut cfg = TreemapConfig::new(800.0, 600.0, 1.0);
/// cfg.cushion = Some(CushionParams::default());
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CushionParams {
    /// Ridge height at depth 0.
    pub height: f32,
    /// Per-level multiplier on the ridge height (`0..1`; smaller makes
    /// deeper cushions flatter).
    pub falloff: f32,
}

impl Default for CushionParams {
    /// WinDirStat's defaults.
    fn default() -> Self {
        Self {
            height: 0.38,
            falloff: 0.91,
        }
    }
}

impl CushionParams {
    /// Ridge height at `depth`, sanitized to a finite non-negative value.
    pub(crate) fn ridge_height(&self, depth: u16) -> f64 {
        let h = f64::from(self.height);
        let f = f64::from(self.falloff);
        let v = h * f.powi(i32::from(depth));
        if v.is_finite() && v > 0.0 { v } else { 0.0 }
    }
}

/// Accumulated cushion surface for one rect.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Surface {
    kx2: f64,
    ky2: f64,
    kx1: f64,
    ky1: f64,
}

impl Surface {
    /// Returns this surface plus ridges over `r` of height `h`.
    pub(crate) fn with_ridges(&self, r: &R64, h: f64) -> Self {
        let mut s = *self;
        let w = r.w();
        if w > 0.0 {
            s.kx1 += 4.0 * h * (r.x1 + r.x0) / w;
            s.kx2 -= 4.0 * h / w;
        }
        let hh = r.h();
        if hh > 0.0 {
            s.ky1 += 4.0 * h * (r.y1 + r.y0) / hh;
            s.ky2 -= 4.0 * h / hh;
        }
        s
    }

    pub(crate) fn record(&self) -> CushionRecord {
        let f = |v: f64| {
            let v = v as f32;
            if v.is_finite() { v } else { 0.0 }
        };
        CushionRecord {
            kx2: f(self.kx2),
            ky2: f(self.ky2),
            kx1: f(self.kx1),
            ky1: f(self.ky1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ridge_peaks_at_center() {
        let r = R64 {
            x0: 10.0,
            y0: 0.0,
            x1: 30.0,
            y1: 10.0,
        };
        let s = Surface::default().with_ridges(&r, 1.0);
        // dz/dx = 2·kx2·x + kx1 vanishes at the center.
        let slope = 2.0 * s.kx2 * 20.0 + s.kx1;
        assert!(slope.abs() < 1e-12);
        let z = |x: f64| s.kx2 * x * x + s.kx1 * x;
        assert!(z(20.0) > z(12.0));
        assert!(z(20.0) > z(28.0));
    }

    #[test]
    fn falloff_shrinks_ridges() {
        let p = CushionParams::default();
        assert!(p.ridge_height(3) < p.ridge_height(0));
        let bad = CushionParams {
            height: f32::NAN,
            falloff: 0.5,
        };
        assert_eq!(bad.ridge_height(1), 0.0);
    }
}
