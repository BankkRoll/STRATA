/**
 * Shared view state: what is shown, how, and what is selected.
 *
 * Every view reads the same root, selection, size mode, color mode and
 * filters, so switching views keeps context. Per-frame state
 * (hover, animation) deliberately lives outside React; see `store/hover.ts`.
 */
import { create } from "zustand";
import type { ColorMode } from "../lib/palette";
import { NO_FILTERS, type SizeMode, type TreemapStyle, type ViewFilters, type ViewId, type VisualView } from "../lib/types";

/** Collapsible panes. */
export type Pane = "nav" | "list" | "detail";

/** Shared app state and its actions. */
export interface AppState {
  /** Volume being explored, or `null` on the home screen before any scan. */
  volumeId: string | null;
  /**
   * Entry ids from the volume root to the current root (breadcrumbs). The
   * last element is the layout root.
   */
  path: number[];
  /** Selected entry ids (multi-select via Ctrl/Shift or marquee). */
  selection: number[];
  /** The focused selected entry the detail panel shows. */
  primary: number | null;
  sizeMode: SizeMode;
  colorMode: ColorMode;
  treemapStyle: TreemapStyle;
  filters: ViewFilters;
  /** Active view. */
  view: ViewId;
  /** Last visual view, restored when a volume opens. */
  lastVisual: VisualView;
  /** Pane visibility. */
  panes: Record<Pane, boolean>;
  /** Latest message for the status bar's polite live region. */
  status: string;

  openVolume: (volumeId: string, rootId: number) => void;
  /** Makes `chain` (ids below the current root, ending at the new root) the new root. */
  drillTo: (chain: number[]) => void;
  /** Goes to the parent root; returns false at the volume root. */
  goUp: () => boolean;
  /** Jumps to breadcrumb `index`. */
  jumpTo: (index: number) => void;
  /** Replaces the selection. */
  select: (ids: number[], primary?: number | null) => void;
  /** Adds or removes one id (Ctrl+click). */
  toggleSelect: (id: number) => void;
  clearSelection: () => void;
  setSizeMode: (mode: SizeMode) => void;
  setColorMode: (mode: ColorMode) => void;
  setTreemapStyle: (style: TreemapStyle) => void;
  setFilters: (filters: ViewFilters) => void;
  /** Hides entries via "Exclude from view". */
  exclude: (ids: number[]) => void;
  setView: (view: ViewId) => void;
  setPane: (pane: Pane, open: boolean) => void;
  togglePane: (pane: Pane) => void;
  notify: (message: string) => void;
}

const VISUAL: ReadonlySet<ViewId> = new Set(["treemap", "sunburst", "icicle", "flame", "bubbles", "mindmap"]);

/** Whether a view id is a visual (layout-backed) view. */
export function isVisualView(view: ViewId): view is VisualView {
  return VISUAL.has(view);
}

/** Initial state, exported so tests can reset the store. */
export const INITIAL_APP_STATE = {
  volumeId: null,
  path: [],
  selection: [],
  primary: null,
  sizeMode: "allocated",
  colorMode: "category",
  treemapStyle: "flat",
  filters: NO_FILTERS,
  view: "home",
  lastVisual: "treemap",
  panes: { nav: true, list: true, detail: true },
  status: "",
} satisfies Partial<AppState>;

/** The shared app store. */
export const useApp = create<AppState>()((set, get) => ({
  ...INITIAL_APP_STATE,

  openVolume(volumeId, rootId) {
    set((s) => ({
      volumeId,
      path: [rootId],
      selection: [],
      primary: null,
      filters: NO_FILTERS,
      view: s.lastVisual,
    }));
  },

  drillTo(chain) {
    if (chain.length === 0) return;
    const last = chain[chain.length - 1] as number;
    set((s) => ({ path: [...s.path, ...chain], selection: [last], primary: last }));
  },

  goUp() {
    const { path } = get();
    if (path.length <= 1) return false;
    const leaving = path[path.length - 1] as number;
    // Selecting the folder we came out of keeps the user's place.
    set({ path: path.slice(0, -1), selection: [leaving], primary: leaving });
    return true;
  },

  jumpTo(index) {
    const { path } = get();
    if (index < 0 || index >= path.length - 1) return;
    const leaving = path[index + 1] as number;
    set({ path: path.slice(0, index + 1), selection: [leaving], primary: leaving });
  },

  select(ids, primary) {
    set({ selection: ids, primary: primary === undefined ? (ids[ids.length - 1] ?? null) : primary });
  },

  toggleSelect(id) {
    set((s) => {
      const has = s.selection.includes(id);
      const selection = has ? s.selection.filter((x) => x !== id) : [...s.selection, id];
      return { selection, primary: has ? (selection[selection.length - 1] ?? null) : id };
    });
  },

  clearSelection() {
    set({ selection: [], primary: null });
  },

  setSizeMode(sizeMode) {
    set({ sizeMode });
  },

  setColorMode(colorMode) {
    set({ colorMode });
  },

  setTreemapStyle(treemapStyle) {
    set({ treemapStyle });
  },

  setFilters(filters) {
    set({ filters });
  },

  exclude(ids) {
    set((s) => {
      const excluded = [...new Set([...s.filters.excluded, ...ids])];
      return {
        filters: { ...s.filters, excluded },
        selection: s.selection.filter((x) => !ids.includes(x)),
        primary: s.primary !== null && ids.includes(s.primary) ? null : s.primary,
      };
    });
  },

  setView(view) {
    set(isVisualView(view) ? { view, lastVisual: view } : { view });
  },

  setPane(pane, open) {
    set((s) => ({ panes: { ...s.panes, [pane]: open } }));
  },

  togglePane(pane) {
    set((s) => ({ panes: { ...s.panes, [pane]: !s.panes[pane] } }));
  },

  notify(status) {
    set({ status });
  },
}));

/** Current layout root, or `null`. */
export function selectRoot(s: AppState): number | null {
  return s.path[s.path.length - 1] ?? null;
}
