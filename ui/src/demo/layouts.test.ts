import { describe, expect, it } from "vitest";
import { ViewKind, type ViewTransform } from "../lib/layout/frame";
import { fixtureBytes, fixtureFrame, fixtureTree } from "../test/fixtures";
import { encodeRectFrame, layoutIcicle, layoutMindMap, layoutPack, layoutSunburst, layoutTreemap, transitionRects, type TreeSource } from "./layouts";

function source(): TreeSource {
  const nodes = fixtureTree();
  const children: number[][] = nodes.map(() => []);
  for (let i = 1; i < nodes.length; i++) children[nodes[i]?.parent ?? 0]?.push(i);
  return {
    size: (id) => nodes[id]?.size ?? 0,
    children: (id) => children[id] ?? [],
    isDir: (id) => nodes[id]?.dir ?? false,
    colorKey: (id) => nodes[id]?.colorKey ?? 0,
  };
}

const IDENTITY: ViewTransform = { scale: 1, tx: 0, ty: 0 };
const SIZE = { width: 960, height: 640, dpr: 1.25 };

/** First differing byte offset, for a readable failure. */
function firstDiff(a: ArrayBuffer, b: ArrayBuffer): number {
  const x = new Uint8Array(a);
  const y = new Uint8Array(b);
  for (let i = 0; i < Math.max(x.length, y.length); i++) if (x[i] !== y[i]) return i;
  return -1;
}

describe("layout port", () => {
  const src = source();

  it("reproduces the Rust root treemap byte for byte", () => {
    const l = layoutTreemap(src, 0, { ...SIZE, transform: IDENTITY, cushion: true });
    const bytes = encodeRectFrame(l, { view: ViewKind.Treemap, seq: 1, root: 0, rootBytes: src.size(0), ...SIZE, transform: IDENTITY });
    expect(firstDiff(bytes, fixtureBytes("treemap.frame.bin"))).toBe(-1);
  });

  it("reproduces the drill frame and its transitions", () => {
    const drill = fixtureFrame("treemap-drill").root;
    const opts = { ...SIZE, transform: IDENTITY, cushion: true };
    const before = layoutTreemap(src, 0, opts);
    const after = layoutTreemap(src, drill, opts);
    const bytes = encodeRectFrame(after, { view: ViewKind.Treemap, seq: 2, root: drill, rootBytes: src.size(drill), ...SIZE, transform: IDENTITY }, transitionRects(before, after));
    expect(firstDiff(bytes, fixtureBytes("treemap-drill.frame.bin"))).toBe(-1);
  });

  it.each([
    ["icicle", ViewKind.Icicle, false],
    ["flame", ViewKind.Flame, true],
  ] as const)("reproduces the Rust %s layout", (name, view, bottomUp) => {
    const l = layoutIcicle(src, 0, { ...SIZE, bottomUp });
    const bytes = encodeRectFrame(l, { view, seq: 1, root: 0, rootBytes: src.size(0), ...SIZE, transform: IDENTITY });
    expect(firstDiff(bytes, fixtureBytes(`${name}.frame.bin`))).toBe(-1);
  });

  it("reproduces the Rust sunburst", () => {
    const { layout, center, ring } = layoutSunburst(src, 0, SIZE);
    const bytes = encodeRectFrame(layout, { view: ViewKind.Sunburst, seq: 1, root: 0, rootBytes: src.size(0), ...SIZE, transform: IDENTITY, center, ringWidth: ring });
    expect(firstDiff(bytes, fixtureBytes("sunburst.frame.bin"))).toBe(-1);
  });

  it("reproduces the Rust bubbles", () => {
    const l = layoutPack(src, 0, SIZE);
    const bytes = encodeRectFrame(l, { view: ViewKind.Bubbles, seq: 1, root: 0, rootBytes: src.size(0), ...SIZE, transform: IDENTITY });
    expect(firstDiff(bytes, fixtureBytes("bubbles.frame.bin"))).toBe(-1);
  });

  it("reproduces the Rust mind map", () => {
    const l = layoutMindMap(src, 0, SIZE);
    const bytes = encodeRectFrame(l, { view: ViewKind.MindMap, seq: 1, root: 0, rootBytes: src.size(0), ...SIZE, transform: IDENTITY });
    expect(firstDiff(bytes, fixtureBytes("mindmap.frame.bin"))).toBe(-1);
  });

  it("reproduces a zoomed layout", () => {
    const transform = { scale: 3, tx: 300 - 300 * 3, ty: 200 - 200 * 3 };
    const l = layoutTreemap(src, 0, { ...SIZE, transform, cushion: false });
    const bytes = encodeRectFrame(l, { view: ViewKind.Treemap, seq: 1, root: 0, rootBytes: src.size(0), ...SIZE, transform });
    expect(firstDiff(bytes, fixtureBytes("treemap-zoom.frame.bin"))).toBe(-1);
  });
});
