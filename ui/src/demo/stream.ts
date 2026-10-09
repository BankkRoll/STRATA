/**
 * The demo's stand-in for the backend's layout stream: answers every
 * {@link LayoutRequest} with a frame laid out at the requested canvas size by
 * the `strata-layout` port in `layouts.ts`, over the sample tree.
 *
 * Responsibilities:
 * - Build a {@link TreeSource} for the request's size mode and filters, the
 *   way the engine applies them (filtered entries count as zero bytes).
 * - Run the requested view's layout, add drill transitions for rect views
 *   when the request asks to animate, and deliver the encoded frame.
 */
import type { FixtureData } from "../dev/fixtureServices";
import { IDENTITY_TRANSFORM, ViewKind } from "../lib/layout/frame";
import { BaseLayoutStream, type LayoutRequest } from "../lib/layout/stream";
import { decodeColorKey } from "../lib/palette";
import type { SizeMode, ViewFilters, VisualView } from "../lib/types";
import { encodeRectFrame, layoutIcicle, layoutMindMap, layoutPack, layoutSunburst, layoutTreemap, transitionRects, type FrameMeta, type RectLayout, type TreeSource } from "./layouts";

/**
 * Days since an entry of the sample tree was modified. Matches the
 * `modifiedMs` the fixture entry info reports (`age × 9` days).
 */
function ageDays(key: number): number {
  return decodeColorKey(key).age * 9;
}

/**
 * The sample tree as a layout source for one size mode and filter set.
 *
 * Without filters in allocated mode this reads the exported sizes as they
 * are, which is what the parity tests check. Otherwise sizes are recomputed
 * bottom-up: logical sizes are 97% of allocated (as the fixture entry info
 * reports), filtered-out files weigh nothing, and directories sum their
 * children.
 *
 * @param data - The decoded sample tree.
 * @param sizeMode - Allocated or logical bytes.
 * @param filters - Active view filters.
 * @returns The source.
 */
export function sampleSource(data: Pick<FixtureData, "nodes" | "children">, sizeMode: SizeMode, filters: ViewFilters): TreeSource {
  const { nodes, children } = data;
  const base: TreeSource = {
    size: (id) => nodes[id]?.size ?? 0,
    children: (id) => children[id] ?? [],
    isDir: (id) => nodes[id]?.dir ?? false,
    colorKey: (id) => nodes[id]?.key ?? 0,
  };
  const filtered = filters.excluded.length > 0 || filters.categories.length > 0 || filters.minBytes > 0 || filters.modifiedWithinDays !== null;
  if (sizeMode === "allocated" && !filtered) return base;

  const excluded = new Set(filters.excluded);
  const categories = new Set(filters.categories);
  const sizes = new Float64Array(nodes.length);
  // Children always have larger ids than their parent in the sample tree, so
  // one reverse pass totals every directory after its children.
  for (let id = nodes.length - 1; id >= 0; id--) {
    const n = nodes[id];
    if (!n || excluded.has(id)) continue;
    if (n.dir) {
      for (const c of children[id] ?? []) sizes[id] = (sizes[id] ?? 0) + (sizes[c] ?? 0);
      continue;
    }
    const bytes = sizeMode === "logical" ? Math.round(n.size * 0.97) : n.size;
    const keep =
      (categories.size === 0 || categories.has(decodeColorKey(n.key).category)) &&
      bytes >= filters.minBytes &&
      (filters.modifiedWithinDays === null || ageDays(n.key) <= filters.modifiedWithinDays);
    sizes[id] = keep ? bytes : 0;
  }
  return {
    ...base,
    size: (id) => sizes[id] ?? 0,
    // Filtered-out entries are gone, not "small items".
    children: filtered ? (id) => (children[id] ?? []).filter((c) => (sizes[c] ?? 0) > 0) : (id) => children[id] ?? [],
  };
}

const KIND: Readonly<Record<VisualView, ViewKind>> = {
  treemap: ViewKind.Treemap,
  icicle: ViewKind.Icicle,
  flame: ViewKind.Flame,
  sunburst: ViewKind.Sunburst,
  bubbles: ViewKind.Bubbles,
  mindmap: ViewKind.MindMap,
};

/**
 * Layout stream over the sample tree. Same request type and frame bytes as
 * the backend's; only the latest request is answered.
 */
export class DemoLayoutStream extends BaseLayoutStream {
  private seq = 0;
  private previous: { view: VisualView; layout: RectLayout } | null = null;
  private source: { key: string; src: TreeSource } | null = null;

  /** @param data - The decoded sample tree. */
  constructor(private readonly data: Pick<FixtureData, "nodes" | "children">) {
    super();
  }

  private sourceFor(req: LayoutRequest): TreeSource {
    const key = JSON.stringify([req.sizeMode, req.filters]);
    if (this.source?.key !== key) this.source = { key, src: sampleSource(this.data, req.sizeMode, req.filters) };
    return this.source.src;
  }

  request(req: LayoutRequest): number {
    const seq = ++this.seq;
    // PERF: resize and wheel bursts send a request per event; laying out only
    // the newest keeps the demo as responsive as the backend's coalescing.
    queueMicrotask(() => {
      if (seq !== this.seq) return;
      try {
        this.deliver(this.layout(req, seq));
      } catch (err) {
        this.fail(err);
      }
    });
    return seq;
  }

  private layout(req: LayoutRequest, seq: number): ArrayBuffer {
    const src = this.sourceFor(req);
    const size = { width: req.width, height: req.height, dpr: req.dpr };
    const transform = req.view === "treemap" ? req.transform : IDENTITY_TRANSFORM;
    const meta: FrameMeta = { view: KIND[req.view], seq, root: req.root, rootBytes: src.size(req.root), ...size, transform };
    switch (req.view) {
      case "sunburst": {
        const { layout, center, ring } = layoutSunburst(src, req.root, size);
        this.previous = null;
        return encodeRectFrame(layout, { ...meta, center, ringWidth: ring });
      }
      case "bubbles":
        this.previous = null;
        return encodeRectFrame(layoutPack(src, req.root, size), meta);
      case "mindmap":
        this.previous = null;
        return encodeRectFrame(layoutMindMap(src, req.root, size), meta);
      case "treemap":
      case "icicle":
      case "flame": {
        const layout =
          req.view === "treemap"
            ? layoutTreemap(src, req.root, { ...size, transform, cushion: req.style === "cushion" })
            : layoutIcicle(src, req.root, { ...size, bottomUp: req.view === "flame" });
        const prev = this.previous;
        const transitions = req.animate && prev?.view === req.view ? transitionRects(prev.layout, layout) : [];
        this.previous = { view: req.view, layout };
        return encodeRectFrame(layout, meta, transitions);
      }
    }
  }

  close(): void {
    this.previous = null;
  }
}
