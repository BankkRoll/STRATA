/**
 * Demo data: the layout fixtures `strata-layout` exports for the UI tests
 * (`crates/strata-layout/examples/export_fixtures.rs`), served byte for byte.
 *
 * Responsibilities:
 * - Fetch the synthetic 4,000-entry tree and the layout frames on demand.
 * - Answer name/size lookups for entry ids ({@link DemoEntries}). The fixture
 *   tree has no names, so each entry gets a neutral name derived from its
 *   category and id.
 * - Replay frames for layout requests ({@link DemoLayoutStream}) the way the
 *   backend would: fit the frame to the canvas, keep or drop the transition
 *   section, and play drill-up transitions backwards.
 */
import type { EntryInfo, EntryInfoProvider } from "@ui/lib/entries";
import { decodeFrame, NO_INDEX, TransitionKind, TRANSITION_STRIDE, type LayoutFrame } from "@ui/lib/layout/frame";
import type { FrameListener, LayoutErrorListener, LayoutRequest, LayoutStream } from "@ui/lib/layout/stream";
import { decodeColorKey } from "@ui/lib/palette";
import type { Safety, VisualView } from "@ui/lib/types";
import treeUrl from "@ui/lib/layout/__fixtures__/tree.bin?url";
import treemapUrl from "@ui/lib/layout/__fixtures__/treemap.frame.bin?url";
import drillUrl from "@ui/lib/layout/__fixtures__/treemap-drill.frame.bin?url";
import sunburstUrl from "@ui/lib/layout/__fixtures__/sunburst.frame.bin?url";
import icicleUrl from "@ui/lib/layout/__fixtures__/icicle.frame.bin?url";
import bubblesUrl from "@ui/lib/layout/__fixtures__/bubbles.frame.bin?url";

/** Fixture frames the demo can show. */
export type FrameName = "treemap" | "treemap-drill" | "sunburst" | "icicle" | "bubbles";

const FRAME_URLS: Record<FrameName, string> = {
  treemap: treemapUrl,
  "treemap-drill": drillUrl,
  sunburst: sunburstUrl,
  icicle: icicleUrl,
  bubbles: bubblesUrl,
};

/** Name shown for the layout root. */
export const ROOT_NAME = "Demo volume";

/** Directory stem, file stem and extension per category id. */
const STEMS: readonly (readonly [string, string, string])[] = [
  ["misc", "item", ".bin"],
  ["system", "sys", ".dll"],
  ["apps", "app", ".exe"],
  ["games", "game", ".pak"],
  ["models", "model", ".gguf"],
  ["build", "obj", ".o"],
  ["cache", "cache", ".blob"],
  ["temp", "tmp", ".tmp"],
  ["downloads", "setup", ".zip"],
  ["documents", "doc", ".pdf"],
  ["media", "clip", ".mp4"],
  ["archives", "image", ".iso"],
  ["cloud", "synced", ".docx"],
  ["recycled", "$R", ".bin"],
  ["$Extend", "$meta", ""],
];

const SAFETY: readonly (Safety | null)[] = [null, "safe", "probably", "careful", "never"];

async function fetchBytes(url: string): Promise<ArrayBuffer> {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`${url}: HTTP ${r.status}`);
  return r.arrayBuffer();
}

/** One node of the fixture tree (16 bytes: `size:u64 parent:u32 info:u32`). */
interface TreeNode {
  size: number;
  parent: number;
  key: number;
  dir: boolean;
}

/**
 * Entry metadata for the fixture tree. Everything is local, so lookups are
 * synchronous and {@link load} resolves immediately.
 */
export class DemoEntries implements EntryInfoProvider {
  private readonly nodes: TreeNode[] = [];
  private readonly items: Uint32Array;
  private readonly cache = new Map<number, EntryInfo>();

  /** @param tree - `tree.bin` from the fixture export. */
  constructor(tree: ArrayBuffer) {
    const v = new DataView(tree);
    for (let o = 0; o + 16 <= v.byteLength; o += 16) {
      const info = v.getUint32(o + 12, true);
      this.nodes.push({
        size: v.getUint32(o, true) + v.getUint32(o + 4, true) * 2 ** 32,
        parent: v.getUint32(o + 8, true),
        key: info & 0x7fffffff,
        dir: info >>> 31 === 1,
      });
    }
    // Children always follow their parent in the synthetic tree, so one
    // backwards pass totals every subtree.
    this.items = new Uint32Array(this.nodes.length);
    for (let i = this.nodes.length - 1; i > 0; i--) {
      const p = this.nodes[i]?.parent ?? 0;
      if (p < i) this.items[p] = (this.items[p] ?? 0) + 1 + (this.items[i] ?? 0);
    }
  }

  /** Number of entries in the tree. */
  get count(): number {
    return this.nodes.length;
  }

  /** Display name of entry `id`. */
  name(id: number): string {
    const n = this.nodes[id];
    if (id === 0 || !n) return ROOT_NAME;
    const [dir, file, ext] = STEMS[n.key & 0xf] ?? ["misc", "item", ".bin"];
    return n.dir ? `${dir}-${id}` : `${file}-${id}${ext}`;
  }

  /** Packed color key of entry `id`. */
  colorKey(id: number): number {
    return this.nodes[id]?.key ?? 0;
  }

  /** Ids from the root down to `id`, inclusive. */
  pathTo(id: number): number[] {
    const out = [id];
    let cur = id;
    for (let guard = 0; cur !== 0 && guard < 64; guard++) {
      cur = this.nodes[cur]?.parent ?? 0;
      out.push(cur);
    }
    return out.reverse();
  }

  get(id: number): EntryInfo | undefined {
    let info = this.cache.get(id);
    if (info) return info;
    const n = this.nodes[id];
    if (!n) return undefined;
    const k = decodeColorKey(n.key);
    info = {
      id,
      name: this.name(id),
      isDir: n.dir,
      allocated: n.size,
      logical: n.size,
      items: this.items[id] ?? 0,
      category: k.category,
      app: k.app === 0 ? null : `App #${k.app}`,
      safety: SAFETY[k.safety] ?? null,
      modifiedMs: null,
      suspiciousTime: false,
    };
    this.cache.set(id, info);
    return info;
  }

  load(): Promise<void> {
    return Promise.resolve();
  }

  subscribe(): () => void {
    return () => undefined;
  }

  invalidate(): void {
    // Fixture data never changes.
  }
}

/** Lazily fetched fixture files. */
export class Fixtures {
  private readonly frames = new Map<FrameName, Promise<ArrayBuffer>>();
  private decoded = new Map<FrameName, LayoutFrame>();

  private constructor(
    /** Entry metadata. */
    readonly entries: DemoEntries,
    /** Entry id of the folder the drill frame is laid out for. */
    readonly drillRoot: number,
  ) {}

  /**
   * Loads the tree plus the two treemap frames the demo opens with.
   *
   * @returns Ready fixtures.
   */
  static async load(): Promise<Fixtures> {
    const [tree, drill] = await Promise.all([fetchBytes(treeUrl), fetchBytes(drillUrl)]);
    const f = new Fixtures(new DemoEntries(tree), decodeFrame(drill.slice(0)).root);
    f.frames.set("treemap-drill", Promise.resolve(drill));
    void f.bytes("treemap");
    return f;
  }

  /** Raw bytes of a frame (fetched once). */
  bytes(name: FrameName): Promise<ArrayBuffer> {
    let p = this.frames.get(name);
    if (!p) {
      p = fetchBytes(FRAME_URLS[name]);
      this.frames.set(name, p);
    }
    return p;
  }

  /** A decoded copy of a frame, kept for reading geometry. */
  async frame(name: FrameName): Promise<LayoutFrame> {
    let f = this.decoded.get(name);
    if (!f) {
      f = decodeFrame((await this.bytes(name)).slice(0));
      this.decoded.set(name, f);
    }
    return f;
  }
}

/**
 * Builds the transition section for drilling back up from the drill frame
 * to the root frame.
 *
 * The backend computes this with `transition_rects(drill, root)`; because
 * record matching is symmetric, that is the drill frame's own section with
 * `from`/`to`, the indices and appear/disappear swapped. Records are emitted
 * in the order `match_records` would produce (drill-only records first, then
 * the root frame in pre-order) so parents still draw beneath their children.
 */
function reverseTransitions(drill: LayoutFrame, root: LayoutFrame): Uint8Array {
  const t = drill.transitions;
  if (!t) return new Uint8Array(0);
  const order: number[] = [];
  for (let i = 0; i < t.count; i++) if (t.oldIndex(i) === NO_INDEX) order.push(i);
  const kept: number[] = [];
  for (let i = 0; i < t.count; i++) if (t.oldIndex(i) !== NO_INDEX) kept.push(i);
  kept.sort((a, b) => t.oldIndex(a) - t.oldIndex(b));
  order.push(...kept);

  const out = new ArrayBuffer(t.count * TRANSITION_STRIDE);
  const f32 = new Float32Array(out);
  const u32 = new Uint32Array(out);
  order.forEach((src, dst) => {
    const s = src * 12;
    const d = dst * 12;
    for (let k = 0; k < 4; k++) {
      f32[d + k] = t.f32[s + 4 + k] ?? 0;
      f32[d + 4 + k] = t.f32[s + k] ?? 0;
    }
    const oldIndex = t.newIndex(src);
    const newIndex = t.oldIndex(src);
    const kind = t.kind(src);
    const swapped = kind === TransitionKind.Appear ? TransitionKind.Disappear : kind === TransitionKind.Disappear ? TransitionKind.Appear : kind;
    const flags = newIndex !== NO_INDEX ? root.nodes.flags(newIndex) : drill.nodes.flags(oldIndex);
    u32[d + 8] = t.id(src);
    u32[d + 9] = oldIndex;
    u32[d + 10] = newIndex;
    u32[d + 11] = (swapped | (flags << 16)) >>> 0;
  });
  return new Uint8Array(out);
}

/**
 * Copies a frame for delivery: new sequence number, a transform that fits
 * its fixed 960×640 layout into the requested canvas, and the transition
 * section kept, replaced or dropped.
 */
function prepare(src: ArrayBuffer, seq: number, req: LayoutRequest, transitions: Uint8Array | "keep" | "drop"): ArrayBuffer {
  const extra = transitions instanceof Uint8Array && transitions.byteLength > 0 ? transitions : null;
  const pad = (16 - (src.byteLength % 16)) % 16;
  const out = new ArrayBuffer(src.byteLength + (extra ? pad + extra.byteLength : 0));
  new Uint8Array(out).set(new Uint8Array(src));
  const v = new DataView(out);
  v.setUint32(8, seq, true);

  // NOTE: the fixtures are laid out once at 960×640 device px. Recording the
  // frame as computed under a zoom of 1/k makes the controller draw it at
  // k× (centered), so the same bytes stay sharp at any canvas size.
  const fw = v.getFloat32(16, true);
  const fh = v.getFloat32(20, true);
  const k = Math.min(req.width / fw, req.height / fh);
  const ox = (req.width - fw * k) / 2;
  const oy = (req.height - fh * k) / 2;
  v.setFloat64(32, 1 / k, true);
  v.setFloat64(40, -ox / k, true);
  v.setFloat64(48, -oy / k, true);

  if (extra) {
    new Uint8Array(out).set(extra, src.byteLength + pad);
    v.setUint32(104, src.byteLength + pad, true);
    v.setUint32(108, extra.byteLength, true);
  } else if (transitions !== "keep") {
    v.setUint32(108, 0, true);
  }
  return out;
}

/**
 * Layout stream over the fixtures. Stands in for the backend's
 * `layout_request` channel: same request type, same frame bytes.
 */
export class DemoLayoutStream implements LayoutStream {
  private readonly frameListeners = new Set<FrameListener>();
  private readonly errorListeners = new Set<LayoutErrorListener>();
  private seq = 0;
  private shown: { view: VisualView; root: number } | null = null;

  /** @param fixtures - Loaded fixtures. */
  constructor(private readonly fixtures: Fixtures) {}

  /** The fixture frame that answers a request. */
  private nameFor(req: LayoutRequest): FrameName | null {
    switch (req.view) {
      case "treemap":
        return req.root === this.fixtures.drillRoot ? "treemap-drill" : "treemap";
      case "sunburst":
      case "icicle":
      case "bubbles":
        return req.view;
      default:
        return null;
    }
  }

  request(req: LayoutRequest): number {
    const seq = ++this.seq;
    const name = this.nameFor(req);
    const prev = this.shown;
    this.shown = { view: req.view, root: req.root };
    void (async () => {
      if (!name) throw new Error(`no fixture frame for the ${req.view} view`);
      const src = await this.fixtures.bytes(name);
      // Only drills between the root and the drill folder have transitions.
      let transitions: Uint8Array | "keep" | "drop" = "drop";
      if (req.animate && prev?.view === "treemap" && prev.root !== req.root) {
        if (name === "treemap-drill") transitions = "keep";
        else transitions = reverseTransitions(await this.fixtures.frame("treemap-drill"), await this.fixtures.frame("treemap"));
      }
      if (seq !== this.seq) return;
      const frame = decodeFrame(prepare(src, seq, req, transitions));
      for (const l of this.frameListeners) l(frame);
    })().catch((err: unknown) => {
      for (const l of this.errorListeners) l(err);
    });
    return seq;
  }

  subscribe(listener: FrameListener): () => void {
    this.frameListeners.add(listener);
    return () => this.frameListeners.delete(listener);
  }

  onError(listener: LayoutErrorListener): () => void {
    this.errorListeners.add(listener);
    return () => this.errorListeners.delete(listener);
  }

  close(): void {
    this.frameListeners.clear();
    this.errorListeners.clear();
  }
}
