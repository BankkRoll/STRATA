/// <reference types="node" />
/**
 * Test-only access to the layout fixtures exported by
 * `crates/strata-layout/examples/export_fixtures.rs` (`pnpm fixtures`).
 */
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { decodeFrame, type LayoutFrame } from "../lib/layout/frame";

// Vitest runs with the `ui/` package as its working directory.
const DIR = join(process.cwd(), "src/lib/layout/__fixtures__");

/** Reads a fixture file into a fresh, aligned `ArrayBuffer`. */
export function fixtureBytes(name: string): ArrayBuffer {
  const buf = readFileSync(join(DIR, name));
  const out = new ArrayBuffer(buf.byteLength);
  new Uint8Array(out).set(buf);
  return out;
}

/** Decodes `<name>.frame.bin`. */
export function fixtureFrame(name: string): LayoutFrame {
  return decodeFrame(fixtureBytes(`${name}.frame.bin`));
}

/** One Rust pick sample. */
export interface RustPickSample {
  x: number;
  y: number;
  pick: { index: number; id: number; flags: number; ancestors: number[] } | null;
}

/** Rust picking results and skip table for a fixture. */
export interface RustPicks {
  subtreeEnd: (number | null)[];
  picks: RustPickSample[];
}

/** Reads `<name>.picks.json`. */
export function fixturePicks(name: string): RustPicks {
  return JSON.parse(readFileSync(join(DIR, `${name}.picks.json`), "utf8")) as RustPicks;
}

/** One node of the synthetic fixture tree (`tree.bin`). */
export interface FixtureNode {
  size: number;
  parent: number;
  colorKey: number;
  dir: boolean;
}

/** Decodes `tree.bin` (16 bytes per node: size u64, parent u32, info u32). */
export function fixtureTree(): FixtureNode[] {
  const v = new DataView(fixtureBytes("tree.bin"));
  const out: FixtureNode[] = [];
  for (let o = 0; o < v.byteLength; o += 16) {
    const info = v.getUint32(o + 12, true);
    out.push({
      size: v.getUint32(o, true) + v.getUint32(o + 4, true) * 2 ** 32,
      parent: v.getUint32(o + 8, true),
      colorKey: info & 0x7fffffff,
      dir: info >>> 31 === 1,
    });
  }
  return out;
}
