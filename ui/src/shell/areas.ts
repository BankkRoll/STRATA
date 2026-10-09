/**
 * The main areas of the app (the activity bar) and the views inside each.
 *
 * Every {@link ViewId} belongs to exactly one area; the activity bar switches
 * areas, the sidebar lists an area's views, and the workspace renders the
 * active view.
 */
import type { IconName } from "../components/icons";
import type { FeatureView, ViewId, VisualView } from "../lib/types";

/** Activity bar areas. */
export type AreaId = "explore" | "insights" | "cleanup" | "history" | "activity" | "settings";

/** Static description of an area. */
export interface AreaInfo {
  id: AreaId;
  label: string;
  icon: IconName;
  /** Global shortcut (display form). */
  shortcut: string;
  /** View opened the first time the area is visited. */
  defaultView: ViewId;
  /** One-line purpose, used as the sidebar subtitle and tooltip. */
  hint: string;
}

/** Areas in activity bar order. Settings sits apart at the bottom of the bar. */
export const AREAS: readonly AreaInfo[] = [
  { id: "explore", label: "Explore", icon: "treemap", shortcut: "Alt+1", defaultView: "home", hint: "Volumes, maps and folders" },
  { id: "insights", label: "Insights", icon: "sparkle", shortcut: "Alt+2", defaultView: "recommendations", hint: "Where space goes and what to remove" },
  { id: "cleanup", label: "Cleanup", icon: "broom", shortcut: "Alt+3", defaultView: "cleanup", hint: "Review the queue and run Windows tools" },
  { id: "history", label: "History", icon: "history", shortcut: "Alt+4", defaultView: "history", hint: "Snapshots and what changed" },
  { id: "activity", label: "Activity", icon: "pulse", shortcut: "Alt+5", defaultView: "activity", hint: "Programs writing to disk" },
  { id: "settings", label: "Settings", icon: "gear", shortcut: "Ctrl+,", defaultView: "settings", hint: "Preferences, rules and data" },
];

const AREA_BY_ID = new Map(AREAS.map((a) => [a.id, a]));

/** Metadata of an area. */
export function areaInfo(id: AreaId): AreaInfo {
  return AREA_BY_ID.get(id) as AreaInfo;
}

const AREA_OF_FEATURE: Readonly<Record<FeatureView, AreaId>> = {
  recommendations: "insights",
  largest: "insights",
  filetypes: "insights",
  categories: "insights",
  apps: "insights",
  duplicates: "insights",
  cleanup: "cleanup",
  tools: "cleanup",
  history: "history",
  activity: "activity",
  settings: "settings",
};

/**
 * The area a view belongs to.
 *
 * @param view - Any view id.
 * @returns Its area; the volumes page and every visual view are in Explore.
 */
export function areaOf(view: ViewId): AreaId {
  return (AREA_OF_FEATURE as Partial<Record<ViewId, AreaId>>)[view] ?? "explore";
}

/** One entry of the visual view switcher. */
export interface VisualViewInfo {
  id: VisualView;
  label: string;
  icon: IconName;
  shortcut: string;
}

/** Visual views in switcher order, with their Ctrl+digit shortcuts. */
export const VISUAL_VIEWS: readonly VisualViewInfo[] = [
  { id: "treemap", label: "Treemap", icon: "treemap", shortcut: "Ctrl+1" },
  { id: "sunburst", label: "Sunburst", icon: "sunburst", shortcut: "Ctrl+2" },
  { id: "icicle", label: "Icicle", icon: "icicle", shortcut: "Ctrl+3" },
  { id: "flame", label: "Flame", icon: "flame", shortcut: "Ctrl+4" },
  { id: "bubbles", label: "Bubbles", icon: "bubbles", shortcut: "Ctrl+5" },
  { id: "mindmap", label: "Mind map", icon: "mindmap", shortcut: "Ctrl+6" },
];

/** Shortcut of the list-only workspace mode (the last switcher entry). */
export const LIST_VIEW_SHORTCUT = "Ctrl+7";
