/**
 * Workspace tabs: one per opened volume or folder root.
 *
 * The app store (`useApp`) stays the single source of truth for the location
 * being shown. The active tab mirrors it: drilling updates the active tab's
 * path, and activating a tab writes its location back into the app store.
 * {@link useTabSync} wires the two together.
 */
import { useEffect } from "react";
import { create } from "zustand";
import { isVisualView, useApp } from "../store/app";

/** One workspace tab. */
export interface WorkspaceTab {
  id: string;
  volumeId: string;
  /** Entry ids from the volume root to the tab's current folder. */
  path: number[];
}

/** Tab store shape. */
export interface TabsState {
  tabs: WorkspaceTab[];
  activeId: string | null;
}

/** The tab store. Mutations go through the functions below. */
export const useTabs = create<TabsState>()(() => ({ tabs: [], activeId: null }));

let seq = 0;
const newId = () => `t${++seq}`;

/** Resets tabs (tests). */
export function resetTabs(): void {
  useTabs.setState({ tabs: [], activeId: null });
}

/** The active tab, or `null`. */
export function activeTab(s: TabsState = useTabs.getState()): WorkspaceTab | null {
  return s.tabs.find((t) => t.id === s.activeId) ?? null;
}

function show(tab: WorkspaceTab | null): void {
  const app = useApp.getState();
  if (!tab) {
    useApp.setState({ volumeId: null, path: [], selection: [], primary: null, view: "home" });
    return;
  }
  const sameVolume = app.volumeId === tab.volumeId;
  const samePath = sameVolume && app.path.length === tab.path.length && app.path.every((id, i) => id === tab.path[i]);
  if (!samePath) useApp.setState({ volumeId: tab.volumeId, path: tab.path, selection: [], primary: null });
  if (!isVisualView(useApp.getState().view)) useApp.getState().setView(useApp.getState().lastVisual);
}

/**
 * Activates a tab and shows its location.
 *
 * @param id - Tab id.
 */
export function activateTab(id: string): void {
  const tab = useTabs.getState().tabs.find((t) => t.id === id);
  if (!tab) return;
  useTabs.setState({ activeId: id });
  show(tab);
}

/**
 * Opens a location in a new tab (or focuses the active one when asked to
 * reuse it) and shows it.
 *
 * @param volumeId - Volume.
 * @param path - Root-to-folder entry ids.
 * @returns The tab.
 */
export function openTab(volumeId: string, path: number[]): WorkspaceTab {
  const tab: WorkspaceTab = { id: newId(), volumeId, path: [...path] };
  const { tabs, activeId } = useTabs.getState();
  const at = tabs.findIndex((t) => t.id === activeId);
  const next = [...tabs];
  next.splice(at < 0 ? next.length : at + 1, 0, tab);
  useTabs.setState({ tabs: next, activeId: tab.id });
  show(tab);
  return tab;
}

/**
 * Closes a tab; closing the active one activates its right neighbour (or the
 * left one at the end). Closing the last tab returns to the volumes page.
 *
 * @param id - Tab id.
 */
export function closeTab(id: string): void {
  const { tabs, activeId } = useTabs.getState();
  const i = tabs.findIndex((t) => t.id === id);
  if (i < 0) return;
  const next = tabs.filter((t) => t.id !== id);
  if (id !== activeId) {
    useTabs.setState({ tabs: next });
    return;
  }
  const neighbour = next[i] ?? next[i - 1] ?? null;
  useTabs.setState({ tabs: next, activeId: neighbour?.id ?? null });
  show(neighbour);
}

/**
 * Moves a tab left (`-1`) or right (`1`).
 *
 * @param id - Tab id.
 * @param delta - Direction.
 */
export function moveTab(id: string, delta: -1 | 1): void {
  const tabs = [...useTabs.getState().tabs];
  const i = tabs.findIndex((t) => t.id === id);
  const j = i + delta;
  if (i < 0 || j < 0 || j >= tabs.length) return;
  [tabs[i], tabs[j]] = [tabs[j] as WorkspaceTab, tabs[i] as WorkspaceTab];
  useTabs.setState({ tabs });
}

/**
 * Activates the next or previous tab, wrapping around.
 *
 * @param delta - `1` next, `-1` previous.
 */
export function cycleTab(delta: -1 | 1): void {
  const { tabs, activeId } = useTabs.getState();
  if (tabs.length === 0) return;
  const i = tabs.findIndex((t) => t.id === activeId);
  const next = tabs[(i + delta + tabs.length) % tabs.length];
  if (next) activateTab(next.id);
}

/**
 * Mirrors the app store into the tabs: a location change updates the active
 * tab; opening a volume that no tab shows (from the volumes page, the
 * sidebar or a search result) opens a tab for it. Mounted once by the shell.
 */
export function useTabSync(): void {
  useEffect(() => {
    const sync = () => {
      const { volumeId, path } = useApp.getState();
      if (volumeId === null || path.length === 0) return;
      const state = useTabs.getState();
      const active = activeTab(state);
      if (active?.volumeId === volumeId) {
        if (active.path.length !== path.length || active.path.some((id, i) => id !== path[i])) {
          useTabs.setState({ tabs: state.tabs.map((t) => (t.id === active.id ? { ...t, path: [...path] } : t)) });
        }
        return;
      }
      const existing = state.tabs.find((t) => t.volumeId === volumeId);
      if (existing) {
        useTabs.setState({ activeId: existing.id, tabs: state.tabs.map((t) => (t.id === existing.id ? { ...t, path: [...path] } : t)) });
        return;
      }
      const tab: WorkspaceTab = { id: newId(), volumeId, path: [...path] };
      useTabs.setState({ tabs: [...state.tabs, tab], activeId: tab.id });
    };
    sync();
    return useApp.subscribe((s, prev) => {
      if (s.volumeId !== prev.volumeId || s.path !== prev.path) sync();
    });
  }, []);
}
