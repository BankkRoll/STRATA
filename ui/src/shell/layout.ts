/**
 * Window layout state: sidebar, inspector and list sizes, which panes are
 * open, the workspace split mode, collapsed sidebar sections, saved filters
 * and the last view of every area.
 *
 * This is pure UI layout, so it persists per user in `localStorage` (never
 * in the settings store) and is read back synchronously at startup so the
 * first paint already has the user's layout. Storage failures (private mode,
 * quota) fall back to the defaults silently.
 */
import { useEffect } from "react";
import { create } from "zustand";
import type { ViewFilters, ViewId } from "../lib/types";
import { useApp } from "../store/app";
import type { AreaId } from "./areas";

/** How the Explore workspace splits the visual view and the list. */
export type SplitMode = "split" | "visual" | "list";

/** A named, reusable set of view filters. */
export interface SavedFilter {
  id: string;
  name: string;
  filters: ViewFilters;
}

/** Persisted layout. */
export interface LayoutPrefs {
  /** Sidebar width in CSS px. */
  sidebarWidth: number;
  sidebarOpen: boolean;
  /** Inspector (detail panel) width in CSS px. */
  inspectorWidth: number;
  /** List pane height in CSS px when split. */
  listHeight: number;
  split: SplitMode;
  /** Sidebar sections the user collapsed, by section id. */
  collapsed: Record<string, boolean>;
  savedFilters: SavedFilter[];
  /** Last view shown in each area, restored when the area is reopened. */
  lastView: Partial<Record<AreaId, ViewId>>;
  /** Pane visibility mirrored from the app store (`list`, `detail`). */
  listOpen: boolean;
  inspectorOpen: boolean;
}

/** Size limits shared by the splitters and the persistence clamp. */
export const LIMITS = {
  sidebar: { min: 200, max: 420, initial: 256 },
  inspector: { min: 280, max: 560, initial: 336 },
  list: { min: 120, max: 640, initial: 280 },
} as const;

/** Defaults for a first run. */
export const DEFAULT_LAYOUT: Readonly<LayoutPrefs> = Object.freeze({
  sidebarWidth: LIMITS.sidebar.initial,
  sidebarOpen: true,
  inspectorWidth: LIMITS.inspector.initial,
  listHeight: LIMITS.list.initial,
  split: "split",
  collapsed: {},
  savedFilters: [],
  lastView: {},
  listOpen: true,
  inspectorOpen: true,
});

/** `localStorage` key, versioned so a shape change never reads stale data. */
export const LAYOUT_STORAGE_KEY = "strata.layout.v1";

/** Layout store: persisted prefs plus transient overlay state. */
export interface LayoutState extends LayoutPrefs {
  /** Sidebar shown as an overlay (narrow windows, where it is normally collapsed). */
  sidebarOverlay: boolean;
  /** The keyboard shortcut sheet is open. */
  cheatSheet: boolean;
  /** The path bar is in edit mode (Ctrl+L). */
  editingPath: boolean;
  /** Settings category to scroll to when Settings opens (deep links). */
  settingsFocus: string | null;
  set: (patch: Partial<LayoutPrefs>) => void;
  setSidebarOverlay: (open: boolean) => void;
  setCheatSheet: (open: boolean) => void;
  setEditingPath: (editing: boolean) => void;
  toggleSection: (id: string) => void;
  saveFilter: (name: string, filters: ViewFilters) => SavedFilter;
  deleteFilter: (id: string) => void;
}

const clamp = (v: unknown, min: number, max: number, fallback: number): number =>
  typeof v === "number" && Number.isFinite(v) ? Math.round(Math.min(max, Math.max(min, v))) : fallback;

/**
 * Validates persisted JSON field by field, so one damaged value never resets
 * the rest of the layout.
 *
 * @param raw - Parsed storage value (any shape).
 * @returns A complete, in-range layout.
 */
export function sanitizeLayout(raw: unknown): LayoutPrefs {
  const r = (typeof raw === "object" && raw !== null ? raw : {}) as Partial<Record<keyof LayoutPrefs, unknown>>;
  const bool = (v: unknown, d: boolean) => (typeof v === "boolean" ? v : d);
  const split: SplitMode = r.split === "visual" || r.split === "list" || r.split === "split" ? r.split : DEFAULT_LAYOUT.split;
  const collapsed: Record<string, boolean> = {};
  if (typeof r.collapsed === "object" && r.collapsed !== null) {
    for (const [k, v] of Object.entries(r.collapsed)) if (typeof v === "boolean") collapsed[k] = v;
  }
  const savedFilters = Array.isArray(r.savedFilters)
    ? (r.savedFilters as unknown[]).filter((f): f is SavedFilter => {
        const s = f as { id?: unknown; name?: unknown; filters?: unknown } | null;
        return typeof s?.id === "string" && typeof s.name === "string" && typeof s.filters === "object" && s.filters !== null;
      })
    : [];
  const lastView = typeof r.lastView === "object" && r.lastView !== null ? (r.lastView as LayoutPrefs["lastView"]) : {};
  return {
    sidebarWidth: clamp(r.sidebarWidth, LIMITS.sidebar.min, LIMITS.sidebar.max, DEFAULT_LAYOUT.sidebarWidth),
    sidebarOpen: bool(r.sidebarOpen, DEFAULT_LAYOUT.sidebarOpen),
    inspectorWidth: clamp(r.inspectorWidth, LIMITS.inspector.min, LIMITS.inspector.max, DEFAULT_LAYOUT.inspectorWidth),
    listHeight: clamp(r.listHeight, LIMITS.list.min, LIMITS.list.max, DEFAULT_LAYOUT.listHeight),
    split,
    collapsed,
    savedFilters,
    lastView,
    listOpen: bool(r.listOpen, DEFAULT_LAYOUT.listOpen),
    inspectorOpen: bool(r.inspectorOpen, DEFAULT_LAYOUT.inspectorOpen),
  };
}

/** Reads the persisted layout, or the defaults. */
export function readLayout(): LayoutPrefs {
  try {
    const text = window.localStorage.getItem(LAYOUT_STORAGE_KEY);
    return sanitizeLayout(text ? (JSON.parse(text) as unknown) : null);
  } catch {
    return { ...DEFAULT_LAYOUT };
  }
}

function prefsOf(s: LayoutState): LayoutPrefs {
  return {
    sidebarWidth: s.sidebarWidth,
    sidebarOpen: s.sidebarOpen,
    inspectorWidth: s.inspectorWidth,
    listHeight: s.listHeight,
    split: s.split,
    collapsed: s.collapsed,
    savedFilters: s.savedFilters,
    lastView: s.lastView,
    listOpen: s.listOpen,
    inspectorOpen: s.inspectorOpen,
  };
}

/** Writes the layout; ignores storage failures. */
export function writeLayout(prefs: LayoutPrefs): void {
  try {
    window.localStorage.setItem(LAYOUT_STORAGE_KEY, JSON.stringify(prefs));
  } catch {
    // Storage blocked or full: the layout simply lasts for this session.
  }
}

let filterSeq = 0;

/** The layout store. */
export const useLayout = create<LayoutState>()((set) => ({
  ...readLayout(),
  sidebarOverlay: false,
  cheatSheet: false,
  editingPath: false,
  settingsFocus: null,
  set(patch) {
    set(patch);
  },
  setSidebarOverlay(sidebarOverlay) {
    set({ sidebarOverlay });
  },
  setCheatSheet(cheatSheet) {
    set({ cheatSheet });
  },
  setEditingPath(editingPath) {
    set({ editingPath });
  },
  toggleSection(id) {
    set((s) => ({ collapsed: { ...s.collapsed, [id]: !s.collapsed[id] } }));
  },
  saveFilter(name, filters) {
    filterSeq += 1;
    const saved: SavedFilter = { id: `f${Date.now().toString(36)}${filterSeq}`, name: name.trim() || "Untitled filter", filters };
    set((s) => ({ savedFilters: [...s.savedFilters, saved] }));
    return saved;
  },
  deleteFilter(id) {
    set((s) => ({ savedFilters: s.savedFilters.filter((f) => f.id !== id) }));
  },
}));

/** Resets the layout store to defaults (tests and "Reset layout"). */
export function resetLayout(): void {
  useLayout.setState({ ...DEFAULT_LAYOUT, collapsed: {}, savedFilters: [], lastView: {}, sidebarOverlay: false, cheatSheet: false, editingPath: false, settingsFocus: null });
}

/**
 * Keeps the app store's list/detail pane flags and the persisted layout in
 * step, and writes the layout to storage (debounced) whenever it changes.
 * Mounted once by the shell.
 */
export function useLayoutPersistence(): void {
  useEffect(() => {
    const { listOpen, inspectorOpen } = useLayout.getState();
    const app = useApp.getState();
    if (app.panes.list !== listOpen) app.setPane("list", listOpen);
    if (app.panes.detail !== inspectorOpen) app.setPane("detail", inspectorOpen);
    const unApp = useApp.subscribe((s, prev) => {
      if (s.panes === prev.panes) return;
      const l = useLayout.getState();
      if (l.listOpen !== s.panes.list || l.inspectorOpen !== s.panes.detail) {
        useLayout.setState({ listOpen: s.panes.list, inspectorOpen: s.panes.detail });
      }
    });
    let timer: ReturnType<typeof setTimeout> | null = null;
    const unLayout = useLayout.subscribe(() => {
      if (timer !== null) clearTimeout(timer);
      timer = setTimeout(() => {
        timer = null;
        writeLayout(prefsOf(useLayout.getState()));
      }, 150);
    });
    return () => {
      unApp();
      unLayout();
      if (timer !== null) {
        clearTimeout(timer);
        writeLayout(prefsOf(useLayout.getState()));
      }
    };
  }, []);
}
