/**
 * Canvas2D label overlay for the rect and circle views.
 *
 * Label candidates come from the frame's label records (only blocks big
 * enough for text). Names come from the {@link EntryInfoProvider}; missing
 * names are requested in one batch and drawn when they arrive. Each label is
 * ellipsized to its box ({@link ellipsize}) and dropped if it would overlap
 * one already placed ({@link CollisionGrid}); pre-order means parents are
 * placed before their children, so folder headers win.
 */
import type { EntryInfoProvider } from "../lib/entries";
import { formatBytes, type SizeUnits } from "../lib/format";
import { NodeFlag, type LayoutFrame, type ViewTransform } from "../lib/layout/frame";
import { rgbFor, type ColorMode } from "../lib/palette";

/** Measures text width in the current font. */
export type Measure = (text: string) => number;

/**
 * Shortens `text` with a trailing ellipsis so it fits `maxWidth`, using a
 * binary search over prefix lengths (O(log n) measurements).
 *
 * @param text - Full text.
 * @param maxWidth - Available width.
 * @param measure - Width of a string in the target font.
 * @returns The fitting text, or `""` when not even "…" fits.
 */
export function ellipsize(text: string, maxWidth: number, measure: Measure): string {
  if (measure(text) <= maxWidth) return text;
  const ell = "…";
  if (measure(ell) > maxWidth) return "";
  // Work on code points so surrogate pairs (emoji names) are never split.
  const chars = Array.from(text);
  let lo = 0;
  let hi = chars.length;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (measure(chars.slice(0, mid).join("") + ell) <= maxWidth) lo = mid;
    else hi = mid - 1;
  }
  return lo === 0 ? ell : chars.slice(0, lo).join("").trimEnd() + ell;
}

/** Axis-aligned box. */
interface Rect4 {
  x: number;
  y: number;
  w: number;
  h: number;
}

/**
 * Uniform-grid occupancy test for placed labels. Insert and query cost
 * O(cells touched), independent of how many labels are placed.
 */
export class CollisionGrid {
  private readonly cells = new Map<number, Rect4[]>();

  /** @param cell - Cell size in pixels. */
  constructor(private readonly cell = 64) {}

  private keys(r: Rect4): number[] {
    const c = this.cell;
    const out: number[] = [];
    const x0 = Math.floor(r.x / c);
    const y0 = Math.floor(r.y / c);
    const x1 = Math.floor((r.x + r.w) / c);
    const y1 = Math.floor((r.y + r.h) / c);
    for (let y = y0; y <= y1; y++) for (let x = x0; x <= x1; x++) out.push(y * 65536 + x);
    return out;
  }

  /** Places `r` unless it overlaps a placed box; returns whether it was placed. */
  tryPlace(r: Rect4): boolean {
    const keys = this.keys(r);
    for (const k of keys) {
      for (const o of this.cells.get(k) ?? []) {
        if (r.x < o.x + o.w && o.x < r.x + r.w && r.y < o.y + o.h && o.y < r.y + r.h) return false;
      }
    }
    for (const k of keys) {
      let list = this.cells.get(k);
      if (!list) {
        list = [];
        this.cells.set(k, list);
      }
      list.push(r);
    }
    return true;
  }
}

/** Inputs of one overlay draw. */
export interface LabelDrawParams {
  frame: LayoutFrame;
  view: ViewTransform;
  width: number;
  height: number;
  dpr: number;
  dark: boolean;
  colorMode: ColorMode;
  units: SizeUnits;
  entries: EntryInfoProvider;
}

/** Relative luminance (sRGB, 0–255 input) for picking label ink. */
function luminance(r: number, g: number, b: number): number {
  const lin = (c: number) => {
    const s = c / 255;
    return s <= 0.04045 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}

/** Draws labels into a 2D canvas sized in device pixels. */
export class LabelOverlay {
  private readonly widths = new Map<string, number>();
  private font = "";

  /** @param ctx - The overlay canvas context. */
  constructor(private readonly ctx: CanvasRenderingContext2D) {}

  /** Clears the overlay (during animations). */
  clear(): void {
    const c = this.ctx.canvas;
    this.ctx.clearRect(0, 0, c.width, c.height);
  }

  /**
   * Draws every label that fits and does not collide.
   *
   * @param p - Frame, transform and display settings.
   * @returns Number of labels drawn.
   */
  draw(p: LabelDrawParams): number {
    const ctx = this.ctx;
    this.clear();
    const px = Math.round(12 * p.dpr);
    const font = `${px}px "Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif`;
    if (font !== this.font) {
      this.font = font;
      this.widths.clear();
    }
    ctx.font = font;
    ctx.textBaseline = "middle";
    const measure: Measure = (t) => {
      let w = this.widths.get(t);
      if (w === undefined) {
        w = ctx.measureText(t).width;
        if (this.widths.size > 20_000) this.widths.clear();
        this.widths.set(t, w);
      }
      return w;
    };
    const grid = new CollisionGrid(64 * p.dpr);
    const { scale, tx, ty } = p.view;
    const pad = 3 * p.dpr;
    const missing: number[] = [];
    let drawn = 0;
    for (let i = 0; i < p.frame.labels.count; i++) {
      const l = p.frame.labels.get(i);
      const x = l.x * scale + tx;
      const y = l.y * scale + ty;
      let w = l.w * scale;
      const h = Math.min(l.h * scale, px * 1.6);
      if (w < 24 * p.dpr || h < px || x > p.width || y > p.height || x + w < 0 || y + h < 0) continue;
      const flags = p.frame.nodes.flags(l.record);
      const isAgg = (flags & NodeFlag.AGGREGATE) !== 0;
      let name: string;
      if (isAgg) {
        const a = p.frame.aggregates.forRecord(l.record);
        name = `${a ? a.count.toLocaleString() : "Small"} small items`;
      } else {
        const info = p.entries.get(l.id);
        if (!info) {
          missing.push(l.id);
          continue;
        }
        name = info.name;
      }
      const size = formatBytes(l.size, { units: p.units });
      const sizeW = measure(size);
      // Clip the box to the canvas so labels of huge zoomed blocks stay visible.
      const cx = Math.max(x, 0);
      w -= cx - x;
      w = Math.min(w, p.width - cx);
      const showSize = w > sizeW + 48 * p.dpr;
      const nameText = ellipsize(name, w - pad * 2 - (showSize ? sizeW + pad * 2 : 0), measure);
      if (!nameText) continue;
      const box = { x: cx, y, w: Math.min(w, measure(nameText) + pad * 2 + (showSize ? sizeW + pad * 2 : 0)), h };
      if (!grid.tryPlace(box)) continue;
      const [r, g, b] = rgbFor(p.colorMode, p.frame.nodes.colorKey(l.record), p.dark);
      const isDir = (flags & NodeFlag.DIR) !== 0;
      // Directory headers sit on the background-tinted frame color.
      const lum = isDir ? (p.dark ? 0.02 : 0.75) : luminance(r, g, b);
      ctx.fillStyle = lum > 0.35 ? "rgba(0,0,0,0.88)" : "rgba(255,255,255,0.95)";
      ctx.fillText(nameText, cx + pad, y + h / 2);
      if (showSize) {
        ctx.globalAlpha = 0.75;
        ctx.fillText(size, cx + w - pad - sizeW, y + h / 2);
        ctx.globalAlpha = 1;
      }
      drawn++;
    }
    if (missing.length > 0) void p.entries.load(missing);
    return drawn;
  }
}
