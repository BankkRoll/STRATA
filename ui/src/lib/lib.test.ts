// @vitest-environment node
import { describe, expect, it, vi } from "vitest";
import { fixtureFrame, fixtureTree } from "../test/fixtures";
import { BackendUnavailableError } from "./backend";
import { CommandBus, ENTRY_ACTIONS, type UiActions } from "./commands";
import { BatchedEntryInfoProvider, LruCache, MAX_BATCH, type EntryInfo } from "./entries";
import { NodeFlag } from "./layout/frame";
import { childrenOf, navigate, recordOf } from "./layout/navigate";
import { computeSubtreeEnd } from "./layout/pick";
import { BaseLayoutStream } from "./layout/stream";
import { clampTransform, invertPoint, panBy, relativeTransform, zoomAbout } from "./layout/transform";
import {
  AGE_SUSPICIOUS,
  CATEGORIES,
  PALETTE_ROWS,
  PALETTE_WIDTH,
  ageBucket,
  buildPaletteTexture,
  categoryInfo,
  decodeColorKey,
  encodeColorKey,
} from "./palette";
import { ROW_PAGE_MAGIC, RowPageError, decodeRowPage } from "./rows";
import { fuzzyMatch } from "./search";

describe("color keys and palettes", () => {
  it("round-trips every field", () => {
    const f = { category: 14, safety: 4, age: 31, fileType: 255, app: 1023, recent: true };
    expect(decodeColorKey(encodeColorKey(f))).toEqual(f);
    expect(encodeColorKey(f)).toBeGreaterThan(0);
    expect(encodeColorKey(f) >>> 31).toBe(0);
  });

  it("decodes the fixture keys exported by the Rust example", () => {
    const tree = fixtureTree();
    for (const n of tree.slice(0, 500)) {
      const k = decodeColorKey(n.colorKey);
      expect(k.category).toBeLessThan(15);
      expect(k.safety).toBeLessThan(5);
      expect(k.age).toBeLessThan(21);
      expect(encodeColorKey(k)).toBe(n.colorKey);
    }
  });

  it("has a fixed palette and distinct pattern pairs for all 15 categories", () => {
    expect(CATEGORIES).toHaveLength(15);
    expect(new Set(CATEGORIES.map((c) => c.id)).size).toBe(15);
    const combos = new Set(CATEGORIES.map((c) => `${c.light}|${c.pattern}`));
    expect(combos.size).toBe(15);
    expect(categoryInfo(99).label).toBe("Unknown");
  });

  it("buckets ages like the backend", () => {
    expect(ageBucket(10)).toBe(1);
    expect(ageBucket(5 * 3600)).toBe(2);
    expect(ageBucket(2 * 86400)).toBe(4);
    expect(ageBucket(400 * 86400)).toBe(13);
    expect(ageBucket(50 * 365 * 86400)).toBe(20);
    expect(ageBucket(-5)).toBe(1);
    expect(ageBucket(1, true)).toBe(AGE_SUSPICIOUS);
  });

  it("builds a palette texture with patterns in the category alpha", () => {
    const tex = buildPaletteTexture(false);
    expect(tex.length).toBe(PALETTE_WIDTH * PALETTE_ROWS * 4);
    const apps = CATEGORIES.find((c) => c.key === "apps");
    expect(tex[(apps?.id ?? 0) * 4 + 3]).toBe(0);
    const games = CATEGORIES.find((c) => c.key === "games");
    expect(tex[(games?.id ?? 0) * 4 + 3]).toBe(5);
    expect(buildPaletteTexture(true)).not.toEqual(tex);
  });
});

describe("transforms", () => {
  it("composes the relative transform and inverts points", () => {
    const desired = { scale: 6, tx: -100, ty: -50 };
    const frame = { scale: 2, tx: -20, ty: -10 };
    const r = relativeTransform(desired, frame);
    // A layout point l maps to l*2-20 in the frame and to l*6-100 on screen.
    const l = 37;
    expect((l * 2 - 20) * r.scale + r.tx).toBeCloseTo(l * 6 - 100);
    expect(invertPoint(r, (l * 2 - 20) * r.scale + r.tx, 0)[0]).toBeCloseTo(l * 2 - 20);
  });

  it("zooms about the cursor and clamps to the canvas", () => {
    const z = zoomAbout({ scale: 1, tx: 0, ty: 0 }, 2, 100, 50, 400, 200);
    expect(z).toEqual({ scale: 2, tx: -100, ty: -50 });
    expect(zoomAbout(z, 0.1, 0, 0, 400, 200)).toEqual({ scale: 1, tx: 0, ty: 0 });
    expect(panBy(z, 500, 0, 400, 200).tx).toBe(0);
    expect(panBy(z, -5000, 0, 400, 200).tx).toBe(-400);
    expect(clampTransform({ scale: Number.NaN, tx: 5, ty: 5 }, 10, 10)).toEqual({ scale: 1, tx: 0, ty: 0 });
  });
});

describe("keyboard navigation", () => {
  const f = fixtureFrame("treemap");
  const end = computeSubtreeEnd(f.nodes);

  it("moves between siblings, into children and up to parents", () => {
    const first = navigate(f.nodes, end, -1, "ArrowRight");
    expect(f.nodes.parent(first)).toBe(0);
    const second = navigate(f.nodes, end, first, "ArrowRight");
    expect(second).not.toBe(first);
    expect(f.nodes.parent(second)).toBe(0);
    expect(navigate(f.nodes, end, second, "ArrowLeft")).toBe(first);
    expect(navigate(f.nodes, end, first, "ArrowLeft")).toBe(first);
    expect(navigate(f.nodes, end, second, "Home")).toBe(first);
    const sibs = childrenOf(f.nodes, end, 0);
    expect(navigate(f.nodes, end, first, "End")).toBe(sibs[sibs.length - 1]);
    const dir = sibs.find((i) => childrenOf(f.nodes, end, i).length > 0) ?? -1;
    const child = navigate(f.nodes, end, dir, "ArrowDown");
    expect(f.nodes.parent(child)).toBe(dir);
    expect(navigate(f.nodes, end, child, "ArrowUp")).toBe(dir);
    expect(navigate(f.nodes, end, 0, "ArrowUp")).toBe(0);
  });

  it("never lands on aggregates", () => {
    for (let i = 0; i < f.nodes.count; i++) {
      if (!(f.nodes.flags(i) & NodeFlag.SELECTABLE)) continue;
      for (const key of ["ArrowLeft", "ArrowRight", "ArrowDown", "Home", "End"] as const) {
        const j = navigate(f.nodes, end, i, key);
        expect(f.nodes.flags(j) & NodeFlag.SELECTABLE).toBeTruthy();
      }
    }
  });

  it("finds records by id", () => {
    expect(recordOf(f.nodes, f.nodes.id(5))).toBe(5);
    expect(recordOf(f.nodes, 0xfffffff)).toBe(-1);
  });
});

describe("entry info provider", () => {
  it("evicts least recently used entries", () => {
    const c = new LruCache<number, string>(2);
    c.set(1, "a");
    c.set(2, "b");
    c.get(1);
    c.set(3, "c");
    expect(c.has(2)).toBe(false);
    expect(c.has(1)).toBe(true);
    expect(c.size).toBe(2);
  });

  it("batches misses from one tick into one fetch per 512 ids", async () => {
    const info = (id: number): EntryInfo => ({
      id,
      name: `n${id}`,
      isDir: false,
      allocated: id,
      logical: id,
      items: 0,
      category: 0,
      app: null,
      safety: null,
      modifiedMs: null,
      suspiciousTime: false,
    });
    const fetcher = vi.fn((_v: string, ids: number[]) => Promise.resolve(ids.map(info)));
    const p = new BatchedEntryInfoProvider("v", fetcher);
    const listener = vi.fn();
    p.subscribe(listener);
    for (let i = 0; i < MAX_BATCH + 10; i++) expect(p.get(i)).toBeUndefined();
    await p.load([1, 2, 3]);
    expect(fetcher).toHaveBeenCalledTimes(2);
    expect(fetcher.mock.calls[0]?.[1]).toHaveLength(MAX_BATCH);
    expect(p.get(7)?.name).toBe("n7");
    expect(listener).toHaveBeenCalledTimes(1);
    await p.load([7]);
    expect(fetcher).toHaveBeenCalledTimes(2);
    p.invalidate([7]);
    await p.load([7]);
    expect(fetcher).toHaveBeenCalledTimes(3);
  });

  it("survives fetch failures", async () => {
    const p = new BatchedEntryInfoProvider("v", () => Promise.reject(new Error("boom")));
    await p.load([1]);
    expect(p.get(1)).toBeUndefined();
  });
});

describe("row pages", () => {
  function page(names: string[]): ArrayBuffer {
    const header = 32;
    const rows = names.length * 64;
    const units = names.map((n) => Array.from(n).flatMap((ch) => {
      const cp = ch.codePointAt(0) ?? 0;
      if (cp < 0x10000) return [cp];
      const v = cp - 0x10000;
      return [0xd800 + (v >> 10), 0xdc00 + (v & 0x3ff)];
    }));
    // An unpaired surrogate must decode to U+FFFD, not throw.
    units.push([0xd800, 0x41]);
    const nameUnits = units.reduce((a, u) => a + u.length, 0);
    const buf = new ArrayBuffer(header + rows + 64 + nameUnits * 2);
    const v = new DataView(buf);
    v.setUint32(0, ROW_PAGE_MAGIC, true);
    v.setUint16(4, 1, true);
    v.setUint32(8, 7, true);
    v.setUint32(12, 1000, true);
    v.setUint32(16, 40, true);
    v.setUint32(20, units.length, true);
    v.setUint32(24, header + rows + 64, true);
    v.setUint32(28, nameUnits * 2, true);
    let off = 0;
    units.forEach((u, i) => {
      const o = header + i * 64;
      v.setUint32(o, 100 + i, true);
      v.setUint32(o + 4, 7, true);
      v.setUint32(o + 8, i === 0 ? 1 | (1 << 14) : 0, true);
      v.setUint16(o + 12, 6, true);
      v.setUint8(o + 14, 2);
      v.setUint32(o + 16, 5, true);
      v.setUint32(o + 20, 1, true);
      v.setUint32(o + 24, 3, true);
      v.setUint32(o + 32, 9, true);
      v.setUint32(o + 40, 86400, true);
      v.setUint32(o + 56, off, true);
      v.setUint32(o + 60, u.length, true);
      u.forEach((c, k) => {
        v.setUint16(header + rows + 64 + (off + k) * 2, c, true);
      });
      off += u.length;
    });
    return buf;
  }

  it("decodes rows, u64 sizes, times and UTF-16 names", () => {
    const p = decodeRowPage(page(["node_modules", "😀 photos"]));
    expect(p).toMatchObject({ parent: 7, total: 1000, offset: 40 });
    expect(p.rows).toHaveLength(3);
    const r = p.rows[0];
    expect(r).toMatchObject({ id: 100, name: "node_modules", isDir: true, category: 6, safety: "probably", items: 9 });
    expect(r?.allocated).toBe(5 + 2 ** 32);
    expect(r?.modifiedMs).toBe(Date.UTC(2000, 0, 2));
    expect(r?.createdMs).toBeNull();
    expect(p.rows[1]?.name).toBe("😀 photos");
    expect(p.rows[2]?.name).toBe("�A");
  });

  it("rejects malformed pages", () => {
    expect(() => decodeRowPage(new ArrayBuffer(8))).toThrow(RowPageError);
    const bad = page(["a"]);
    new DataView(bad).setUint32(28, 9999, true);
    expect(() => decodeRowPage(bad)).toThrow(/out of bounds/);
  });
});

describe("fuzzy matching", () => {
  it("matches subsequences and prefers word starts and runs", () => {
    expect(fuzzyMatch("scd", "Scan D:")?.positions).toEqual([0, 1, 5]);
    expect(fuzzyMatch("xyz", "Scan D:")).toBeNull();
    const a = fuzzyMatch("tree", "Show Treemap")?.score ?? 0;
    const b = fuzzyMatch("tree", "Toggle reference")?.score ?? 0;
    expect(a).toBeGreaterThan(b);
    expect(fuzzyMatch("", "anything")?.score).toBe(0);
  });
});

describe("command bus", () => {
  const ui = (): UiActions & { calls: string[] } => {
    const calls: string[] = [];
    return {
      calls,
      showInList: () => calls.push("list"),
      explain: () => calls.push("explain"),
      exclude: () => calls.push("exclude"),
      notify: (m) => calls.push(`notify:${m}`),
    };
  };
  const target = { volumeId: "v", ids: [1] };

  it("disables backend actions with a reason until the backend reports them", () => {
    const u = ui();
    const bus = new CommandBus({ capabilities: () => new Set(), ui: u, writeClipboard: () => Promise.resolve() });
    for (const a of ENTRY_ACTIONS) {
      const av = bus.availability({ type: a.type, target });
      expect(av.enabled).toBe(a.requires === null);
      if (!av.enabled) expect(av.reason.length).toBeGreaterThan(5);
    }
    const withCaps = new CommandBus({ capabilities: () => new Set(["entry_action", "entry_path", "cleanup_queue_add"]), ui: u, writeClipboard: () => Promise.resolve() });
    for (const a of ENTRY_ACTIONS) expect(withCaps.availability({ type: a.type, target }).enabled).toBe(true);
    expect(withCaps.availability({ type: "properties", target: { volumeId: "v", ids: [1, 2] } }).enabled).toBe(false);
    expect(withCaps.availability({ type: "open", target: { volumeId: "v", ids: [] } }).enabled).toBe(false);
  });

  it("routes UI actions and refuses unavailable ones", async () => {
    const u = ui();
    const bus = new CommandBus({ capabilities: () => new Set(), ui: u, writeClipboard: () => Promise.resolve() });
    await bus.dispatch({ type: "showInList", target });
    await bus.dispatch({ type: "explain", target });
    await bus.dispatch({ type: "excludeFromView", target });
    await bus.dispatch({ type: "open", target });
    expect(u.calls.slice(0, 3)).toEqual(["list", "explain", "exclude"]);
    expect(u.calls[3]).toMatch(/^notify:Shell integration/);
  });

  it("explains backend failures", async () => {
    const u = ui();
    const bus = new CommandBus({ capabilities: () => new Set(["entry_path"]), ui: u, writeClipboard: () => Promise.resolve() });
    await bus.dispatch({ type: "copyPath", target });
    expect(u.calls[0]).toMatch(/^notify:Copy path failed/);
    expect(new BackendUnavailableError("x", "why").reason).toBe("why");
  });
});

describe("layout stream", () => {
  class TestStream extends BaseLayoutStream {
    request(): number {
      return 0;
    }
    close(): void {
      // Nothing to release.
    }
    push(buf: ArrayBuffer): void {
      this.deliver(buf);
    }
  }

  it("drops stale frames and reports bad ones", () => {
    const s = new TestStream();
    const frames: number[] = [];
    const errors: unknown[] = [];
    s.subscribe((f) => frames.push(f.seq));
    s.onError((e) => errors.push(e));
    const a = fixtureFrame("treemap").nodes.bytes.buffer.slice(0) as ArrayBuffer;
    const b = fixtureFrame("treemap-drill").nodes.bytes.buffer.slice(0) as ArrayBuffer;
    s.push(b);
    s.push(a);
    s.push(new ArrayBuffer(4));
    expect(frames).toEqual([2]);
    expect(errors).toHaveLength(1);
  });
});
