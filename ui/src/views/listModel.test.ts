// @vitest-environment node
import { describe, expect, it } from "vitest";
import type { Row } from "../lib/rows";
import { CollisionGrid, ellipsize } from "../render/labels";
import { COLUMNS, PAGE_SIZE, flatten, missingPages, serializeRows, typeAhead, type CellContext, type ChildCache } from "./listModel";

function row(id: number, parent: number, name: string, isDir = false): Row {
  return {
    id,
    parent,
    name,
    flags: isDir ? 1 : 0,
    isDir,
    category: 6,
    safety: "safe",
    allocated: id * 1024,
    logical: id * 1000,
    items: isDir ? 3 : 0,
    childCount: isDir ? 3 : 0,
    modifiedMs: Date.UTC(2025, 0, 1),
    createdMs: null,
    accessedMs: null,
    appId: 0,
  };
}

function cache(rows: (Row | undefined)[], total = rows.length): ChildCache {
  const r = rows.slice();
  r.length = total;
  return { total, rows: r, pages: new Set([0]), error: null, loaded: true };
}

describe("flatten", () => {
  const caches = new Map<number, ChildCache>([
    [0, cache([row(1, 0, "Alpha", true), row(2, 0, "beta"), row(3, 0, "Gamma", true)])],
    [1, cache([row(4, 1, "inner", true), row(5, 1, "leaf")])],
    [4, cache([row(6, 4, "deep")])],
  ]);

  it("shows only expanded folders' children, depth first, with ARIA positions", () => {
    expect(flatten(0, caches, new Set()).map((f) => f.row?.id)).toEqual([1, 2, 3]);
    const flat = flatten(0, caches, new Set([1, 4, 3]));
    expect(flat.map((f) => f.row?.id ?? null)).toEqual([1, 4, 6, 5, 2, 3]);
    expect(flat.map((f) => f.level)).toEqual([1, 2, 3, 2, 1, 1]);
    expect(flat[3]).toMatchObject({ pos: 1, setSize: 2, parent: 1 });
  });

  it("emits placeholders for unloaded pages and lists the pages to fetch", () => {
    const big = new Map<number, ChildCache>([[0, cache([row(1, 0, "a")], 1000)]]);
    const flat = flatten(0, big, new Set());
    expect(flat).toHaveLength(1000);
    expect(flat[500]?.row).toBeNull();
    expect(missingPages(flat, 0, 10, big)).toEqual([]);
    expect(missingPages(flat, 190, 450, big)).toEqual([
      [0, 1],
      [0, 2],
    ]);
    expect(PAGE_SIZE).toBe(200);
  });

  it("finds rows by typed prefix, wrapping", () => {
    const flat = flatten(0, caches, new Set());
    expect(typeAhead(flat, 0, "g")).toBe(2);
    expect(typeAhead(flat, 2, "a")).toBe(0);
    expect(typeAhead(flat, 0, "zz")).toBe(-1);
  });
});

describe("export", () => {
  const ctx: CellContext = { units: "binary", sizeMode: "allocated", appName: () => null, pathOf: (r) => `root\\${r.name}`, now: Date.UTC(2026, 0, 1) };
  const cols = COLUMNS.filter((c) => ["name", "allocated", "percent", "path"].includes(c.id));

  it("writes TSV and quoted CSV with exact bytes", () => {
    const rows = [row(2, 0, 'say "hi", ok'), row(3, 0, "tab\there")];
    expect(serializeRows(rows, cols, ctx, "csv").split("\r\n")).toEqual([
      "Name,On disk (bytes),Path",
      '"say ""hi"", ok",2048,"root\\say ""hi"", ok"',
      "tab\there,3072,root\\tab\there",
    ]);
    expect(serializeRows(rows, cols, ctx, "tsv").split("\r\n")[2]).toBe("tab here\t3072\troot\\tab here");
  });
});

describe("labels", () => {
  const measure = (t: string) => Array.from(t).length * 10;

  it("ellipsizes to the box without splitting surrogate pairs", () => {
    expect(ellipsize("short", 100, measure)).toBe("short");
    expect(ellipsize("abcdefghij", 50, measure)).toBe("abcd…");
    expect(ellipsize("😀😀😀😀😀😀", 40, measure)).toBe("😀😀😀…");
    expect(ellipsize("abc", 5, measure)).toBe("");
    expect(ellipsize("abc", 10, measure)).toBe("…");
  });

  it("culls overlapping labels", () => {
    const g = new CollisionGrid(64);
    expect(g.tryPlace({ x: 0, y: 0, w: 100, h: 14 })).toBe(true);
    expect(g.tryPlace({ x: 50, y: 5, w: 100, h: 14 })).toBe(false);
    expect(g.tryPlace({ x: 0, y: 14, w: 100, h: 14 })).toBe(true);
    expect(g.tryPlace({ x: 300, y: 300, w: 10, h: 10 })).toBe(true);
  });
});
