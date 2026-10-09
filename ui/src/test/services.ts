/**
 * Test services: the dev fixture services over the exported fixture tree and
 * frames, read from disk, with WebGL replaced by a recording fake.
 */
import type { FixtureData } from "../dev/fixtureServices";
import { fixtureServices } from "../dev/fixtureServices";
import type { LayoutFrame } from "../lib/layout/frame";
import type { DrawState, ViewRenderer } from "../render/renderer";
import type { Services } from "../services";
import { useApp, INITIAL_APP_STATE } from "../store/app";
import { useVolumes } from "../store/volumes";
import { fixtureBytes, fixtureFrame, fixtureTree } from "./fixtures";

/** Loads the small fixture set synchronously from disk. */
export function loadTestFixtures(): FixtureData {
  const tree = fixtureTree();
  const nodes = tree.map((n) => ({ size: n.size, parent: n.parent, key: n.colorKey, dir: n.dir }));
  const children: number[][] = nodes.map(() => []);
  for (let i = 1; i < nodes.length; i++) children[nodes[i]?.parent ?? 0]?.push(i);
  const frames = new Map<string, ArrayBuffer>();
  for (const n of ["treemap", "treemap-drill", "icicle", "flame", "sunburst", "bubbles", "mindmap"]) {
    frames.set(n, fixtureBytes(`${n}.frame.bin`));
  }
  return { large: false, nodes, children, frames, drillRoot: fixtureFrame("treemap-drill").root };
}

/** A renderer that records calls instead of drawing. */
export class FakeRenderer implements ViewRenderer {
  frames: LayoutFrame[] = [];
  selections: ReadonlySet<number>[] = [];
  draws: DrawState[] = [];
  disposed = false;
  setFrame(frame: LayoutFrame): void {
    this.frames.push(frame);
  }
  setSelection(ids: ReadonlySet<number>): void {
    this.selections.push(ids);
  }
  setTheme(): void {
    // Theme has no observable effect without a GPU.
  }
  draw(state: DrawState): boolean {
    this.draws.push(state);
    return false;
  }
  restore(): void {
    // Nothing to recreate.
  }
  dispose(): void {
    this.disposed = true;
  }
}

/**
 * Fixture-backed services with injectable overrides.
 *
 * @param overrides - Fields to replace.
 * @returns Services for a component test.
 */
export function testServices(overrides: Partial<Services> = {}): Services {
  const base = fixtureServices(loadTestFixtures());
  return { ...base, createRenderer: () => new FakeRenderer(), ...overrides };
}

/** Resets the shared stores between tests. */
export function resetStores(): void {
  useApp.setState({ ...INITIAL_APP_STATE });
  useVolumes.setState({ volumes: null, helper: null, error: null, unavailable: false });
}
