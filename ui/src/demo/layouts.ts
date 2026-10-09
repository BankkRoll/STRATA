/**
 * The treemap layout and its drill transitions, ported from `strata-layout`
 * (`treemap.rs`, `squarify.rs`, `cushion.rs`, `transition.rs`) so the website
 * demo can lay out any folder of the sample tree at the visitor's canvas size.
 *
 * The app gets these frames from the Rust engine; the demo has no engine, and
 * replaying the fixed 960×640 fixture frames would stretch or letterbox them.
 * The port follows the Rust code step for step, including its `f32`
 * rounding, and `treemap.test.ts` checks it byte for byte against the frames
 * the Rust exporter wrote (`src/lib/layout/__fixtures__`), so a change on
 * either side fails the test instead of drifting.
 *
 * Responsibilities:
 * - {@link layoutTreemap}: nested squarified treemap with headers, padding,
 *   LOD aggregation, viewport culling and cushion coefficients.
 * - {@link transitionRects}: the tween records between two layouts.
 * - {@link encodeRectFrame}: the layout frame container the UI decodes.
 */
import { FRAME_HEADER_BYTES, FRAME_MAGIC, FRAME_VERSION, NO_INDEX, NodeFlag, TransitionKind, type ViewKind, type ViewTransform } from "../lib/layout/frame";

// -----------------------------------------------------------------------------
// Geometry
// -----------------------------------------------------------------------------

/** Edge-based `f64` rectangle (`geom.rs` `R64`). */
interface R64 {
  x0: number;
  y0: number;
  x1: number;
  y1: number;
}

/** `f32` rectangle as stored in records. */
interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

const f32 = Math.fround;
const width = (r: R64) => r.x1 - r.x0;
const height = (r: R64) => r.y1 - r.y0;
const area = (r: R64) => Math.max(width(r), 0) * Math.max(height(r), 0);

function intersect(a: R64, b: R64): R64 | null {
  const r = { x0: Math.max(a.x0, b.x0), y0: Math.max(a.y0, b.y0), x1: Math.min(a.x1, b.x1), y1: Math.min(a.y1, b.y1) };
  return r.x1 > r.x0 && r.y1 > r.y0 ? r : null;
}

function inset(r: R64, l: number, t: number, rr: number, b: number): R64 {
  const x0 = r.x0 + l;
  const y0 = r.y0 + t;
  return { x0, y0, x1: Math.max(r.x1 - rr, x0), y1: Math.max(r.y1 - b, y0) };
}

/** Converts edges to `f32` and derives the size from the rounded edges. */
function toRect(r: R64): Rect {
  const x0 = f32(r.x0);
  const y0 = f32(r.y0);
  return { x: x0, y: y0, w: Math.max(f32(f32(r.x1) - x0), 0), h: Math.max(f32(f32(r.y1) - y0), 0) };
}

function sameR64(a: R64, b: R64): boolean {
  return a.x0 === b.x0 && a.y0 === b.y0 && a.x1 === b.x1 && a.y1 === b.y1;
}

/** `sane_len`: finite and positive, else 0. */
function saneLen(v: number): number {
  return Number.isFinite(v) && v > 0 ? v : 0;
}

/**
 * `f64::powi`, which LLVM lowers to compiler-rt's `__powidf2`
 * (square-and-multiply). `Math.pow` can differ in the last bit.
 */
function powi(a: number, b: number): number {
  let r = 1;
  let base = a;
  let e = b;
  for (;;) {
    if (e & 1) r *= base;
    e = Math.trunc(e / 2);
    if (e === 0) break;
    base *= base;
  }
  return r;
}

/**
 * Squarified subdivision (Bruls, Huizing & van Wijk): lays out `weights`
 * (positive, sorted descending) inside `rect`, one rect per weight.
 */
function squarify(weights: readonly number[], rect: R64, out: R64[]): void {
  const total = weights.reduce((a, b) => a + b, 0);
  const a = area(rect);
  if (weights.length === 0) return;
  if (!(total > 0 && a > 0 && Number.isFinite(total) && Number.isFinite(a))) {
    for (let i = 0; i < weights.length; i++) out.push({ x0: rect.x0, y0: rect.y0, x1: rect.x0, y1: rect.y0 });
    return;
  }
  const k = a / total;
  const r = { ...rect };
  const n = weights.length;
  let i = 0;
  while (i < n) {
    const w = width(r);
    const h = height(r);
    const side = Math.min(w, h);
    if (i + 1 === n || side <= 0) {
      for (let t = i; t < n; t++) out.push({ ...r });
      break;
    }
    const side2 = side * side;
    const head = (weights[i] ?? 0) * k;
    let sum = 0;
    let best = Infinity;
    let j = i;
    while (j < n) {
      const wa = (weights[j] ?? 0) * k;
      const s = sum + wa;
      const s2 = s * s;
      const worst = Math.max((side2 * head) / s2, s2 / (side2 * wa));
      if (j > i && worst > best) break;
      best = worst;
      sum = s;
      j++;
    }
    const lastRow = j === n;
    if (w >= h) {
      const x1 = lastRow ? r.x1 : Math.min(r.x0 + sum / h, r.x1);
      let y = r.y0;
      for (let t = i; t < j; t++) {
        const y1 = t + 1 === j ? r.y1 : Math.min(y + (((weights[t] ?? 0) * k) / sum) * h, r.y1);
        out.push({ x0: r.x0, y0: y, x1, y1 });
        y = y1;
      }
      r.x0 = x1;
    } else {
      const y1 = lastRow ? r.y1 : Math.min(r.y0 + sum / w, r.y1);
      let x = r.x0;
      for (let t = i; t < j; t++) {
        const x1 = t + 1 === j ? r.x1 : Math.min(x + (((weights[t] ?? 0) * k) / sum) * w, r.x1);
        out.push({ x0: x, y0: r.y0, x1, y1 });
        x = x1;
      }
      r.y0 = y1;
    }
    i = j;
  }
}

// -----------------------------------------------------------------------------
// Layout
// -----------------------------------------------------------------------------

/** What the layout reads from the tree (`source.rs` `LayoutSource`). */
export interface TreeSource {
  /** Bytes of entry `id` (directories: their subtree). */
  size(id: number): number;
  /** Direct children of `id` (any order; files have none). */
  children(id: number): readonly number[];
  isDir(id: number): boolean;
  /** Packed color key copied into the records. */
  colorKey(id: number): number;
}

/** One rect record (`RectRecord`, 32 bytes). */
export interface RectRecord extends Rect {
  id: number;
  colorKey: number;
  parent: number;
  depth: number;
  flags: number;
}

/** One "N small items" side-table entry (`AggregateRecord`, 24 bytes). */
export interface AggregateRecord {
  record: number;
  parent: number;
  dirId: number;
  count: number;
  bytes: number;
}

/** One label candidate (`LabelRecord`, 32 bytes). */
export interface LabelRecord extends Rect {
  record: number;
  id: number;
  size: number;
}

/** Cushion coefficients (`CushionRecord`, 16 bytes). */
export interface CushionRecord {
  kx2: number;
  ky2: number;
  kx1: number;
  ky1: number;
}

/** A treemap layout: records in pre-order plus side tables. */
export interface RectLayout {
  rects: RectRecord[];
  aggregates: AggregateRecord[];
  labels: LabelRecord[];
  /** Parallel to `rects`, or `null` for the flat style. */
  cushions: CushionRecord[] | null;
}

/** Inputs of {@link layoutTreemap}. */
export interface TreemapOptions {
  /** Canvas size in device px. */
  width: number;
  height: number;
  /** Device pixel ratio; spacing scales with it. */
  dpr: number;
  /** Visual zoom applied before layout. */
  transform: ViewTransform;
  /** Emit cushion coefficients. */
  cushion: boolean;
}

/** WinDirStat's cushion defaults (`CushionParams::default`). */
const CUSHION_HEIGHT = f32(0.38);
const CUSHION_FALLOFF = f32(0.91);

interface Surface {
  kx2: number;
  ky2: number;
  kx1: number;
  ky1: number;
}

function withRidges(s: Surface, r: R64, h: number): Surface {
  const out = { ...s };
  const w = width(r);
  if (w > 0) {
    out.kx1 += (4 * h * (r.x1 + r.x0)) / w;
    out.kx2 -= (4 * h) / w;
  }
  const hh = height(r);
  if (hh > 0) {
    out.ky1 += (4 * h * (r.y1 + r.y0)) / hh;
    out.ky2 -= (4 * h) / hh;
  }
  return out;
}

function ridgeHeight(depth: number): number {
  const v = CUSHION_HEIGHT * powi(CUSHION_FALLOFF, depth);
  return Number.isFinite(v) && v > 0 ? v : 0;
}

function cushionRecord(s: Surface): CushionRecord {
  const f = (v: number) => {
    const x = f32(v);
    return Number.isFinite(x) ? x : 0;
  };
  return { kx2: f(s.kx2), ky2: f(s.ky2), kx1: f(s.kx1), ky1: f(s.ky1) };
}

interface Item {
  id: number;
  size: number;
  rect: R64;
  /** Aggregate side-table index, or `NO_INDEX` for a real entry. */
  agg: number;
}

interface StackFrame {
  rec: number;
  depth: number;
  color: number;
  surface: Surface;
  start: number;
  next: number;
  end: number;
}

/**
 * Lays out the subtree under `root` as a nested squarified treemap
 * (`layout_treemap` with `TreemapConfig::new(width, height, dpr)`).
 *
 * Walks the tree in pre-order with an explicit stack. Per directory:
 * drop zero-size children, sort by size (id as tie-break), fold children
 * below one pixel of area into an aggregate, squarify, and move the cut
 * while a kept child comes out thinner than a pixel. Rects outside the
 * viewport are culled with their subtree.
 *
 * @param src - The tree.
 * @param root - Layout root id.
 * @param opts - Canvas, zoom and style.
 * @returns The layout.
 */
export function layoutTreemap(src: TreeSource, root: number, opts: TreemapOptions): RectLayout {
  const out: RectLayout = { rects: [], aggregates: [], labels: [], cushions: opts.cushion ? [] : null };
  const s = Number.isFinite(f32(opts.dpr)) && f32(opts.dpr) > 0 ? f32(opts.dpr) : 1;
  const pad = saneLen(f32(2 * s));
  const headerH = saneLen(f32(16 * s));
  const headerMinW = saneLen(f32(64 * s));
  const headerMinH = saneLen(f32(40 * s));
  const minPx = Math.max(1, 1e-3);
  const maxDepth = 64;
  const labelMinW = saneLen(f32(40 * s));
  const labelMinH = saneLen(f32(13 * s));

  const vw = f32(opts.width);
  const vh = f32(opts.height);
  const valid = Number.isFinite(vw) && Number.isFinite(vh) && vw >= 0 && vh >= 0;
  const clip: R64 = valid ? { x0: 0, y0: 0, x1: vw, y1: vh } : { x0: 0, y0: 0, x1: 0, y1: 0 };
  const t = opts.transform;
  const scale = Number.isFinite(t.scale) && t.scale > 0 ? t.scale : 1;
  const tx = Number.isFinite(t.tx) ? t.tx : 0;
  const ty = Number.isFinite(t.ty) ? t.ty : 0;
  const rootRect: R64 = { x0: clip.x0 * scale + tx, y0: clip.y0 * scale + ty, x1: clip.x1 * scale + tx, y1: clip.y1 * scale + ty };
  if (![rootRect.x0, rootRect.y0, rootRect.x1, rootRect.y1].every(Number.isFinite) || width(rootRect) < minPx || height(rootRect) < minPx) return out;

  const arena: Item[] = [];

  const layoutChildren = (dir: number, dirRec: number, content: R64) => {
    const all = src.children(dir);
    const kids = all.map((id) => [id, src.size(id)] as const).filter(([, sz]) => sz > 0);
    kids.sort((a, b) => b[1] - a[1] || a[0] - b[0]);
    const total = kids.reduce((acc, [, sz]) => acc + sz, 0);
    let cut = 0;
    let tail = total;
    let boxes: R64[] = [];
    if (total > 0) {
      const k = area(content) / total;
      const minArea = minPx * minPx;
      cut = kids.findIndex(([, sz]) => !(sz * k >= minArea));
      if (cut < 0) cut = kids.length;
      let rounds = 0;
      for (;;) {
        tail = 0;
        for (let i = cut; i < kids.length; i++) tail += kids[i]?.[1] ?? 0;
        const weights = kids.slice(0, cut).map(([, sz]) => sz);
        if (tail > 0) weights.push(tail);
        boxes = [];
        squarify(weights, content, boxes);
        const bad = boxes.slice(0, cut).findIndex((b) => width(b) < minPx || height(b) < minPx);
        if (bad < 0) break;
        rounds++;
        cut = rounds < 4 ? bad : Math.min(bad, Math.floor((cut * 3) / 4));
      }
    }
    const excluded = all.length - cut;
    let agg = NO_INDEX;
    if (excluded > 0) {
      agg = out.aggregates.length;
      out.aggregates.push({ record: NO_INDEX, parent: dirRec, dirId: dir, count: excluded, bytes: tail });
    }
    for (let i = 0; i < cut; i++) {
      const kid = kids[i];
      const box = boxes[i];
      if (kid && box) arena.push({ id: kid[0], size: kid[1], rect: box, agg: NO_INDEX });
    }
    const b = boxes[cut];
    if (tail > 0 && b && width(b) >= minPx && height(b) >= minPx) arena.push({ id: dir, size: tail, rect: b, agg });
  };

  const emit = (item: Item, parent: number, depth: number, parentSurface: Surface, parentColor: number): StackFrame | null => {
    const visible = intersect(item.rect, clip);
    if (!visible) return null;
    const isAgg = item.agg !== NO_INDEX;
    const isDir = !isAgg && src.isDir(item.id);
    const r = item.rect;
    const hasHeader = isDir && headerH > 0 && width(r) >= headerMinW && height(r) >= headerMinH && height(r) >= headerH + 2 * pad;
    const top = pad + (hasHeader ? headerH : 0);
    const content = inset(r, pad, top, pad, pad);
    const descend = isDir && depth < maxDepth && width(content) >= minPx && height(content) >= minPx && intersect(content, clip) !== null;

    let flags = 0;
    if (isDir) flags |= NodeFlag.DIR;
    if (hasHeader) flags |= NodeFlag.HAS_HEADER;
    flags |= isAgg ? NodeFlag.AGGREGATE : NodeFlag.SELECTABLE;
    if (isDir && !descend) flags |= NodeFlag.TRUNCATED;
    if (!sameR64(visible, r)) flags |= NodeFlag.CLIPPED;
    const color = isAgg ? parentColor : src.colorKey(item.id);
    const index = out.rects.length;
    out.rects.push({ ...toRect(visible), id: item.id, colorKey: color, parent, depth, flags });

    let surface: Surface = { kx2: 0, ky2: 0, kx1: 0, ky1: 0 };
    if (out.cushions) {
      surface = withRidges(parentSurface, r, ridgeHeight(depth));
      out.cushions.push(cushionRecord(surface));
    }
    const a = isAgg ? out.aggregates[item.agg] : undefined;
    if (a) a.record = index;

    let labelBox: R64 | null = null;
    if (hasHeader) labelBox = { x0: r.x0 + pad, y0: r.y0 + pad, x1: r.x1 - pad, y1: r.y0 + pad + headerH };
    else if (!descend) labelBox = inset(r, pad, pad, pad, pad);
    const lb = labelBox ? intersect(labelBox, clip) : null;
    if (lb && width(lb) >= labelMinW && height(lb) >= labelMinH) out.labels.push({ ...toRect(lb), record: index, id: item.id, size: item.size });

    if (!descend) return null;
    const start = arena.length;
    layoutChildren(item.id, index, content);
    const end = arena.length;
    return end > start ? { rec: index, depth, color, surface, start, next: start, end } : null;
  };

  const stack: StackFrame[] = [];
  const first = emit({ id: root, size: src.size(root), rect: rootRect, agg: NO_INDEX }, NO_INDEX, 0, { kx2: 0, ky2: 0, kx1: 0, ky1: 0 }, 0);
  if (first) stack.push(first);
  for (let top = stack[stack.length - 1]; top; top = stack[stack.length - 1]) {
    if (top.next < top.end) {
      const item = arena[top.next] as Item;
      top.next++;
      const f = emit(item, top.rec, top.depth + 1, top.surface, top.color);
      if (f) stack.push(f);
    } else {
      stack.pop();
      arena.length = top.start;
    }
  }
  return out;
}

/** Inputs of {@link layoutIcicle}. */
export interface IcicleOptions {
  /** Canvas size in device px. */
  width: number;
  height: number;
  dpr: number;
  /** `true` stacks from the bottom (flame graph). */
  bottomUp: boolean;
}

interface Slice {
  id: number;
  size: number;
  lo: number;
  hi: number;
  agg: number;
}

/**
 * Slices `dir`'s children over `[lo, hi]` in proportion to size, folding
 * sub-pixel children into one aggregate slice (`partition.rs`).
 */
function partition(src: TreeSource, dir: number, dirRec: number, lo: number, hi: number, pxPerUnit: number, minPx: number, aggregates: AggregateRecord[], out: Slice[]): void {
  const all = src.children(dir);
  const kids = all.map((id) => [id, src.size(id)] as const).filter(([, sz]) => sz > 0);
  kids.sort((a, b) => b[1] - a[1] || a[0] - b[0]);
  const total = kids.reduce((acc, [, sz]) => acc + sz, 0);
  const span = hi - lo;
  let k = 0;
  let cut = 0;
  if (total > 0 && span > 0) {
    k = span / total;
    const px = k * pxPerUnit;
    cut = kids.findIndex(([, sz]) => !(sz * px >= minPx));
    if (cut < 0) cut = kids.length;
  }
  let tail = 0;
  for (let i = cut; i < kids.length; i++) tail += kids[i]?.[1] ?? 0;
  const excluded = all.length - cut;
  let agg = NO_INDEX;
  if (excluded > 0) {
    agg = aggregates.length;
    aggregates.push({ record: NO_INDEX, parent: dirRec, dirId: dir, count: excluded, bytes: tail });
  }
  let cursor = lo;
  for (let i = 0; i < cut; i++) {
    const [id, size] = kids[i] ?? [0, 0];
    const end = i + 1 === cut && tail === 0 ? hi : Math.min(cursor + size * k, hi);
    out.push({ id, size, lo: cursor, hi: end, agg: NO_INDEX });
    cursor = end;
  }
  if (tail > 0 && (hi - cursor) * pxPerUnit >= minPx) out.push({ id: dir, size: tail, lo: cursor, hi, agg });
}

/**
 * Lays out the subtree under `root` as an icicle or flame graph
 * (`layout_icicle` with `IcicleConfig::new(width, height, dpr)`): depth `d`
 * is row `d`, and each directory's x-range is sliced among its children.
 *
 * @param src - The tree.
 * @param root - Layout root id.
 * @param opts - Canvas and orientation.
 * @returns The layout (no cushions).
 */
export function layoutIcicle(src: TreeSource, root: number, opts: IcicleOptions): RectLayout {
  const out: RectLayout = { rects: [], aggregates: [], labels: [], cushions: null };
  const s = Number.isFinite(f32(opts.dpr)) && f32(opts.dpr) > 0 ? f32(opts.dpr) : 1;
  const maxDepth = 8;
  const vw = f32(opts.width);
  const vh = f32(opts.height);
  const vp: R64 = Number.isFinite(vw) && Number.isFinite(vh) && vw >= 0 && vh >= 0 ? { x0: 0, y0: 0, x1: vw, y1: vh } : { x0: 0, y0: 0, x1: 0, y1: 0 };
  const row = f32(vh / f32(maxDepth + 1));
  const minPx = 1;
  if (!(Number.isFinite(row) && row > 0 && width(vp) >= minPx)) return out;
  const gap = saneLen(s);
  const barH = gap < row ? row - gap : row;
  const ins = saneLen(f32(3 * s));
  const lw = saneLen(f32(40 * s));
  const lh = saneLen(f32(12 * s));
  const rowY = (depth: number) => (opts.bottomUp ? vp.y1 - row * (depth + 1) + Math.min(gap, row - barH) : vp.y0 + row * depth);

  const arena: Slice[] = [];
  const frames: { rec: number; depth: number; color: number; start: number; next: number; end: number }[] = [];
  let pending: { s: Slice; parent: number; depth: number; parentColor: number } | null = {
    s: { id: root, size: src.size(root), lo: vp.x0, hi: vp.x1, agg: NO_INDEX },
    parent: NO_INDEX,
    depth: 0,
    parentColor: 0,
  };
  for (;;) {
    if (pending) {
      const { s: sl, parent, depth, parentColor } = pending;
      pending = null;
      const isAgg = sl.agg !== NO_INDEX;
      const isDir = !isAgg && src.isDir(sl.id);
      const descend = isDir && depth < maxDepth;
      let flags = isAgg ? NodeFlag.AGGREGATE : NodeFlag.SELECTABLE;
      if (isDir) flags |= NodeFlag.DIR;
      if (isDir && !descend) flags |= NodeFlag.TRUNCATED;
      const y0 = rowY(depth);
      const bar: R64 = { x0: sl.lo, y0, x1: sl.hi, y1: y0 + barH };
      const color = isAgg ? parentColor : src.colorKey(sl.id);
      const index = out.rects.length;
      out.rects.push({ ...toRect(bar), id: sl.id, colorKey: color, parent, depth, flags });
      const a = isAgg ? out.aggregates[sl.agg] : undefined;
      if (a) a.record = index;
      const label = inset(bar, ins, ins, ins, ins);
      if (width(label) >= lw && height(label) >= lh) out.labels.push({ ...toRect(label), record: index, id: sl.id, size: sl.size });
      if (descend) {
        const start = arena.length;
        partition(src, sl.id, index, sl.lo, sl.hi, 1, minPx, out.aggregates, arena);
        if (arena.length > start) frames.push({ rec: index, depth, color, start, next: start, end: arena.length });
      }
    }
    const top = frames[frames.length - 1];
    if (!top) break;
    if (top.next < top.end) {
      pending = { s: arena[top.next] as Slice, parent: top.rec, depth: top.depth + 1, parentColor: top.color };
      top.next++;
    } else {
      frames.pop();
      arena.length = top.start;
    }
  }
  return out;
}

/** A sunburst layout: sectors in the record geometry slots plus the chart frame. */
export interface ArcLayout {
  /**
   * Sectors in pre-order. The geometry slots hold `a0, a1, r0, r1` (angles
   * from 12 o'clock, radii in device px) in place of `x, y, w, h`.
   */
  layout: RectLayout;
  /** Chart center in device px. */
  center: [number, number];
  /** Ring thickness (and the root disc's radius). */
  ring: number;
}

/**
 * Lays out the subtree under `root` as a sunburst (`layout_sunburst` with
 * `SunburstConfig::new(width, height, dpr)`): the root is the central disc
 * and depth `d` the ring `[d·ring, (d+1)·ring]`.
 *
 * @param src - The tree.
 * @param root - Layout root id.
 * @param opts - Canvas size and device pixel ratio.
 * @returns The layout.
 */
export function layoutSunburst(src: TreeSource, root: number, opts: { width: number; height: number; dpr: number }): ArcLayout {
  const layout: RectLayout = { rects: [], aggregates: [], labels: [], cushions: null };
  const s = Number.isFinite(f32(opts.dpr)) && f32(opts.dpr) > 0 ? f32(opts.dpr) : 1;
  const maxDepth = 6;
  const minPx = Math.max(saneLen(f32(1.5 * s)), 1e-3);
  const vw = f32(opts.width);
  const vh = f32(opts.height);
  const vp: R64 = Number.isFinite(vw) && Number.isFinite(vh) && vw >= 0 && vh >= 0 ? { x0: 0, y0: 0, x1: vw, y1: vh } : { x0: 0, y0: 0, x1: 0, y1: 0 };
  const radius = Math.min(width(vp), height(vp)) / 2 - saneLen(f32(4 * s));
  const ring = radius / (maxDepth + 1);
  if (!(Number.isFinite(radius) && ring > 0)) return { layout, center: [0, 0], ring: 0 };
  const out: ArcLayout = { layout, center: [f32((vp.x0 + vp.x1) / 2), f32((vp.y0 + vp.y1) / 2)], ring: f32(ring) };

  const arena: Slice[] = [];
  const frames: { rec: number; depth: number; start: number; next: number; end: number }[] = [];
  let pending: { s: Slice; parent: number; depth: number } | null = { s: { id: root, size: src.size(root), lo: 0, hi: 2 * Math.PI, agg: NO_INDEX }, parent: NO_INDEX, depth: 0 };
  for (;;) {
    if (pending) {
      const { s: sl, parent, depth } = pending;
      pending = null;
      const isAgg = sl.agg !== NO_INDEX;
      const isDir = !isAgg && src.isDir(sl.id);
      const descend = isDir && depth < maxDepth;
      let flags = isAgg ? NodeFlag.AGGREGATE : NodeFlag.SELECTABLE;
      if (isDir) flags |= NodeFlag.DIR;
      if (isDir && !descend) flags |= NodeFlag.TRUNCATED;
      const r0 = depth === 0 ? 0 : ring * depth;
      const index = layout.rects.length;
      const colorKey = isAgg ? (layout.rects[parent]?.colorKey ?? 0) : src.colorKey(sl.id);
      layout.rects.push({ x: f32(sl.lo), y: f32(sl.hi), w: f32(r0), h: f32(ring * (depth + 1)), id: sl.id, colorKey, parent, depth, flags });
      const a = isAgg ? layout.aggregates[sl.agg] : undefined;
      if (a) a.record = index;
      if (descend) {
        const start = arena.length;
        partition(src, sl.id, index, sl.lo, sl.hi, ring * (depth + 2), minPx, layout.aggregates, arena);
        if (arena.length > start) frames.push({ rec: index, depth, start, next: start, end: arena.length });
      }
    }
    const top = frames[frames.length - 1];
    if (!top) break;
    if (top.next < top.end) {
      pending = { s: arena[top.next] as Slice, parent: top.rec, depth: top.depth + 1 };
      top.next++;
    } else {
      frames.pop();
      arena.length = top.start;
    }
  }
  return out;
}

/**
 * Lays out the subtree under `root` as a radial mind map (`layout_mindmap`
 * with `MindMapConfig::new(width, height, dpr)`).
 *
 * Collects the visible tree in pre-order (the 12 largest children per
 * directory, the rest folded into "N more"), weights every node by its
 * visible leaves, then splits the circle top-down into wedges so subtrees
 * never cross. Node radius grows with `√(size / root size)`.
 *
 * @param src - The tree.
 * @param root - Layout root id.
 * @param opts - Canvas size and device pixel ratio.
 * @returns Circles in the record geometry slots (`cx, cy, r, angle`).
 */
export function layoutMindMap(src: TreeSource, root: number, opts: { width: number; height: number; dpr: number }): RectLayout {
  const out: RectLayout = { rects: [], aggregates: [], labels: [], cushions: null };
  const s = Number.isFinite(f32(opts.dpr)) && f32(opts.dpr) > 0 ? f32(opts.dpr) : 1;
  const maxDepth = 3;
  const keep = 12;
  const vw = f32(opts.width);
  const vh = f32(opts.height);
  const vp: R64 = Number.isFinite(vw) && Number.isFinite(vh) && vw >= 0 && vh >= 0 ? { x0: 0, y0: 0, x1: vw, y1: vh } : { x0: 0, y0: 0, x1: 0, y1: 0 };
  const minR = saneLen(f32(3 * s));
  const maxR = Math.max(saneLen(f32(24 * s)), minR);
  const radius = Math.min(width(vp), height(vp)) / 2 - saneLen(f32(8 * s)) - maxR;
  if (!(Number.isFinite(radius) && radius > 0)) return out;

  interface MapNode {
    id: number;
    size: number;
    parent: number;
    depth: number;
    flags: number;
    agg: number;
    weight: number;
  }
  const nodes: MapNode[] = [];
  const arena: [number, number, number][] = [];
  const frames: { rec: number; start: number; next: number; end: number }[] = [];
  let pending: [number, number, number, number, number] | null = [root, src.size(root), NO_INDEX, 0, NO_INDEX];
  for (;;) {
    if (pending) {
      const [id, size, parent, depth, agg] = pending;
      pending = null;
      const isAgg = agg !== NO_INDEX;
      const isDir = !isAgg && src.isDir(id);
      const descend = isDir && depth < maxDepth;
      let flags = isAgg ? NodeFlag.AGGREGATE : NodeFlag.SELECTABLE;
      if (isDir) flags |= NodeFlag.DIR;
      if (isDir && !descend) flags |= NodeFlag.TRUNCATED;
      const index = nodes.length;
      const node: MapNode = { id, size, parent, depth, flags, agg, weight: 1 };
      nodes.push(node);
      if (descend) {
        const all = src.children(id);
        const kids = all.map((c) => [c, src.size(c)] as const).filter(([, sz]) => sz > 0);
        kids.sort((a, b) => b[1] - a[1] || a[0] - b[0]);
        const shown = Math.min(kids.length, keep);
        const start = arena.length;
        for (const [c, sz] of kids.slice(0, shown)) arena.push([c, sz, NO_INDEX]);
        if (all.length > shown) {
          const bytes = kids.slice(shown).reduce((acc, [, sz]) => acc + sz, 0);
          const a = out.aggregates.length;
          out.aggregates.push({ record: NO_INDEX, parent: index, dirId: id, count: all.length - shown, bytes });
          arena.push([id, bytes, a]);
        }
        if (arena.length > start) {
          node.weight = 0;
          frames.push({ rec: index, start, next: start, end: arena.length });
        }
      }
    }
    const top = frames[frames.length - 1];
    if (!top) break;
    if (top.next < top.end) {
      const [id, size, agg] = arena[top.next] as [number, number, number];
      top.next++;
      pending = [id, size, top.rec, (nodes[top.rec]?.depth ?? 0) + 1, agg];
    } else {
      frames.pop();
      arena.length = top.start;
    }
  }

  for (let i = nodes.length - 1; i >= 1; i--) {
    const n = nodes[i] as MapNode;
    const p = nodes[n.parent];
    if (p) p.weight += n.weight;
  }
  const deepest = Math.max(1, ...nodes.map((n) => n.depth));
  const ring = radius / deepest;
  const rootSize = nodes[0]?.size ?? 0;
  const cx = (vp.x0 + vp.x1) / 2;
  const cy = (vp.y0 + vp.y1) / 2;
  const wedge: [number, number, number][] = [];
  nodes.forEach((n, i) => {
    let lo = 0;
    let hi = 2 * Math.PI;
    if (i !== 0) {
      const w = wedge[n.parent] as [number, number, number];
      const span = ((w[1] - w[0]) * n.weight) / Math.max(nodes[n.parent]?.weight ?? 0, 1);
      lo = w[2];
      hi = w[2] + span;
      w[2] = hi;
    }
    wedge.push([lo, hi, lo]);
    const mid = (lo + hi) / 2;
    const dist = ring * n.depth;
    const share = rootSize > 0 ? Math.sqrt(Math.min(Math.max(n.size / rootSize, 0), 1)) : 0;
    const r = minR + (maxR - minR) * share;
    const color = n.agg === NO_INDEX ? src.colorKey(n.id) : (out.rects[n.parent]?.colorKey ?? 0);
    const index = out.rects.length;
    out.rects.push({ x: f32(cx + dist * Math.sin(mid)), y: f32(cy - dist * Math.cos(mid)), w: f32(r), h: f32(mid), id: n.id, colorKey: color, parent: n.parent, depth: n.depth, flags: n.flags });
    const a = n.agg === NO_INDEX ? undefined : out.aggregates[n.agg];
    if (a) a.record = index;
  });
  return out;
}

// -----------------------------------------------------------------------------
// Circle packing
// -----------------------------------------------------------------------------

interface Circle {
  x: number;
  y: number;
  r: number;
}

const U64 = (1n << 64n) - 1n;

/** `SplitMix64` from `source.rs`, in `BigInt` for exact `u64` wrapping. */
class SplitMix64 {
  constructor(private state: bigint) {}
  next(): bigint {
    this.state = (this.state + 0x9e3779b97f4a7c15n) & U64;
    let z = this.state;
    z = ((z ^ (z >> 30n)) * 0xbf58476d1ce4e5b9n) & U64;
    z = ((z ^ (z >> 27n)) * 0x94d049bb133111ebn) & U64;
    return z ^ (z >> 31n);
  }
  below(n: number): number {
    return Number(this.next() % BigInt(Math.max(n, 1)));
  }
}

/** Places `c` tangent to both `a` and `b`. */
function place(b: Circle, a: Circle, c: Circle): void {
  const dx = b.x - a.x;
  const dy = b.y - a.y;
  const d2 = dx * dx + dy * dy;
  if (d2 > 0) {
    const a2 = (a.r + c.r) * (a.r + c.r);
    const b2 = (b.r + c.r) * (b.r + c.r);
    if (a2 > b2) {
      const x = (d2 + b2 - a2) / (2 * d2);
      const y = Math.sqrt(Math.max(b2 / d2 - x * x, 0));
      c.x = b.x - x * dx - y * dy;
      c.y = b.y - x * dy + y * dx;
    } else {
      const x = (d2 + a2 - b2) / (2 * d2);
      const y = Math.sqrt(Math.max(a2 / d2 - x * x, 0));
      c.x = a.x + x * dx - y * dy;
      c.y = a.y + x * dy + y * dx;
    }
  } else {
    c.x = a.x + c.r;
    c.y = a.y;
  }
}

function intersects(a: Circle, b: Circle): boolean {
  const dr = a.r + b.r - 1e-6;
  const dx = b.x - a.x;
  const dy = b.y - a.y;
  return dr > 0 && dr * dr > dx * dx + dy * dy;
}

function enclosesNot(a: Circle, b: Circle): boolean {
  const dr = a.r - b.r;
  const dx = b.x - a.x;
  const dy = b.y - a.y;
  return dr < 0 || dr * dr < dx * dx + dy * dy;
}

function enclosesWeak(a: Circle, b: Circle): boolean {
  const dr = a.r - b.r + Math.max(a.r, b.r, 1) * 1e-9;
  const dx = b.x - a.x;
  const dy = b.y - a.y;
  return dr > 0 && dr * dr > dx * dx + dy * dy;
}

const enclosesWeakAll = (a: Circle, b: readonly Circle[]) => b.every((p) => enclosesWeak(a, p));

function enclose2(a: Circle, b: Circle): Circle {
  const x21 = b.x - a.x;
  const y21 = b.y - a.y;
  const r21 = b.r - a.r;
  const l = Math.sqrt(x21 * x21 + y21 * y21);
  if (l === 0) return a.r >= b.r ? a : b;
  return { x: (a.x + b.x + (x21 / l) * r21) / 2, y: (a.y + b.y + (y21 / l) * r21) / 2, r: (l + a.r + b.r) / 2 };
}

function enclose3(a: Circle, b: Circle, c: Circle): Circle | null {
  const { x: x1, y: y1, r: r1 } = a;
  const { x: x2, y: y2, r: r2 } = b;
  const { x: x3, y: y3, r: r3 } = c;
  const a2 = x1 - x2;
  const a3 = x1 - x3;
  const b2 = y1 - y2;
  const b3 = y1 - y3;
  const c2 = r2 - r1;
  const c3 = r3 - r1;
  const d1 = x1 * x1 + y1 * y1 - r1 * r1;
  const d2 = d1 - x2 * x2 - y2 * y2 + r2 * r2;
  const d3 = d1 - x3 * x3 - y3 * y3 + r3 * r3;
  const ab = a3 * b2 - a2 * b3;
  if (ab === 0) return null;
  const xa = (b2 * d3 - b3 * d2) / (ab * 2) - x1;
  const xb = (b3 * c2 - b2 * c3) / ab;
  const ya = (a3 * d2 - a2 * d3) / (ab * 2) - y1;
  const yb = (a2 * c3 - a3 * c2) / ab;
  const qa = xb * xb + yb * yb - 1;
  const qb = 2 * (r1 + xa * xb + ya * yb);
  const qc = xa * xa + ya * ya - r1 * r1;
  const r = -(Math.abs(qa) > 1e-6 ? (qb + Math.sqrt(qb * qb - 4 * qa * qc)) / (2 * qa) : qc / qb);
  const e = { x: x1 + xa + xb * r, y: y1 + ya + yb * r, r };
  return Number.isFinite(e.x) && Number.isFinite(e.y) && Number.isFinite(e.r) ? e : null;
}

function encloseBasis(b: readonly Circle[]): Circle | null {
  const [p, q, s] = b;
  if (b.length === 1 && p) return p;
  if (b.length === 2 && p && q) return enclose2(p, q);
  if (b.length === 3 && p && q && s) return enclose3(p, q, s);
  return null;
}

function extendBasis(b: readonly Circle[], p: Circle): Circle[] | null {
  if (enclosesWeakAll(p, b)) return [p];
  for (const bi of b) if (enclosesNot(p, bi) && enclosesWeakAll(enclose2(bi, p), b)) return [bi, p];
  for (let i = 0; i < b.length - 1; i++) {
    for (let j = i + 1; j < b.length; j++) {
      const bi = b[i] as Circle;
      const bj = b[j] as Circle;
      if (enclosesNot(enclose2(bi, bj), p) && enclosesNot(enclose2(bi, p), bj) && enclosesNot(enclose2(bj, p), bi)) {
        const e3 = enclose3(bi, bj, p);
        if (e3 && enclosesWeakAll(e3, b)) return [bi, bj, p];
      }
    }
  }
  return null;
}

/** Welzl's move-to-front smallest enclosing circle. */
function welzl(circles: readonly Circle[]): Circle | null {
  let basis: Circle[] = [];
  let e: Circle | null = null;
  let i = 0;
  let steps = 0;
  const cap = Math.max(circles.length * 64, 64);
  while (i < circles.length) {
    if (++steps > cap) return null;
    const p = circles[i] as Circle;
    if (e && enclosesWeak(e, p)) {
      i++;
    } else {
      const next = extendBasis(basis, p);
      if (!next) return null;
      basis = next;
      e = encloseBasis(basis);
      if (!e) return null;
      i = 0;
    }
  }
  return e;
}

/** Smallest circle enclosing `chain`, checked against `all`; a centroid bound on failure. */
function enclose(chain: Circle[], all: readonly Circle[]): Circle {
  const rng = new SplitMix64(0x00c14c1en);
  for (let i = chain.length - 1; i >= 1; i--) {
    const j = rng.below(i + 1);
    [chain[i], chain[j]] = [chain[j] as Circle, chain[i] as Circle];
  }
  const e = welzl(chain);
  const ok =
    e !== null &&
    Number.isFinite(e.r) &&
    Number.isFinite(e.x) &&
    Number.isFinite(e.y) &&
    all.every((p) => Math.sqrt((p.x - e.x) * (p.x - e.x) + (p.y - e.y) * (p.y - e.y)) + p.r <= e.r * (1 + 1e-9) + 1e-9);
  if (ok) return e;
  const n = Math.max(all.length, 1);
  const x = all.reduce((acc, p) => acc + p.x, 0) / n;
  const y = all.reduce((acc, p) => acc + p.y, 0) / n;
  const r = all.reduce((acc, p) => Math.max(acc, Math.sqrt((p.x - x) * (p.x - x) + (p.y - y) * (p.y - y)) + p.r), 0);
  return { x, y, r };
}

/**
 * Packs `c` (radii set, positions overwritten) around the origin with
 * d3-hierarchy's front-chain `packSiblings`, then recenters on the
 * enclosing circle.
 *
 * @returns The enclosing circle's radius.
 */
function packSiblings(c: Circle[]): number {
  const n = c.length;
  const at = (i: number) => c[i] as Circle;
  if (n === 0) return 0;
  at(0).x = 0;
  at(0).y = 0;
  if (n === 1) return at(0).r;
  at(0).x = -at(1).r;
  at(1).x = at(0).r;
  at(1).y = 0;
  if (n === 2) {
    const e = { x: (at(0).x + at(1).x + (at(1).r - at(0).r)) / 2, r: at(0).r + at(1).r };
    for (const p of c) p.x -= e.x;
    return e.r;
  }
  place({ ...at(1) }, { ...at(0) }, at(2));
  const next = new Array<number>(n).fill(0);
  const prev = new Array<number>(n).fill(0);
  let a = 0;
  let b = 1;
  next[a] = b;
  prev[2] = b;
  next[b] = 2;
  prev[a] = 2;
  next[2] = a;
  prev[b] = a;
  const nx = (i: number) => next[i] ?? 0;
  const pv = (i: number) => prev[i] ?? 0;
  const score = (node: number) => {
    const p = at(node);
    const q = at(nx(node));
    const ab = p.r + q.r;
    const dx = (p.x * q.r + q.x * p.r) / ab;
    const dy = (p.y * q.r + q.y * p.r) / ab;
    return dx * dx + dy * dy;
  };

  let i = 3;
  let retries = 0;
  while (i < n) {
    const ca = { ...at(a) };
    const cb = { ...at(b) };
    place(ca, cb, at(i));
    const ci = { ...at(i) };
    let j = nx(b);
    let k = pv(a);
    let sj = cb.r;
    let sk = ca.r;
    let retry = false;
    if (retries <= n + 8) {
      for (;;) {
        if (sj <= sk) {
          if (intersects(at(j), ci)) {
            b = j;
            next[a] = b;
            prev[b] = a;
            retry = true;
            break;
          }
          sj += at(j).r;
          j = nx(j);
        } else {
          if (intersects(at(k), ci)) {
            a = k;
            next[a] = b;
            prev[b] = a;
            retry = true;
            break;
          }
          sk += at(k).r;
          k = pv(k);
        }
        if (j === nx(k)) break;
      }
    }
    if (retry) {
      retries++;
      continue;
    }
    retries = 0;
    prev[i] = a;
    next[i] = b;
    next[a] = i;
    prev[b] = i;
    b = i;
    let aa = score(a);
    let cur = nx(i);
    while (cur !== b) {
      const s = score(cur);
      if (s < aa) {
        a = cur;
        aa = s;
      }
      cur = nx(cur);
    }
    b = nx(a);
    i++;
  }

  const chain = [{ ...at(b) }];
  let cur = nx(b);
  while (cur !== b && chain.length <= n) {
    chain.push({ ...at(cur) });
    cur = nx(cur);
  }
  const e = enclose(chain, c);
  for (const p of c) {
    p.x -= e.x;
    p.y -= e.y;
  }
  return e.r;
}

interface PackItem {
  id: number;
  size: number;
  c: Circle;
  agg: number;
}

/**
 * Lays out the subtree under `root` as nested packed circles (`layout_pack`
 * with `PackConfig::new(width, height, dpr)`). Each directory packs its
 * children with radii `√(size / largest)` and scales the pack into its own
 * circle; children too small to reach `min_px` fold into an aggregate.
 *
 * @param src - The tree.
 * @param root - Layout root id.
 * @param opts - Canvas size and device pixel ratio.
 * @returns Circles in the record geometry slots (`cx, cy, r, 0`).
 */
export function layoutPack(src: TreeSource, root: number, opts: { width: number; height: number; dpr: number }): RectLayout {
  const out: RectLayout = { rects: [], aggregates: [], labels: [], cushions: null };
  const s = Number.isFinite(f32(opts.dpr)) && f32(opts.dpr) > 0 ? f32(opts.dpr) : 1;
  const vw = f32(opts.width);
  const vh = f32(opts.height);
  const vp: R64 = Number.isFinite(vw) && Number.isFinite(vh) && vw >= 0 && vh >= 0 ? { x0: 0, y0: 0, x1: vw, y1: vh } : { x0: 0, y0: 0, x1: 0, y1: 0 };
  const minPx = Math.max(2, 1e-3);
  const pad = saneLen(f32(3 * s));
  const gap = saneLen(f32(1 * s));
  const labelMin = saneLen(f32(28 * s));
  const maxDepth = 64;
  const radius = Math.min(width(vp), height(vp)) / 2 - saneLen(f32(4 * s));
  if (!(Number.isFinite(radius) && 2 * radius >= minPx)) return out;

  const packChildren = (dir: number, dirRec: number, content: Circle, arena: PackItem[]) => {
    const all = src.children(dir);
    const kids = all.map((id) => [id, src.size(id)] as const).filter(([, z]) => z > 0);
    kids.sort((a, b) => b[1] - a[1] || a[0] - b[0]);
    let cut = 0;
    let tail = 0;
    const dropped: number[] = [];
    let aggKept = false;
    let circles: Circle[] = [];
    const first = kids[0];
    if (first) {
      const largest = first[1];
      const rel = (z: number) => Math.sqrt(z / largest);
      const sumR2 = kids.reduce((acc, [, z]) => acc + z / largest, 0);
      const maxScale = content.r / Math.sqrt(sumR2);
      cut = kids.findIndex(([, z]) => !(2 * rel(z) * maxScale - gap >= minPx));
      if (cut < 0) cut = kids.length;
      for (let i = cut; i < kids.length; i++) tail += kids[i]?.[1] ?? 0;
      circles = kids.slice(0, cut).map(([, z]) => ({ x: 0, y: 0, r: rel(z) }));
      if (tail > 0) circles.push({ x: 0, y: 0, r: Math.sqrt(tail / largest) });
      const enclosing = packSiblings(circles);
      const scale = enclosing > 0 ? content.r / enclosing : 0;
      for (const p of circles) {
        p.x = content.x + p.x * scale;
        p.y = content.y + p.y * scale;
        p.r = Math.max(p.r * scale - gap / 2, 0);
      }
      for (let i = 0; i < cut; i++) if (2 * (circles[i]?.r ?? 0) < minPx) dropped.push(i);
      aggKept = tail > 0 && 2 * (circles[cut]?.r ?? 0) >= minPx;
    }
    const droppedBytes = dropped.reduce((acc, i) => acc + (kids[i]?.[1] ?? 0), 0);
    const excluded = all.length - cut + dropped.length;
    let agg = NO_INDEX;
    if (excluded > 0) {
      agg = out.aggregates.length;
      out.aggregates.push({ record: NO_INDEX, parent: dirRec, dirId: dir, count: excluded, bytes: tail + droppedBytes });
    }
    const skip = new Set(dropped);
    for (let i = 0; i < cut; i++) {
      if (skip.has(i)) continue;
      const [id, size] = kids[i] ?? [0, 0];
      arena.push({ id, size, c: circles[i] as Circle, agg: NO_INDEX });
    }
    if (aggKept) arena.push({ id: dir, size: tail, c: circles[cut] as Circle, agg });
  };

  const arena: PackItem[] = [];
  const frames: { rec: number; depth: number; color: number; start: number; next: number; end: number }[] = [];
  let pending: { it: PackItem; parent: number; depth: number; parentColor: number } | null = {
    it: { id: root, size: src.size(root), c: { x: (vp.x0 + vp.x1) / 2, y: (vp.y0 + vp.y1) / 2, r: radius }, agg: NO_INDEX },
    parent: NO_INDEX,
    depth: 0,
    parentColor: 0,
  };
  for (;;) {
    if (pending) {
      const { it, parent, depth, parentColor } = pending;
      pending = null;
      const isAgg = it.agg !== NO_INDEX;
      const isDir = !isAgg && src.isDir(it.id);
      const content = it.c.r - pad;
      const descend = isDir && depth < maxDepth && 2 * content >= minPx;
      let flags = isAgg ? NodeFlag.AGGREGATE : NodeFlag.SELECTABLE;
      if (isDir) flags |= NodeFlag.DIR;
      if (isDir && !descend) flags |= NodeFlag.TRUNCATED;
      const color = isAgg ? parentColor : src.colorKey(it.id);
      const index = out.rects.length;
      out.rects.push({ x: f32(it.c.x), y: f32(it.c.y), w: f32(it.c.r), h: 0, id: it.id, colorKey: color, parent, depth, flags });
      const a = isAgg ? out.aggregates[it.agg] : undefined;
      if (a) a.record = index;
      const side = it.c.r * Math.SQRT2;
      if (!descend && side >= labelMin) {
        const h = side / 2;
        out.labels.push({ x: f32(it.c.x - h), y: f32(it.c.y - h), w: f32(side), h: f32(side), record: index, id: it.id, size: it.size });
      }
      if (descend) {
        const start = arena.length;
        packChildren(it.id, index, { ...it.c, r: content }, arena);
        if (arena.length > start) frames.push({ rec: index, depth, color, start, next: start, end: arena.length });
      }
    }
    const top = frames[frames.length - 1];
    if (!top) break;
    if (top.next < top.end) {
      pending = { it: arena[top.next] as PackItem, parent: top.rec, depth: top.depth + 1, parentColor: top.color };
      top.next++;
    } else {
      frames.pop();
      arena.length = top.start;
    }
  }
  return out;
}

// -----------------------------------------------------------------------------
// Transitions
// -----------------------------------------------------------------------------

/** One tween record (`TransitionRecord`, 48 bytes). */
export interface TransitionRecord {
  from: Rect;
  to: Rect;
  id: number;
  oldIndex: number;
  newIndex: number;
  kind: number;
  flags: number;
}

interface Camera {
  sx: number;
  sy: number;
  tx: number;
  ty: number;
}

const IDENTITY_CAMERA: Camera = { sx: 1, sy: 1, tx: 0, ty: 0 };

function validRect(r: Rect): boolean {
  return Number.isFinite(r.x) && Number.isFinite(r.y) && Number.isFinite(r.w) && Number.isFinite(r.h) && r.w >= 0 && r.h >= 0;
}

function cameraBetween(a: Rect, b: Rect): Camera | null {
  if (!(a.w > 0 && a.h > 0 && validRect(b))) return null;
  const sx = b.w / a.w;
  const sy = b.h / a.h;
  return { sx, sy, tx: b.x - a.x * sx, ty: b.y - a.y * sy };
}

function applyCamera(c: Camera, r: Rect): Rect {
  const out = { x: f32(r.x * c.sx + c.tx), y: f32(r.y * c.sy + c.ty), w: f32(r.w * c.sx), h: f32(r.h * c.sy) };
  return validRect(out) ? out : r;
}

const keyOf = (r: RectRecord) => `${r.id}:${(r.flags & NodeFlag.AGGREGATE) !== 0 ? 1 : 0}`;
const rectOf = (r: RectRecord): Rect => ({ x: r.x, y: r.y, w: r.w, h: r.h });

/** Camera from old to new: anchored on the root the two layouts share. */
function camera(old: RectLayout, next: RectLayout): Camera {
  const oldRoot = old.rects[0];
  const newRoot = next.rects[0];
  if (!oldRoot || !newRoot) return IDENTITY_CAMERA;
  const find = (l: RectLayout, id: number) => l.rects.find((r) => r.id === id && (r.flags & NodeFlag.AGGREGATE) === 0);
  if (oldRoot.id === newRoot.id) return cameraBetween(rectOf(oldRoot), rectOf(newRoot)) ?? IDENTITY_CAMERA;
  const inOld = find(old, newRoot.id);
  if (inOld) return cameraBetween(rectOf(inOld), rectOf(newRoot)) ?? IDENTITY_CAMERA;
  const inNew = find(next, oldRoot.id);
  if (inNew) return cameraBetween(rectOf(oldRoot), rectOf(inNew)) ?? IDENTITY_CAMERA;
  return IDENTITY_CAMERA;
}

/**
 * Builds the tween records between two treemap layouts
 * (`transition_rects`): disappearing old records first, then every new
 * record, with entries present on one side only moved along the camera
 * between the two roots.
 *
 * @param old - Previous layout.
 * @param next - New layout.
 * @returns Transition records in draw order.
 */
export function transitionRects(old: RectLayout, next: RectLayout): TransitionRecord[] {
  const cam = camera(old, next);
  const inverse: Camera = { sx: 1 / cam.sx, sy: 1 / cam.sy, tx: -cam.tx / cam.sx, ty: -cam.ty / cam.sy };
  const newIndex = new Map<string, number>();
  next.rects.forEach((r, i) => newIndex.set(keyOf(r), i));
  const oldIndex = new Map<string, number>();
  old.rects.forEach((r, i) => oldIndex.set(keyOf(r), i));
  const out: TransitionRecord[] = [];
  old.rects.forEach((o, i) => {
    if (newIndex.has(keyOf(o))) return;
    out.push({ from: rectOf(o), to: applyCamera(cam, rectOf(o)), id: o.id, oldIndex: i, newIndex: NO_INDEX, kind: TransitionKind.Disappear, flags: o.flags });
  });
  next.rects.forEach((n, j) => {
    const i = oldIndex.get(keyOf(n));
    const o = i === undefined ? undefined : old.rects[i];
    if (o && i !== undefined) out.push({ from: rectOf(o), to: rectOf(n), id: n.id, oldIndex: i, newIndex: j, kind: TransitionKind.Stay, flags: n.flags });
    else out.push({ from: applyCamera(inverse, rectOf(n)), to: rectOf(n), id: n.id, oldIndex: NO_INDEX, newIndex: j, kind: TransitionKind.Appear, flags: n.flags });
  });
  return out;
}

// -----------------------------------------------------------------------------
// Frame container
// -----------------------------------------------------------------------------

/** Header fields of a frame that are not sections. */
export interface FrameMeta {
  view: ViewKind;
  seq: number;
  root: number;
  rootBytes: number;
  width: number;
  height: number;
  dpr: number;
  transform: ViewTransform;
  /** Sunburst center and ring width; zero for other views. */
  center?: readonly [number, number];
  ringWidth?: number;
}

function u64(v: DataView, off: number, n: number): void {
  v.setUint32(off, n % 0x1_0000_0000, true);
  v.setUint32(off + 4, Math.floor(n / 0x1_0000_0000), true);
}

/**
 * Encodes a rect layout as a layout frame (`write_frame` in
 * `strata-layout/examples/export_fixtures.rs`): the 128-byte header, then
 * nodes, aggregates, labels, cushions and transitions, each 16-byte aligned.
 *
 * @param l - Layout.
 * @param meta - Header fields.
 * @param transitions - Tween records, or none.
 * @returns Frame bytes.
 */
export function encodeRectFrame(l: RectLayout, meta: FrameMeta, transitions: readonly TransitionRecord[] = []): ArrayBuffer {
  const sizes = [l.rects.length * 32, l.aggregates.length * 24, l.labels.length * 32, (l.cushions?.length ?? 0) * 16, transitions.length * 48];
  const offsets: number[] = [];
  let len = FRAME_HEADER_BYTES;
  for (const s of sizes) {
    len = Math.ceil(len / 16) * 16;
    offsets.push(len);
    len += s;
  }
  const buf = new ArrayBuffer(len);
  const v = new DataView(buf);
  const at = (k: number) => offsets[k] ?? 0;

  l.rects.forEach((r, i) => {
    const o = at(0) + i * 32;
    v.setFloat32(o, r.x, true);
    v.setFloat32(o + 4, r.y, true);
    v.setFloat32(o + 8, r.w, true);
    v.setFloat32(o + 12, r.h, true);
    v.setUint32(o + 16, r.id, true);
    v.setUint32(o + 20, r.colorKey, true);
    v.setUint32(o + 24, r.parent, true);
    v.setUint16(o + 28, r.depth, true);
    v.setUint16(o + 30, r.flags, true);
  });
  l.aggregates.forEach((a, i) => {
    const o = at(1) + i * 24;
    v.setUint32(o, a.record, true);
    v.setUint32(o + 4, a.parent, true);
    v.setUint32(o + 8, a.dirId, true);
    v.setUint32(o + 12, a.count, true);
    u64(v, o + 16, a.bytes);
  });
  l.labels.forEach((b, i) => {
    const o = at(2) + i * 32;
    v.setUint32(o, b.record, true);
    v.setUint32(o + 4, b.id, true);
    v.setFloat32(o + 8, b.x, true);
    v.setFloat32(o + 12, b.y, true);
    v.setFloat32(o + 16, b.w, true);
    v.setFloat32(o + 20, b.h, true);
    u64(v, o + 24, b.size);
  });
  l.cushions?.forEach((c, i) => {
    const o = at(3) + i * 16;
    v.setFloat32(o, c.kx2, true);
    v.setFloat32(o + 4, c.ky2, true);
    v.setFloat32(o + 8, c.kx1, true);
    v.setFloat32(o + 12, c.ky1, true);
  });
  transitions.forEach((t, i) => {
    const o = at(4) + i * 48;
    [t.from.x, t.from.y, t.from.w, t.from.h, t.to.x, t.to.y, t.to.w, t.to.h].forEach((x, k) => {
      v.setFloat32(o + k * 4, x, true);
    });
    v.setUint32(o + 32, t.id, true);
    v.setUint32(o + 36, t.oldIndex, true);
    v.setUint32(o + 40, t.newIndex, true);
    v.setUint16(o + 44, t.kind, true);
    v.setUint16(o + 46, t.flags, true);
  });

  v.setUint32(0, FRAME_MAGIC, true);
  v.setUint16(4, FRAME_VERSION, true);
  v.setUint16(6, meta.view, true);
  v.setUint32(8, meta.seq, true);
  v.setUint32(12, meta.root, true);
  v.setFloat32(16, meta.width, true);
  v.setFloat32(20, meta.height, true);
  v.setFloat32(24, meta.dpr, true);
  v.setUint32(28, (sizes[3] ? 1 : 0) | (sizes[4] ? 2 : 0), true);
  v.setFloat64(32, meta.transform.scale, true);
  v.setFloat64(40, meta.transform.tx, true);
  v.setFloat64(48, meta.transform.ty, true);
  v.setFloat32(56, meta.center?.[0] ?? 0, true);
  v.setFloat32(60, meta.center?.[1] ?? 0, true);
  v.setFloat32(64, meta.ringWidth ?? 0, true);
  sizes.forEach((s, k) => {
    v.setUint32(72 + k * 8, at(k), true);
    v.setUint32(76 + k * 8, s, true);
  });
  u64(v, 112, meta.rootBytes);
  return buf;
}
