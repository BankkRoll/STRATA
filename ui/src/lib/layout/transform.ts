/**
 * Visual-zoom transform math (`screen = layout * scale + t`).
 *
 * The view keeps a *desired* absolute transform `A`. A frame was laid out
 * under its own transform `F` (its coordinates are already in screen space),
 * so the GPU draws it with the relative transform `R = A ∘ F⁻¹`. When a new
 * frame computed under `A` arrives, `R` becomes the identity.
 */
import type { ViewTransform } from "./frame";

/** Largest visual zoom factor. */
export const MAX_ZOOM = 4096;

/**
 * Relative transform taking frame coordinates (laid out under `frame`) to the
 * screen under `desired`.
 *
 * @param desired - Absolute transform the user is looking at.
 * @param frame - Absolute transform the frame was computed with.
 * @returns `desired ∘ frame⁻¹`.
 */
export function relativeTransform(desired: ViewTransform, frame: ViewTransform): ViewTransform {
  const s = desired.scale / frame.scale;
  return { scale: s, tx: desired.tx - frame.tx * s, ty: desired.ty - frame.ty * s };
}

/**
 * Maps a screen point back through `t`.
 *
 * @param t - Transform.
 * @param x - Screen x.
 * @param y - Screen y.
 * @returns The pre-transform point.
 */
export function invertPoint(t: ViewTransform, x: number, y: number): [number, number] {
  return [(x - t.tx) / t.scale, (y - t.ty) / t.scale];
}

/**
 * Zooms `t` by `factor` keeping screen point (`cx`, `cy`) fixed, then clamps
 * so the root always covers the canvas (no zooming out past 1×, no panning
 * past the edges).
 *
 * @param t - Current absolute transform.
 * @param factor - Multiplicative zoom (> 1 zooms in).
 * @param cx - Screen anchor x (device px).
 * @param cy - Screen anchor y (device px).
 * @param width - Canvas width (device px).
 * @param height - Canvas height (device px).
 * @returns The new transform.
 */
export function zoomAbout(
  t: ViewTransform,
  factor: number,
  cx: number,
  cy: number,
  width: number,
  height: number,
): ViewTransform {
  const scale = Math.min(MAX_ZOOM, Math.max(1, t.scale * factor));
  const k = scale / t.scale;
  return clampTransform({ scale, tx: cx - (cx - t.tx) * k, ty: cy - (cy - t.ty) * k }, width, height);
}

/**
 * Pans `t` by (`dx`, `dy`) device pixels, clamped.
 *
 * @returns The new transform.
 */
export function panBy(t: ViewTransform, dx: number, dy: number, width: number, height: number): ViewTransform {
  return clampTransform({ scale: t.scale, tx: t.tx + dx, ty: t.ty + dy }, width, height);
}

/**
 * Keeps the zoomed root covering the `width × height` canvas.
 *
 * @returns The clamped transform (identity translation at scale 1).
 */
export function clampTransform(t: ViewTransform, width: number, height: number): ViewTransform {
  const scale = Number.isFinite(t.scale) ? Math.min(MAX_ZOOM, Math.max(1, t.scale)) : 1;
  const clamp = (v: number, size: number) => Math.min(0, Math.max(size - size * scale, Number.isFinite(v) ? v : 0));
  return { scale, tx: clamp(t.tx, width), ty: clamp(t.ty, height) };
}

/** Whether two transforms are equal within a tiny epsilon. */
export function sameTransform(a: ViewTransform, b: ViewTransform): boolean {
  return Math.abs(a.scale - b.scale) < 1e-9 && Math.abs(a.tx - b.tx) < 1e-6 && Math.abs(a.ty - b.ty) < 1e-6;
}
