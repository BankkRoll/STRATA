/**
 * Chart math for the lightweight SVG charts (no chart library): linear
 * scales, "nice" axis ticks, SVG path building with gaps, and byte-axis
 * labels. Pure functions so formatting is unit-tested.
 */
import { formatBytes, type SizeUnits } from "./format";

const BINARY_TICK_LABELS = ["B", "KB", "MB", "GB", "TB", "PB", "EB"] as const;
const SI_TICK_LABELS = ["B", "kB", "MB", "GB", "TB", "PB", "EB"] as const;
const TICK_NUMBER = new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 });

/** A linear map from a domain to a range. */
export interface Scale {
  (v: number): number;
  domain: readonly [number, number];
  range: readonly [number, number];
}

/**
 * Builds a linear scale. A zero-width domain maps everything to the range's
 * midpoint instead of dividing by zero.
 *
 * @param domain - Input interval.
 * @param range - Output interval.
 * @returns The scale.
 */
export function linearScale(domain: readonly [number, number], range: readonly [number, number]): Scale {
  const [d0, d1] = domain;
  const [r0, r1] = range;
  const span = d1 - d0;
  const f = ((v: number) => (span === 0 ? (r0 + r1) / 2 : r0 + ((v - d0) / span) * (r1 - r0))) as Scale;
  f.domain = domain;
  f.range = range;
  return f;
}

/**
 * Picks round tick values covering `[min, max]` (1/2/5 × 10^n steps), with
 * the domain widened to the outer ticks so lines never touch the frame.
 *
 * @param min - Data minimum.
 * @param max - Data maximum.
 * @param count - Desired number of intervals (about).
 * @returns Ticks ascending, first ≤ min, last ≥ max.
 * @example
 * niceTicks(0, 97, 4) // [0, 25, 50, 75, 100]
 */
export function niceTicks(min: number, max: number, count = 4): number[] {
  if (!Number.isFinite(min) || !Number.isFinite(max)) return [0];
  if (min === max) {
    if (min === 0) return [0, 1];
    const pad = Math.abs(min) * 0.1;
    min -= pad;
    max += pad;
  }
  const raw = (max - min) / Math.max(1, count);
  const mag = 10 ** Math.floor(Math.log10(raw));
  const norm = raw / mag;
  const step = (norm <= 1 ? 1 : norm <= 2 ? 2 : norm <= 2.5 ? 2.5 : norm <= 5 ? 5 : 10) * mag;
  const start = Math.floor(min / step) * step;
  const end = Math.ceil(max / step) * step;
  const out: number[] = [];
  for (let v = start; v <= end + step / 2; v += step) out.push(Math.round(v / step) * step);
  return out;
}

/**
 * Byte ticks on binary boundaries so labels read "0 B, 256 GB, 512 GB"
 * rather than "0, 274.9 GB".
 *
 * @param min - Data minimum (bytes).
 * @param max - Data maximum (bytes).
 * @param count - Desired intervals.
 * @param units - Unit system.
 * @returns Ticks ascending.
 */
export function byteTicks(min: number, max: number, count = 4, units: SizeUnits = "binary"): number[] {
  const base = units === "binary" ? 1024 : 1000;
  const top = Math.max(Math.abs(min), Math.abs(max), 1);
  const exp = Math.max(0, Math.floor(Math.log(top) / Math.log(base)));
  const unit = base ** exp;
  return niceTicks(min / unit, max / unit, count).map((t) => t * unit);
}

/**
 * Formats a byte tick compactly ("512 GB", "1.5 TB").
 *
 * @param bytes - Tick value.
 * @param units - Unit system.
 * @returns Label.
 */
export function formatByteTick(bytes: number, units: SizeUnits = "binary"): string {
  if (bytes === 0) return "0";
  // Ticks are round numbers, so fraction digits are shown only when needed
  // ("1.5 TB", not "1.50 TB"); formatBytes always shows three digits.
  const labels = units === "binary" ? BINARY_TICK_LABELS : SI_TICK_LABELS;
  const base = units === "binary" ? 1024 : 1000;
  let v = Math.abs(bytes);
  let i = 0;
  while (v >= base && i < labels.length - 1) {
    v /= base;
    i++;
  }
  return `${bytes < 0 ? "−" : ""}${TICK_NUMBER.format(v)} ${labels[i] ?? ""}`;
}

/** A point with an optional value (`null` = gap). */
export interface SeriesPoint {
  x: number;
  y: number | null;
}

/**
 * Builds an SVG path for a series, starting a new subpath after each gap.
 *
 * @param points - Points sorted by x.
 * @param sx - X scale.
 * @param sy - Y scale.
 * @returns The `d` attribute ("" when there is nothing to draw).
 */
export function linePath(points: readonly SeriesPoint[], sx: Scale, sy: Scale): string {
  let d = "";
  let pen = false;
  for (const p of points) {
    if (p.y === null || !Number.isFinite(p.y)) {
      pen = false;
      continue;
    }
    d += `${pen ? "L" : "M"}${sx(p.x).toFixed(1)},${sy(p.y).toFixed(1)}`;
    pen = true;
  }
  return d;
}

/**
 * Index of the point nearest to `x` (for hover and keyboard focus).
 *
 * @param points - Points sorted by x.
 * @param x - Domain value.
 * @returns Index, or -1 when empty.
 */
export function nearestIndex(points: readonly SeriesPoint[], x: number): number {
  let best = -1;
  let dist = Infinity;
  for (let i = 0; i < points.length; i++) {
    const d = Math.abs((points[i] as SeriesPoint).x - x);
    if (d < dist) {
      dist = d;
      best = i;
    }
  }
  return best;
}

/**
 * Formats a signed byte delta with an explicit sign ("+8.20 GB", "−1.00 MB").
 *
 * @param bytes - Delta.
 * @param units - Unit system.
 * @returns Label.
 */
export function formatDelta(bytes: number, units: SizeUnits = "binary"): string {
  if (bytes === 0) return "±0 B";
  const s = formatBytes(Math.abs(bytes), { units });
  return bytes > 0 ? `+${s}` : `−${s}`;
}
