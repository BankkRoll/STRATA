/**
 * Hit-testing directly on layout node buffers, mirroring `strata-layout`'s
 * Rust picking record for record so hover never needs an IPC round trip.
 *
 * Responsibilities:
 * - {@link computeSubtreeEnd}: the pre-order skip table (not on the wire;
 *   derived from `parent` in one reverse pass).
 * - {@link createPicker}: per-view pickers (nested treemap with per-directory
 *   grids for wide folders, stacked icicle/flame, polar sunburst, circle
 *   packing, mind map).
 * - {@link ancestorIds} and {@link marqueeSelect}.
 *
 * Rust compares rect edges in `f32` (`x + w` rounded to f32), so the rect
 * pickers round with `Math.fround` to give identical answers on shared
 * edges. Circle and arc pickers widen to f64 exactly like Rust does.
 */
import { NO_INDEX, NodeFlag, ViewKind, type LayoutFrame, type NodeBuffer } from "./frame";

/** Directories with more emitted children than this get a child grid (as in Rust). */
const WIDE_DIR = 256;

/**
 * Computes `subtreeEnd[i]`: one past the last descendant of record `i`, so
 * `i + 1 .. subtreeEnd[i]` are exactly its descendants.
 *
 * Pre-order puts every descendant after its ancestor, so one reverse pass
 * that pushes each record's end into its parent suffices. O(n).
 *
 * @param nodes - Node buffer in pre-order.
 * @returns The skip table.
 */
export function computeSubtreeEnd(nodes: NodeBuffer): Uint32Array {
  const n = nodes.count;
  const end = new Uint32Array(n);
  for (let i = 0; i < n; i++) end[i] = i + 1;
  const u32 = nodes.u32;
  for (let i = n - 1; i > 0; i--) {
    const p = u32[i * 8 + 6] ?? NO_INDEX;
    // Parents always precede children; anything else is a corrupt buffer
    // and is ignored rather than trusted.
    if (p < i) {
      const e = end[i] ?? 0;
      if (e > (end[p] ?? 0)) end[p] = e;
    }
  }
  return end;
}

/** Result of picking a point. */
export interface Pick {
  /** Record index of the hit. */
  index: number;
  /** Entry id (the directory id for aggregates). */
  id: number;
  /** Flag bits. */
  flags: number;
  /** Ancestor ids, root first, excluding `id`. */
  ancestors: number[];
}

/**
 * Collects the ids of every ancestor of record `i`, root first.
 *
 * @param nodes - Node buffer.
 * @param i - Record index.
 * @returns Ancestor ids (excluding record `i`).
 */
export function ancestorIds(nodes: NodeBuffer, i: number): number[] {
  const out: number[] = [];
  let cur = nodes.parent(i);
  let guard = nodes.count;
  while (cur !== NO_INDEX && cur < nodes.count && guard-- > 0) {
    out.push(nodes.id(cur));
    cur = nodes.parent(cur);
  }
  return out.reverse();
}

/** Hit-tester for one frame. */
export interface Picker {
  /**
   * Record index under (`x`, `y`) in the frame's device-pixel space, or -1.
   * Must stay well under 1 ms; it runs on every pointer move.
   */
  hitTest(x: number, y: number): number;
  /** {@link hitTest} plus id, flags and ancestor chain. */
  pick(x: number, y: number): Pick | null;
  /** The skip table, shared with marquee selection and keyboard navigation. */
  readonly subtreeEnd: Uint32Array;
}

function makePick(nodes: NodeBuffer, i: number): Pick | null {
  if (i < 0 || i >= nodes.count) return null;
  return { index: i, id: nodes.id(i), flags: nodes.flags(i), ancestors: ancestorIds(nodes, i) };
}

// -----------------------------------------------------------------------------
// Rects
// -----------------------------------------------------------------------------

/** Uniform grid over child rects: each child listed in every cell it overlaps (CSR). */
interface CellIndex {
  x0: number;
  y0: number;
  cell: number;
  cols: number;
  rows: number;
  starts: Uint32Array;
  items: Uint32Array;
}

const MAX_CELLS = 1 << 20;

function buildCellIndex(entries: Uint32Array, edges: Float32Array, cellPx: number): CellIndex | null {
  const n = entries.length;
  if (n === 0) return null;
  let x0 = Infinity;
  let y0 = Infinity;
  let x1 = -Infinity;
  let y1 = -Infinity;
  for (let k = 0; k < n; k++) {
    x0 = Math.min(x0, edges[k * 4] ?? 0);
    y0 = Math.min(y0, edges[k * 4 + 1] ?? 0);
    x1 = Math.max(x1, edges[k * 4 + 2] ?? 0);
    y1 = Math.max(y1, edges[k * 4 + 3] ?? 0);
  }
  const w = Math.max(x1 - x0, 1);
  const h = Math.max(y1 - y0, 1);
  let cell = Number.isFinite(cellPx) && cellPx >= 1 ? cellPx : 16;
  const cap = Math.min(MAX_CELLS, Math.max(n * 4, 64));
  while (Math.ceil(w / cell) * Math.ceil(h / cell) > cap) cell *= 1.5;
  const cols = Math.max(1, Math.ceil(w / cell));
  const rows = Math.max(1, Math.ceil(h / cell));
  const cx = (v: number) => Math.min(cols - 1, Math.max(0, Math.floor((v - x0) / cell)));
  const cy = (v: number) => Math.min(rows - 1, Math.max(0, Math.floor((v - y0) / cell)));
  const starts = new Uint32Array(cols * rows + 1);
  for (let k = 0; k < n; k++) {
    const a = cx(edges[k * 4] ?? 0);
    const b = cy(edges[k * 4 + 1] ?? 0);
    const c = cx(edges[k * 4 + 2] ?? 0);
    const d = cy(edges[k * 4 + 3] ?? 0);
    for (let yy = b; yy <= d; yy++) {
      for (let xx = a; xx <= c; xx++) {
        const at = yy * cols + xx + 1;
        starts[at] = (starts[at] ?? 0) + 1;
      }
    }
  }
  for (let k = 1; k < starts.length; k++) starts[k] = (starts[k] ?? 0) + (starts[k - 1] ?? 0);
  const items = new Uint32Array(starts[cols * rows] ?? 0);
  const fill = starts.slice();
  for (let k = 0; k < n; k++) {
    const a = cx(edges[k * 4] ?? 0);
    const b = cy(edges[k * 4 + 1] ?? 0);
    const c = cx(edges[k * 4 + 2] ?? 0);
    const d = cy(edges[k * 4 + 3] ?? 0);
    for (let yy = b; yy <= d; yy++) {
      for (let xx = a; xx <= c; xx++) {
        const slot = yy * cols + xx;
        items[fill[slot] ?? 0] = entries[k] ?? 0;
        fill[slot] = (fill[slot] ?? 0) + 1;
      }
    }
  }
  return { x0, y0, cell, cols, rows, starts, items };
}

function cellRange(g: CellIndex, x: number, y: number): [number, number] {
  const fx = (x - g.x0) / g.cell;
  const fy = (y - g.y0) / g.cell;
  if (!(fx >= 0 && fy >= 0)) return [0, 0];
  const cx = Math.floor(fx);
  const cy = Math.floor(fy);
  if (cx >= g.cols || cy >= g.rows) return [0, 0];
  const c = cy * g.cols + cx;
  return [g.starts[c] ?? 0, g.starts[c + 1] ?? 0];
}

class RectPicker implements Picker {
  readonly subtreeEnd: Uint32Array;
  private readonly f32: Float32Array;
  private readonly wide = new Map<number, CellIndex>();

  constructor(
    private readonly nodes: NodeBuffer,
    private readonly stacked: boolean,
  ) {
    this.subtreeEnd = computeSubtreeEnd(nodes);
    this.f32 = nodes.f32;
    if (!stacked) this.indexWideDirs();
  }

  private indexWideDirs(): void {
    const n = this.nodes.count;
    const counts = new Uint32Array(n);
    for (let i = 1; i < n; i++) {
      const p = this.nodes.parent(i);
      if (p < n) counts[p] = (counts[p] ?? 0) + 1;
    }
    for (let dir = 0; dir < n; dir++) {
      const count = counts[dir] ?? 0;
      if (count <= WIDE_DIR) continue;
      const entries = new Uint32Array(count);
      const edges = new Float32Array(count * 4);
      const end = this.subtreeEnd[dir] ?? 0;
      let k = 0;
      for (let c = dir + 1; c < end && k < count; c = this.subtreeEnd[c] ?? end) {
        entries[k] = c;
        edges[k * 4] = this.x0(c);
        edges[k * 4 + 1] = this.y0(c);
        edges[k * 4 + 2] = this.x1(c);
        edges[k * 4 + 3] = this.y1(c);
        k++;
      }
      const area = Math.fround(Math.fround(this.x1(dir) - this.x0(dir)) * Math.fround(this.y1(dir) - this.y0(dir)));
      const grid = buildCellIndex(entries.subarray(0, k), edges, Math.sqrt((area / count) * 4));
      if (grid) this.wide.set(dir, grid);
    }
  }

  private x0(i: number): number {
    return this.f32[i * 8] ?? 0;
  }
  private y0(i: number): number {
    return this.f32[i * 8 + 1] ?? 0;
  }
  private x1(i: number): number {
    return Math.fround((this.f32[i * 8] ?? 0) + (this.f32[i * 8 + 2] ?? 0));
  }
  private y1(i: number): number {
    return Math.fround((this.f32[i * 8 + 1] ?? 0) + (this.f32[i * 8 + 3] ?? 0));
  }
  private contains(i: number, x: number, y: number): boolean {
    return x >= this.x0(i) && y >= this.y0(i) && x < this.x1(i) && y < this.y1(i);
  }

  hitTest(xIn: number, yIn: number): number {
    if (this.nodes.count === 0 || !Number.isFinite(xIn) || !Number.isFinite(yIn)) return -1;
    const x = Math.fround(xIn);
    const y = Math.fround(yIn);
    return this.stacked ? this.hitStacked(x, y) : this.hitNested(x, y);
  }

  private hitNested(x: number, y: number): number {
    if (!this.contains(0, x, y)) return -1;
    let node = 0;
    descend: for (;;) {
      const grid = this.wide.get(node);
      if (grid) {
        // Siblings are disjoint, so at most one child in the cell contains the point.
        const [a, b] = cellRange(grid, x, y);
        for (let k = a; k < b; k++) {
          const c = grid.items[k] ?? 0;
          if (this.contains(c, x, y)) {
            node = c;
            continue descend;
          }
        }
        return node;
      }
      const end = this.subtreeEnd[node] ?? 0;
      for (let c = node + 1; c < end; c = this.subtreeEnd[c] ?? end) {
        if (this.contains(c, x, y)) {
          node = c;
          continue descend;
        }
      }
      return node;
    }
  }

  private hitStacked(x: number, y: number): number {
    let node = 0;
    for (;;) {
      if (x < this.x0(node) || x >= this.x1(node)) return -1;
      if (this.contains(node, x, y)) return node;
      const end = this.subtreeEnd[node] ?? 0;
      let next = -1;
      for (let c = node + 1; c < end; c = this.subtreeEnd[c] ?? end) {
        if (x >= this.x0(c) && x < this.x1(c)) {
          next = c;
          break;
        }
      }
      if (next < 0) return -1;
      node = next;
    }
  }

  pick(x: number, y: number): Pick | null {
    return makePick(this.nodes, this.hitTest(x, y));
  }
}

// -----------------------------------------------------------------------------
// Arcs
// -----------------------------------------------------------------------------

const TAU = Math.PI * 2;

class ArcPicker implements Picker {
  readonly subtreeEnd: Uint32Array;

  constructor(
    private readonly nodes: NodeBuffer,
    private readonly cx: number,
    private readonly cy: number,
    private readonly ring: number,
  ) {
    this.subtreeEnd = computeSubtreeEnd(nodes);
  }

  hitTest(xIn: number, yIn: number): number {
    if (this.nodes.count === 0 || !Number.isFinite(xIn) || !Number.isFinite(yIn) || !(this.ring > 0)) return -1;
    const dx = Math.fround(xIn) - this.cx;
    const dy = Math.fround(yIn) - this.cy;
    const r = Math.hypot(dx, dy);
    let a = Math.atan2(dx, -dy);
    if (a < 0) a += TAU;
    const ring = Math.floor(r / this.ring);
    if (!(ring >= 0 && ring < 0xffff)) return -1;
    const f = this.nodes.f32;
    let node = 0;
    for (let level = 0; level < ring; level++) {
      const end = this.subtreeEnd[node] ?? 0;
      let next = -1;
      for (let c = node + 1; c < end; c = this.subtreeEnd[c] ?? end) {
        if (a >= (f[c * 8] ?? 0) && a < (f[c * 8 + 1] ?? 0)) {
          next = c;
          break;
        }
      }
      if (next < 0) return -1;
      node = next;
    }
    return r >= (f[node * 8 + 2] ?? 0) && r < (f[node * 8 + 3] ?? 0) ? node : -1;
  }

  pick(x: number, y: number): Pick | null {
    return makePick(this.nodes, this.hitTest(x, y));
  }
}

// -----------------------------------------------------------------------------
// Circles
// -----------------------------------------------------------------------------

class CirclePicker implements Picker {
  readonly subtreeEnd: Uint32Array;

  constructor(
    private readonly nodes: NodeBuffer,
    private readonly nested: boolean,
  ) {
    this.subtreeEnd = computeSubtreeEnd(nodes);
  }

  private contains(i: number, x: number, y: number): boolean {
    const f = this.nodes.f32;
    const dx = x - (f[i * 8] ?? 0);
    const dy = y - (f[i * 8 + 1] ?? 0);
    const r = f[i * 8 + 2] ?? 0;
    return dx * dx + dy * dy < r * r;
  }

  hitTest(xIn: number, yIn: number): number {
    if (this.nodes.count === 0 || !Number.isFinite(xIn) || !Number.isFinite(yIn)) return -1;
    const x = Math.fround(xIn);
    const y = Math.fround(yIn);
    if (!this.nested) {
      // Mind-map nodes may overlap; later records draw on top, so the last hit wins.
      for (let i = this.nodes.count - 1; i >= 0; i--) if (this.contains(i, x, y)) return i;
      return -1;
    }
    if (!this.contains(0, x, y)) return -1;
    let node = 0;
    descend: for (;;) {
      const end = this.subtreeEnd[node] ?? 0;
      for (let c = node + 1; c < end; c = this.subtreeEnd[c] ?? end) {
        if (this.contains(c, x, y)) {
          node = c;
          continue descend;
        }
      }
      return node;
    }
  }

  pick(x: number, y: number): Pick | null {
    return makePick(this.nodes, this.hitTest(x, y));
  }
}

/**
 * Creates the picker matching a frame's view kind. Build cost is O(n) (skip
 * table plus wide-directory grids) and happens once per frame.
 *
 * @param frame - Decoded layout frame.
 * @returns A picker answering in the frame's device-pixel space.
 */
export function createPicker(frame: LayoutFrame): Picker {
  switch (frame.view) {
    case ViewKind.Treemap:
      return new RectPicker(frame.nodes, false);
    case ViewKind.Icicle:
    case ViewKind.Flame:
      return new RectPicker(frame.nodes, true);
    case ViewKind.Sunburst:
      return new ArcPicker(frame.nodes, frame.centerX, frame.centerY, frame.ringWidth);
    case ViewKind.Bubbles:
      return new CirclePicker(frame.nodes, true);
    case ViewKind.MindMap:
      return new CirclePicker(frame.nodes, false);
  }
}

// -----------------------------------------------------------------------------
// Marquee
// -----------------------------------------------------------------------------

/** Axis-aligned box in device pixels. */
export interface Box {
  x0: number;
  y0: number;
  x1: number;
  y1: number;
}

/** Bounding box of record `i` for any view kind. */
export function recordBounds(frame: LayoutFrame, i: number): Box {
  const f = frame.nodes.f32;
  const a = f[i * 8] ?? 0;
  const b = f[i * 8 + 1] ?? 0;
  const c = f[i * 8 + 2] ?? 0;
  const d = f[i * 8 + 3] ?? 0;
  switch (frame.view) {
    case ViewKind.Bubbles:
    case ViewKind.MindMap:
      return { x0: a - c, y0: b - c, x1: a + c, y1: b + c };
    case ViewKind.Sunburst: {
      // Conservative: the bounding box of the outer circle's sampled arc.
      let x0 = Infinity;
      let y0 = Infinity;
      let x1 = -Infinity;
      let y1 = -Infinity;
      const steps = 8;
      for (let s = 0; s <= steps; s++) {
        const ang = a + ((b - a) * s) / steps;
        for (const r of [c, d]) {
          const x = frame.centerX + r * Math.sin(ang);
          const y = frame.centerY - r * Math.cos(ang);
          x0 = Math.min(x0, x);
          y0 = Math.min(y0, y);
          x1 = Math.max(x1, x);
          y1 = Math.max(y1, y);
        }
      }
      return { x0, y0, x1, y1 };
    }
    default:
      return { x0: a, y0: b, x1: a + c, y1: b + d };
  }
}

/**
 * Selects the maximal selectable records lying entirely inside `box`: a
 * record is chosen when it fits and its parent does not, so dragging around
 * a whole folder selects the folder rather than all of its files.
 *
 * Subtrees that are chosen are skipped, which keeps big marquees cheap.
 *
 * @param frame - Decoded frame.
 * @param subtreeEnd - Skip table from the frame's picker.
 * @param box - Marquee in device pixels (any corner order).
 * @returns Record indices in pre-order.
 */
export function marqueeSelect(frame: LayoutFrame, subtreeEnd: Uint32Array, box: Box): number[] {
  const bx0 = Math.min(box.x0, box.x1);
  const bx1 = Math.max(box.x0, box.x1);
  const by0 = Math.min(box.y0, box.y1);
  const by1 = Math.max(box.y0, box.y1);
  const out: number[] = [];
  const n = frame.nodes.count;
  for (let i = 0; i < n; ) {
    const r = recordBounds(frame, i);
    const inside = r.x0 >= bx0 && r.y0 >= by0 && r.x1 <= bx1 && r.y1 <= by1;
    if (inside && (frame.nodes.flags(i) & NodeFlag.SELECTABLE) !== 0) {
      out.push(i);
      i = subtreeEnd[i] ?? i + 1;
    } else {
      i++;
    }
  }
  return out;
}
