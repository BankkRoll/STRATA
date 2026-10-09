/**
 * Keyboard navigation over a layout's hierarchy (the visual views' tree
 * model): Left/Right move between siblings, Down enters the first child, Up
 * goes to the parent, Home/End jump to the first/last sibling. Aggregates are
 * skipped; they are not real entries.
 */
import { NO_INDEX, NodeFlag, type NodeBuffer } from "./frame";

/** Keys {@link navigate} understands. */
export type NavKey = "ArrowLeft" | "ArrowRight" | "ArrowUp" | "ArrowDown" | "Home" | "End";

/** Whether `key` is a {@link NavKey}. */
export function isNavKey(key: string): key is NavKey {
  return key === "ArrowLeft" || key === "ArrowRight" || key === "ArrowUp" || key === "ArrowDown" || key === "Home" || key === "End";
}

function selectable(nodes: NodeBuffer, i: number): boolean {
  return (nodes.flags(i) & NodeFlag.SELECTABLE) !== 0;
}

/** Selectable direct children of record `p`, in pre-order. */
export function childrenOf(nodes: NodeBuffer, subtreeEnd: Uint32Array, p: number): number[] {
  const out: number[] = [];
  const end = subtreeEnd[p] ?? p + 1;
  for (let c = p + 1; c < end; c = subtreeEnd[c] ?? end) if (selectable(nodes, c)) out.push(c);
  return out;
}

/**
 * Moves keyboard focus.
 *
 * @param nodes - Node buffer.
 * @param subtreeEnd - Skip table.
 * @param index - Focused record, or -1 when nothing is focused.
 * @param key - Navigation key.
 * @returns The new focused record (unchanged when the move is impossible).
 */
export function navigate(nodes: NodeBuffer, subtreeEnd: Uint32Array, index: number, key: NavKey): number {
  if (nodes.count === 0) return -1;
  if (index < 0 || index >= nodes.count) {
    return childrenOf(nodes, subtreeEnd, 0)[0] ?? 0;
  }
  if (key === "ArrowDown") return childrenOf(nodes, subtreeEnd, index)[0] ?? index;
  const parent = nodes.parent(index);
  if (key === "ArrowUp") return parent === NO_INDEX ? index : parent;
  if (parent === NO_INDEX) return index;
  const sibs = childrenOf(nodes, subtreeEnd, parent);
  const at = sibs.indexOf(index);
  switch (key) {
    case "ArrowRight":
      return sibs[at + 1] ?? index;
    case "ArrowLeft":
      return at > 0 ? (sibs[at - 1] ?? index) : index;
    case "Home":
      return sibs[0] ?? index;
    case "End":
      return sibs[sibs.length - 1] ?? index;
  }
}

/**
 * Finds the record of entry `id` (first selectable match), or -1.
 *
 * @param nodes - Node buffer.
 * @param id - Entry id.
 * @returns Record index.
 */
export function recordOf(nodes: NodeBuffer, id: number): number {
  const u32 = nodes.u32;
  for (let i = 0; i < nodes.count; i++) {
    if (u32[i * 8 + 4] === id && selectable(nodes, i)) return i;
  }
  return -1;
}
