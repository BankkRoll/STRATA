//! The squarified treemap subdivision (Bruls, Huizing & van Wijk, 2000).
//!
//! # Algorithm
//!
//! Input: weights sorted descending and a rectangle. Weights are scaled so
//! they sum to the rectangle's area.
//!
//! 1. Let `l` be the shorter side of the remaining rectangle.
//! 2. Grow a *row* greedily: keep adding the next weight while doing so does
//!    not worsen the row's worst aspect ratio,
//!    `max(l²·max/s², s²/(l²·min))` where `s` is the row sum. Because input
//!    is sorted, `max` is the row's first weight and `min` its last, so each
//!    step is O(1).
//! 3. Lay the row out as a strip of thickness `s / l` along the shorter
//!    side, and cut that strip off the remaining rectangle.
//! 4. Repeat until every weight is placed.
//!
//! Each weight is visited a bounded number of times (once when accepted,
//! once when it ends a row), so the whole pass is **O(n)** after the
//! O(n log n) sort done by the caller.
//!
//! Positions come from running cursors, and the last item of every row (and
//! the last row) snaps to the remaining edge, so siblings tile the input
//! exactly with no accumulated floating-point gap.

use crate::geom::R64;

/// Lays out `weights` (positive, sorted descending) inside `rect`, pushing
/// one rectangle per weight onto `out` in input order.
///
/// Weights need not sum to the rect's area; they are rescaled. An empty or
/// zero-area `rect` yields degenerate rects at its origin so callers always
/// get one output per input.
pub(crate) fn squarify(weights: &[f64], rect: R64, out: &mut Vec<R64>) {
    let total: f64 = weights.iter().sum();
    let area = rect.area();
    if weights.is_empty() {
        return;
    }
    if !(total > 0.0 && area > 0.0 && total.is_finite() && area.is_finite()) {
        let p = R64 {
            x0: rect.x0,
            y0: rect.y0,
            x1: rect.x0,
            y1: rect.y0,
        };
        out.extend(std::iter::repeat_n(p, weights.len()));
        return;
    }
    let k = area / total;
    let mut r = rect;
    let n = weights.len();
    let mut i = 0;
    while i < n {
        let w = r.w();
        let h = r.h();
        let side = w.min(h);
        if i + 1 == n || side <= 0.0 {
            // Last item, or the remainder collapsed: give everything left the
            // remaining rectangle so nothing is lost.
            out.extend(std::iter::repeat_n(r, n - i));
            break;
        }
        let side2 = side * side;
        let head = weights[i] * k;
        let mut sum = 0.0;
        let mut best = f64::INFINITY;
        let mut j = i;
        while j < n {
            let a = weights[j] * k;
            let s = sum + a;
            let s2 = s * s;
            let worst = (side2 * head / s2).max(s2 / (side2 * a));
            if j > i && worst > best {
                break;
            }
            best = worst;
            sum = s;
            j += 1;
        }
        let last_row = j == n;
        if w >= h {
            // Strip along the left edge, items stacked top to bottom.
            let x1 = if last_row {
                r.x1
            } else {
                (r.x0 + sum / h).min(r.x1)
            };
            let mut y = r.y0;
            for (t, &wt) in weights[i..j].iter().enumerate() {
                let y1 = if i + t + 1 == j {
                    r.y1
                } else {
                    (y + wt * k / sum * h).min(r.y1)
                };
                out.push(R64 {
                    x0: r.x0,
                    y0: y,
                    x1,
                    y1,
                });
                y = y1;
            }
            r.x0 = x1;
        } else {
            // Strip along the top edge, items left to right.
            let y1 = if last_row {
                r.y1
            } else {
                (r.y0 + sum / w).min(r.y1)
            };
            let mut x = r.x0;
            for (t, &wt) in weights[i..j].iter().enumerate() {
                let x1 = if i + t + 1 == j {
                    r.x1
                } else {
                    (x + wt * k / sum * w).min(r.x1)
                };
                out.push(R64 {
                    x0: x,
                    y0: r.y0,
                    x1,
                    y1,
                });
                x = x1;
            }
            r.y0 = y1;
        }
        i = j;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: f64, h: f64) -> R64 {
        R64 {
            x0: 0.0,
            y0: 0.0,
            x1: w,
            y1: h,
        }
    }

    #[test]
    fn paper_example_tiles_exactly() {
        // The 6x4 example from the paper.
        let weights = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0];
        let mut out = Vec::new();
        squarify(&weights, rect(6.0, 4.0), &mut out);
        assert_eq!(out.len(), 7);
        let total: f64 = out.iter().map(R64::area).sum();
        assert!((total - 24.0).abs() < 1e-9);
        for (r, w) in out.iter().zip(weights) {
            assert!((r.area() - w).abs() < 1e-9, "{r:?} vs {w}");
        }
        // The first row holds the two 6s stacked on the left: 3 wide, 2 tall.
        assert!((out[0].w() - 3.0).abs() < 1e-9 && (out[0].h() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn single_item_fills_rect() {
        let mut out = Vec::new();
        squarify(&[5.0], rect(10.0, 3.0), &mut out);
        assert_eq!(out, vec![rect(10.0, 3.0)]);
    }

    #[test]
    fn zero_area_rect_yields_degenerate_rects() {
        let mut out = Vec::new();
        squarify(&[1.0, 2.0], rect(0.0, 10.0), &mut out);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|r| r.area() == 0.0));
    }

    #[test]
    fn extreme_aspect_ratio_still_tiles() {
        let weights: Vec<f64> = (1..=50).rev().map(f64::from).collect();
        let mut out = Vec::new();
        squarify(&weights, rect(100_000.0, 1.0), &mut out);
        let total: f64 = out.iter().map(R64::area).sum();
        assert!((total - 100_000.0).abs() < 1e-6);
        assert!(out.iter().all(R64::is_finite));
    }
}
