/**
 * DEV ONLY. Services backed by the layout fixtures exported from
 * `strata-layout` (`VecTree::synthetic`), for the `?fixture=` harness. Never
 * imported by production code paths: `main.tsx` reaches this module only
 * behind `import.meta.env.DEV`.
 *
 * Names are synthetic ("folder #12") because the fixture tree has none.
 */
import { createStoreBus, type Services } from "../services";
import type { EntryDetail } from "../lib/detail";
import { BatchedEntryInfoProvider, type EntryInfo, type EntryInfoProvider } from "../lib/entries";
import { decodeFrame, ViewKind } from "../lib/layout/frame";
import { BaseLayoutStream, type LayoutRequest } from "../lib/layout/stream";
import { decodeColorKey } from "../lib/palette";
import type { Row, RowPage, RowQuery } from "../lib/rows";
import type { SearchBatch, SearchResult, SearchStream } from "../lib/search";
import type { Safety } from "../lib/types";
import type { VolumeInfo } from "../lib/volumes";
import { createRenderer } from "../render/registry";

const smallUrls = import.meta.glob<string>("../lib/layout/__fixtures__/*.bin", { query: "?url", import: "default", eager: true });
const largeUrls = import.meta.glob<string>("./__fixtures__/large/*.bin", { query: "?url", import: "default", eager: true });

async function fetchBytes(url: string): Promise<ArrayBuffer> {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`fixture ${url}: HTTP ${r.status}`);
  return r.arrayBuffer();
}

function urlOf(name: string, large: boolean): string | undefined {
  return large ? largeUrls[`./__fixtures__/large/${name}`] : smallUrls[`../lib/layout/__fixtures__/${name}`];
}

/** One node of the fixture tree. */
interface Node {
  size: number;
  parent: number;
  key: number;
  dir: boolean;
}

const SAFETY: readonly (Safety | null)[] = [null, "safe", "probably", "careful", "never"];
const NOW = Date.now();

/** Loaded fixture data. */
export interface FixtureData {
  large: boolean;
  nodes: Node[];
  children: number[][];
  frames: Map<string, ArrayBuffer>;
  drillRoot: number;
}

/**
 * Loads the fixture set.
 *
 * @param large - Use the 1M-node treemap (`pnpm fixtures:large`).
 * @returns Decoded tree and raw frames.
 */
export async function loadFixtures(large: boolean): Promise<FixtureData> {
  const treeUrl = urlOf(large ? "tree-1m.bin" : "tree.bin", large);
  if (!treeUrl) throw new Error(large ? "Large fixtures missing: run `pnpm --dir ui fixtures:large`." : "Fixtures missing: run `pnpm --dir ui fixtures`.");
  const tree = new DataView(await fetchBytes(treeUrl));
  const nodes: Node[] = [];
  for (let o = 0; o < tree.byteLength; o += 16) {
    const info = tree.getUint32(o + 12, true);
    nodes.push({
      size: tree.getUint32(o, true) + tree.getUint32(o + 4, true) * 2 ** 32,
      parent: tree.getUint32(o + 8, true),
      key: info & 0x7fffffff,
      dir: info >>> 31 === 1,
    });
  }
  const children: number[][] = nodes.map(() => []);
  for (let i = 1; i < nodes.length; i++) children[nodes[i]?.parent ?? 0]?.push(i);
  const frames = new Map<string, ArrayBuffer>();
  const names = large
    ? ["treemap-1m"]
    : ["treemap", "treemap-drill", "icicle", "flame", "sunburst", "bubbles", "mindmap"];
  await Promise.all(
    names.map(async (n) => {
      const u = urlOf(`${n}.frame.bin`, large);
      if (u) frames.set(n, await fetchBytes(u));
    }),
  );
  const drill = frames.get("treemap-drill");
  return { large, nodes, children, frames, drillRoot: drill ? decodeFrame(drill.slice(0)).root : -1 };
}

/** Replays fixture frames for layout requests. */
export class FixtureLayoutStream extends BaseLayoutStream {
  private seq = 0;

  constructor(private readonly data: FixtureData) {
    super();
  }

  request(req: LayoutRequest): number {
    const seq = ++this.seq;
    let name: string = req.view;
    if (req.view === "treemap") {
      if (this.data.large) name = "treemap-1m";
      else name = req.root === this.data.drillRoot ? "treemap-drill" : "treemap";
    }
    const src = this.data.frames.get(name);
    queueMicrotask(() => {
      if (!src) {
        this.fail(new Error(`no fixture frame for ${name}`));
        return;
      }
      const copy = src.slice(0);
      const v = new DataView(copy);
      v.setUint32(8, seq, true);
      // Only animate when the request asked for it, like the backend.
      if (!req.animate) {
        v.setUint32(108, 0, true);
      }
      this.deliver(copy);
    });
    return seq;
  }

  close(): void {
    // Nothing to release.
  }
}

function nameOf(n: Node, id: number): string {
  return id === 0 ? "Fixture root" : `${n.dir ? "folder" : "file"} #${id}`;
}

function countBelow(data: FixtureData, id: number): number {
  let total = 0;
  const stack = [id];
  while (stack.length > 0) {
    const c = data.children[stack.pop() ?? 0] ?? [];
    total += c.length;
    stack.push(...c);
  }
  return total;
}

function info(data: FixtureData, id: number): EntryInfo {
  const n = data.nodes[id] ?? { size: 0, parent: 0, key: 0, dir: false };
  const k = decodeColorKey(n.key);
  return {
    id,
    name: nameOf(n, id),
    isDir: n.dir,
    allocated: n.size,
    logical: Math.round(n.size * 0.97),
    items: n.dir && !data.large ? countBelow(data, id) : (data.children[id]?.length ?? 0),
    category: k.category,
    app: k.app === 0 ? null : `App ${k.app}`,
    safety: SAFETY[k.safety] ?? null,
    modifiedMs: NOW - k.age * 9 * 86_400_000,
    suspiciousTime: false,
  };
}

function row(data: FixtureData, id: number): Row {
  const i = info(data, id);
  const n = data.nodes[id];
  return {
    id,
    parent: n?.parent ?? 0,
    name: i.name,
    flags: i.isDir ? 1 : 0,
    isDir: i.isDir,
    category: i.category,
    safety: i.safety,
    allocated: i.allocated,
    logical: i.logical,
    items: i.items,
    childCount: data.children[id]?.length ?? 0,
    modifiedMs: i.modifiedMs,
    createdMs: i.modifiedMs === null ? null : i.modifiedMs - 86_400_000 * 30,
    accessedMs: i.modifiedMs,
    appId: decodeColorKey(n?.key ?? 0).app,
  };
}

class FixtureSearch implements SearchStream {
  private readonly listeners = new Set<(b: SearchBatch) => void>();
  private seq = 0;
  constructor(private readonly data: FixtureData) {}
  query(q: { text: string }): number {
    const seq = ++this.seq;
    const text = q.text.toLowerCase();
    const results: SearchResult[] = [];
    for (let id = 1; id < this.data.nodes.length && results.length < 200; id++) {
      const n = this.data.nodes[id];
      if (!n || !nameOf(n, id).toLowerCase().includes(text)) continue;
      results.push({ volumeId: "fixture", id, name: nameOf(n, id), parentPath: `Fixture root\\…\\#${n.parent}`, isDir: n.dir, allocated: n.size, logical: n.size });
    }
    queueMicrotask(() => {
      for (const l of this.listeners) l({ seq, results, done: true, total: results.length });
    });
    return seq;
  }
  subscribe(l: (b: SearchBatch) => void): () => void {
    this.listeners.add(l);
    return () => this.listeners.delete(l);
  }
  onError(): () => void {
    return () => undefined;
  }
  close(): void {
    this.listeners.clear();
  }
}

/**
 * Builds {@link Services} over loaded fixtures.
 *
 * @param data - Loaded fixtures.
 * @returns Fixture services (no backend capabilities, so actions show as unavailable).
 */
export function fixtureServices(data: FixtureData): Services {
  let provider: EntryInfoProvider | null = null;
  const caps = new Set<string>();
  const total = data.nodes[0]?.size ?? 0;
  const categoryBytes: Record<string, number> = {};
  for (let id = 1; id < data.nodes.length; id++) {
    const n = data.nodes[id];
    if (!n || n.dir) continue;
    const c = String(n.key & 0xf);
    categoryBytes[c] = (categoryBytes[c] ?? 0) + n.size;
  }
  const volume: VolumeInfo = {
    id: "fixture",
    mountPoints: [],
    label: data.large ? "Synthetic 1M fixture" : "Synthetic fixture",
    filesystem: "NTFS",
    devDrive: false,
    kind: "fixed",
    isSystem: false,
    totalBytes: Math.ceil(total * 1.25),
    freeBytes: Math.ceil(total * 0.2),
    clusterSize: 4096,
    serial: "0000-0000",
    bitlocker: "none",
    present: true,
    scan: { state: "live", progress: null, lastScanMs: NOW - 3_600_000, scanner: "mft", rootId: 0 },
    categoryBytes,
  };
  return {
    createLayoutStream: () => new FixtureLayoutStream(data),
    entryInfo: () => {
      provider ??= new BatchedEntryInfoProvider("fixture", (_v, ids) => Promise.resolve(ids.map((id) => info(data, id))));
      return provider;
    },
    rows: {
      fetchChildren(q: RowQuery): Promise<RowPage> {
        const kids = (data.children[q.parent] ?? []).slice();
        const dir = q.sort.desc ? -1 : 1;
        kids.sort((a, b) => {
          if (q.sort.key === "name") return dir * nameOf(data.nodes[a] as Node, a).localeCompare(nameOf(data.nodes[b] as Node, b));
          return dir * ((data.nodes[a]?.size ?? 0) - (data.nodes[b]?.size ?? 0)) || a - b;
        });
        const page = kids.slice(q.offset, q.offset + q.limit).map((id) => row(data, id));
        return Promise.resolve({ parent: q.parent, total: kids.length, offset: q.offset, rows: page });
      },
    },
    fetchDetail(_v, id): Promise<EntryDetail> {
      const i = info(data, id);
      const k = decodeColorKey(data.nodes[id]?.key ?? 0);
      return Promise.resolve({
        id,
        volumeId: "fixture",
        name: i.name,
        path: `Fixture root\\…\\${i.name}`,
        isDir: i.isDir,
        iconDataUrl: null,
        sizes: { logical: i.logical, allocated: i.allocated, adsLogical: 0, adsAllocated: 0, dirOverhead: i.isDir ? 4096 : 0, compressionRatio: i.logical > 0 ? i.allocated / i.logical : null, estimated: false },
        counts: i.isDir ? { files: i.items, dirs: data.children[id]?.filter((c) => data.nodes[c]?.dir).length ?? 0 } : null,
        times: {
          created: { ms: i.modifiedMs, suspicious: false },
          modified: { ms: i.modifiedMs, suspicious: false },
          accessed: { ms: i.modifiedMs, suspicious: false },
          mftChanged: { ms: i.modifiedMs, suspicious: false },
          fileNameCreated: null,
          accessUnreliable: true,
        },
        flags: i.isDir ? 1 : 0,
        reparse: null,
        cloud: null,
        hardlinks: null,
        streams: [],
        detectedType: null,
        classification: { category: k.category, ruleId: "fixture.synthetic", ruleName: "Synthetic fixture rule", explain: "Generated by VecTree::synthetic for UI development." },
        attribution: i.app ? { app: i.app, confidence: "heuristic", evidence: ["Fixture data"] } : null,
        safety: i.safety ? { tier: i.safety, why: "Fixture tier", regenerable: i.safety === "safe" } : null,
        lastWriter: null,
        history: i.isDir ? Array.from({ length: 12 }, (_, w) => ({ atMs: NOW - (11 - w) * 7 * 86_400_000, allocated: i.allocated * (0.7 + 0.3 * Math.sin(w / 2) ** 2) })) : null,
        partial: false,
      });
    },
    volumes: {
      list: () => Promise.resolve([volume]),
      watch: () => () => undefined,
      helperStatus: () => Promise.resolve({ elevated: false, mode: "none" }),
      sinceLastScan: () => Promise.resolve({ deltaBytes: Math.round(total * 0.03), sinceMs: NOW - 5 * 86_400_000, biggest: { path: "Fixture root\\folder #1", deltaBytes: Math.round(total * 0.02) } }),
      startScan: () => Promise.reject(new Error("The fixture harness cannot scan.")),
      cancelScan: () => Promise.resolve(null),
      elevate: () => Promise.reject(new Error("The fixture harness has no helper.")),
    },
    createSearchStream: () => new FixtureSearch(data),
    appName: (id) => `App ${id}`,
    capabilities: () => caps,
    bus: createStoreBus(() => caps),
    createRenderer,
  };
}

/** View kinds with a fixture (for the shader check). */
export const FIXTURE_KINDS: readonly ViewKind[] = [ViewKind.Treemap, ViewKind.Sunburst, ViewKind.Bubbles, ViewKind.MindMap];
