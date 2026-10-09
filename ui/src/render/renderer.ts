/**
 * The GPU boundary. Everything above this interface (controller, picking,
 * labels, React) is testable in jsdom; everything below needs WebGL2 and is
 * exercised in the dev harness (`?fixture=`), which also checks that every
 * shader compiles.
 */
import type { LayoutFrame, ViewTransform } from "../lib/layout/frame";

/** Per-draw inputs, all cheap uniforms. */
export interface DrawState {
  /** Drawing-buffer size in device pixels. */
  width: number;
  height: number;
  /** Device pixel ratio (pattern and border scale). */
  dpr: number;
  /** Visual zoom relative to the frame's own coordinates. */
  view: ViewTransform;
  /** Shader color mode index (`COLOR_MODE_INDEX`). */
  colorMode: number;
  /** Category patterns for color-blind users. */
  patterns: boolean;
  /** Dark theme. */
  dark: boolean;
  /** Cushion shading (treemap; needs the frame's cushion section). */
  cushion: boolean;
  /** Hovered record index, or -1. */
  hover: number;
  /** Changed-recently pulse, 0–1. */
  pulse: number;
  /** Selection/focus accent color, linear 0–1 RGB. */
  accent: readonly [number, number, number];
  /** Background clear color, 0–1 RGB. */
  background: readonly [number, number, number];
  /** `performance.now()` of this frame. */
  now: number;
  /** Skip animations (`prefers-reduced-motion`). */
  reducedMotion: boolean;
}

/** One view's GPU renderer. */
export interface ViewRenderer {
  /** Uploads a frame's buffers as-is. `animate` plays its transition section. */
  setFrame(frame: LayoutFrame, animate: boolean): void;
  /** Marks records whose id is in `ids` as selected (one byte per instance). */
  setSelection(ids: ReadonlySet<number>): void;
  /** Rebuilds the palette texture for the theme. */
  setTheme(dark: boolean): void;
  /** Draws; returns `true` while an animation needs more frames. */
  draw(state: DrawState): boolean;
  /** Recreates every GL resource after `webglcontextrestored`. */
  restore(): void;
  /** Frees GL resources. */
  dispose(): void;
}

/** Transition length for drill-down / go-up zooms. */
export const TRANSITION_MS = 420;

/** Ease-in-out cubic: slow start and finish so the zoom reads as camera motion. */
export function easeInOut(t: number): number {
  const x = Math.min(1, Math.max(0, t));
  return x < 0.5 ? 4 * x * x * x : 1 - (-2 * x + 2) ** 3 / 2;
}
