/**
 * DEV ONLY. Fetches the exported layout fixtures for the `?fixture=` harness.
 *
 * Kept apart from `fixtureServices.ts` because the globs below turn every
 * fixture file into a build asset; the website demo reuses the services but
 * loads only `tree.bin`.
 */
import { decodeFrame } from "../lib/layout/frame";
import { decodeTree, type FixtureData } from "./fixtureServices";

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

/**
 * Loads the fixture set.
 *
 * @param large - Use the 1M-node treemap (`pnpm fixtures:large`).
 * @returns Decoded tree and raw frames.
 */
export async function loadFixtures(large: boolean): Promise<FixtureData> {
  const treeUrl = urlOf(large ? "tree-1m.bin" : "tree.bin", large);
  if (!treeUrl) throw new Error(large ? "Large fixtures missing: run `pnpm --dir ui fixtures:large`." : "Fixtures missing: run `pnpm --dir ui fixtures`.");
  const { nodes, children } = decodeTree(await fetchBytes(treeUrl));
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
