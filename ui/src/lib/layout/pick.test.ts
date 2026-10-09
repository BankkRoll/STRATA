// @vitest-environment node
import { describe, expect, it } from "vitest";
import { fixtureFrame, fixturePicks, fixtureTree } from "../../test/fixtures";
import { NO_INDEX, NodeFlag, TransitionKind, ViewKind, decodeFrame, FrameError } from "./frame";
import { ancestorIds, computeSubtreeEnd, createPicker, marqueeSelect, recordBounds } from "./pick";

const FIXTURES = [
  "treemap",
  "treemap-zoom",
  "treemap-wide",
  "icicle",
  "flame",
  "sunburst",
  "bubbles",
  "mindmap",
] as const;

describe("decodeFrame", () => {
  it("reads the header and sections of a real treemap frame", () => {
    const f = fixtureFrame("treemap");
    expect(f.view).toBe(ViewKind.Treemap);
    expect(f.seq).toBe(1);
    expect(f.root).toBe(0);
    expect([f.width, f.height]).toEqual([960, 640]);
    expect(f.dpr).toBeCloseTo(1.25);
    expect(f.transform).toEqual({ scale: 1, tx: 0, ty: 0 });
    expect(f.nodes.count).toBeGreaterThan(1000);
    expect(f.cushions?.length).toBe(f.nodes.count * 4);
    expect(f.transitions).toBeNull();
    // The root fills the viewport and has no parent.
    expect([f.nodes.geom(0, 0), f.nodes.geom(0, 1), f.nodes.geom(0, 2), f.nodes.geom(0, 3)]).toEqual([0, 0, 960, 640]);
    expect(f.nodes.parent(0)).toBe(NO_INDEX);
    expect(f.nodes.depth(0)).toBe(0);
    expect(f.nodes.flags(0) & NodeFlag.DIR).toBeTruthy();
  });

  it("matches the synthetic tree: ids, sizes and color keys", () => {
    const f = fixtureFrame("treemap");
    const tree = fixtureTree();
    expect(f.rootBytes).toBe(tree[0]?.size);
    for (let i = 0; i < f.nodes.count; i++) {
      const flags = f.nodes.flags(i);
      const node = tree[f.nodes.id(i)];
      expect(node).toBeDefined();
      if (flags & NodeFlag.AGGREGATE) continue;
      expect(f.nodes.colorKey(i)).toBe(node?.colorKey);
      expect(Boolean(flags & NodeFlag.DIR)).toBe(node?.dir);
      // Pre-order: the parent record precedes the child and holds its parent id.
      const p = f.nodes.parent(i);
      if (i > 0) {
        expect(p).toBeLessThan(i);
        expect(f.nodes.id(p)).toBe(node?.parent);
        expect(f.nodes.depth(i)).toBe(f.nodes.depth(p) + 1);
      }
    }
  });

  it("accounts every aggregate's bytes against its directory", () => {
    const f = fixtureFrame("treemap");
    expect(f.aggregates.count).toBeGreaterThan(0);
    for (let i = 0; i < f.aggregates.count; i++) {
      const a = f.aggregates.get(i);
      expect(f.nodes.id(a.parent)).toBe(a.dirId);
      if (a.record !== NO_INDEX) {
        expect(f.nodes.flags(a.record) & NodeFlag.AGGREGATE).toBeTruthy();
        expect(f.aggregates.forRecord(a.record)).toEqual(a);
      }
    }
  });

  it("decodes labels inside their records", () => {
    const f = fixtureFrame("treemap");
    expect(f.labels.count).toBeGreaterThan(0);
    for (let i = 0; i < f.labels.count; i++) {
      const l = f.labels.get(i);
      const b = recordBounds(f, l.record);
      expect(l.x).toBeGreaterThanOrEqual(b.x0 - 0.01);
      expect(l.x + l.w).toBeLessThanOrEqual(b.x1 + 0.01);
      expect(l.size).toBeGreaterThanOrEqual(0);
    }
  });

  it("decodes drill-down transitions consistently with the new frame", () => {
    const old = fixtureFrame("treemap");
    const f = fixtureFrame("treemap-drill");
    const t = f.transitions;
    expect(t).not.toBeNull();
    if (!t) return;
    let appear = 0;
    let disappear = 0;
    for (let i = 0; i < t.count; i++) {
      const kind = t.kind(i);
      if (kind === TransitionKind.Disappear) {
        disappear++;
        expect(t.newIndex(i)).toBe(NO_INDEX);
        expect(old.nodes.id(t.oldIndex(i))).toBe(t.id(i));
      } else {
        if (kind === TransitionKind.Appear) appear++;
        expect(f.nodes.id(t.newIndex(i))).toBe(t.id(i));
        // `to` is the new geometry.
        for (let k = 0; k < 4; k++) expect(t.f32[i * 12 + 4 + k]).toBe(f.nodes.geom(t.newIndex(i), k));
      }
    }
    expect(appear + disappear).toBeGreaterThan(0);
    expect(f.root).not.toBe(old.root);
  });

  it("reads the zoom transform and sunburst geometry", () => {
    const z = fixtureFrame("treemap-zoom");
    expect(z.transform.scale).toBe(3);
    expect(z.transform.tx).toBe(300 - 900);
    const s = fixtureFrame("sunburst");
    expect(s.view).toBe(ViewKind.Sunburst);
    expect([s.centerX, s.centerY]).toEqual([480, 320]);
    expect(s.ringWidth).toBeGreaterThan(0);
    expect(s.nodes.geom(0, 1)).toBeCloseTo(Math.PI * 2, 5);
  });

  it("rejects malformed frames", () => {
    expect(() => decodeFrame(new ArrayBuffer(16))).toThrow(FrameError);
    const bad = new Uint8Array(128);
    expect(() => decodeFrame(bad)).toThrow(/magic/);
    const good = new Uint8Array(fixtureFrame("mindmap").nodes.bytes.buffer.slice(0));
    const dv = new DataView(good.buffer);
    dv.setUint16(4, 9, true);
    expect(() => decodeFrame(good)).toThrow(/version/);
    dv.setUint16(4, 1, true);
    dv.setUint32(76, 0xffffff, true);
    expect(() => decodeFrame(good)).toThrow(/out of bounds/);
  });

  it("copies unaligned input instead of failing", () => {
    const src = new Uint8Array(fixtureFrame("mindmap").nodes.bytes.buffer);
    const shifted = new Uint8Array(src.byteLength + 1);
    shifted.set(src, 1);
    const f = decodeFrame(shifted.subarray(1));
    expect(f.view).toBe(ViewKind.MindMap);
  });
});

describe.each(FIXTURES)("picking matches Rust: %s", (name) => {
  const frame = fixtureFrame(name);
  const rust = fixturePicks(name);
  const picker = createPicker(frame);

  it("computes the same subtree_end table", () => {
    expect(Array.from(computeSubtreeEnd(frame.nodes))).toEqual(rust.subtreeEnd);
  });

  it("returns the same record, flags and ancestors for every sample point", () => {
    let hits = 0;
    for (const s of rust.picks) {
      const got = picker.pick(s.x, s.y);
      expect(got, `(${s.x}, ${s.y})`).toEqual(s.pick);
      if (got) hits++;
    }
    expect(hits).toBeGreaterThan(0);
  });
});

describe("picking cost", () => {
  it("stays far below 1 ms per hover query", () => {
    const frame = fixtureFrame("treemap-wide");
    const picker = createPicker(frame);
    const n = 20_000;
    const t0 = performance.now();
    let acc = 0;
    for (let k = 0; k < n; k++) acc += picker.hitTest((k * 37) % 960, (k * 91) % 640);
    const perQuery = (performance.now() - t0) / n;
    expect(acc).toBeGreaterThan(0);
    expect(perQuery).toBeLessThan(0.05);
  });
});

describe("ancestorIds and marqueeSelect", () => {
  it("returns root-first ancestor chains", () => {
    const frame = fixtureFrame("treemap");
    const deepest = Array.from({ length: frame.nodes.count }, (_, i) => i).reduce((a, b) =>
      frame.nodes.depth(b) > frame.nodes.depth(a) ? b : a,
    );
    const chain = ancestorIds(frame.nodes, deepest);
    expect(chain.length).toBe(frame.nodes.depth(deepest));
    expect(chain[0]).toBe(frame.root);
  });

  it("selects maximal fully-contained selectable records", () => {
    const frame = fixtureFrame("treemap");
    const picker = createPicker(frame);
    const all = marqueeSelect(frame, picker.subtreeEnd, { x0: -1, y0: -1, x1: 2000, y1: 2000 });
    expect(all).toEqual([0]);
    const part = marqueeSelect(frame, picker.subtreeEnd, { x0: 480, y0: 320, x1: 0, y1: 0 });
    expect(part.length).toBeGreaterThan(0);
    for (const i of part) {
      const b = recordBounds(frame, i);
      expect(b.x1).toBeLessThanOrEqual(480);
      expect(b.y1).toBeLessThanOrEqual(320);
      expect(frame.nodes.flags(i) & NodeFlag.SELECTABLE).toBeTruthy();
      // No chosen record is inside another chosen record.
      for (const j of part) if (j < i) expect(i >= (picker.subtreeEnd[j] ?? 0)).toBe(true);
    }
  });
});
