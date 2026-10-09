/**
 * Hover state for tooltips, outside React's render cycle.
 *
 * The renderer writes here on pointer moves; only the tooltip subscribes, and
 * it re-renders only when the hovered record changes, so moving the mouse
 * across one block costs no React work at all.
 */
import { createStore } from "zustand/vanilla";

/** What the pointer is over. */
export interface HoverTarget {
  /** Entry id (directory id for aggregates). */
  id: number;
  /** Record index in the current frame. */
  index: number;
  /** Node flags. */
  flags: number;
  /** For aggregates: folded child count and bytes. */
  aggregate: { count: number; bytes: number } | null;
  /** Layout root size, for the percentage. */
  rootBytes: number;
}

/** Hover store shape. */
export interface HoverState {
  target: HoverTarget | null;
  /** Pointer position in CSS px relative to the viewport. */
  clientX: number;
  clientY: number;
}

/** The hover store. */
export const hoverStore = createStore<HoverState>()(() => ({ target: null, clientX: 0, clientY: 0 }));
