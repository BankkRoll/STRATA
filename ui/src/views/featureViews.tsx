/**
 * Registry and router of the non-visual views (insights, cleanup, tools,
 * settings). Each view is its own lazy chunk so the entry bundle stays
 * within budget; the shell renders {@link FeatureRouter} for any
 * {@link FeatureView}.
 */
import { Suspense, lazy, type ComponentType, type LazyExoticComponent } from "react";
import type { IconName } from "../components/icons";
import type { FeatureView, ViewId } from "../lib/types";
import { useApp } from "../store/app";

/** Nav metadata for a feature view. */
export interface FeatureViewInfo {
  id: FeatureView;
  label: string;
  icon: IconName;
  /** Works on the open volume; disabled with a reason until one is open. */
  needsVolume: boolean;
  /** Nav group heading. */
  group: "Insights" | "Manage";
  /** The selection is meaningful here, so the detail panel stays open. */
  showsDetail: boolean;
}

/** Feature views in nav order. */
export const FEATURE_VIEWS: readonly FeatureViewInfo[] = [
  { id: "recommendations", label: "Free up space", icon: "sparkle", needsVolume: false, group: "Insights", showsDetail: false },
  { id: "largest", label: "Largest files", icon: "largest", needsVolume: true, group: "Insights", showsDetail: true },
  { id: "filetypes", label: "File types", icon: "tag", needsVolume: true, group: "Insights", showsDetail: false },
  { id: "categories", label: "Categories", icon: "bars", needsVolume: true, group: "Insights", showsDetail: true },
  { id: "apps", label: "Apps", icon: "apps", needsVolume: false, group: "Insights", showsDetail: false },
  { id: "duplicates", label: "Duplicates", icon: "copies", needsVolume: false, group: "Insights", showsDetail: false },
  { id: "history", label: "History", icon: "history", needsVolume: true, group: "Insights", showsDetail: true },
  { id: "activity", label: "Activity", icon: "pulse", needsVolume: false, group: "Insights", showsDetail: false },
  { id: "cleanup", label: "Cleanup queue", icon: "broom", needsVolume: false, group: "Manage", showsDetail: false },
  { id: "tools", label: "Windows tools", icon: "tools", needsVolume: false, group: "Manage", showsDetail: false },
  { id: "settings", label: "Settings", icon: "gear", needsVolume: false, group: "Manage", showsDetail: false },
];

const BY_ID = new Map(FEATURE_VIEWS.map((v) => [v.id, v]));

/** Whether a view id is a feature view. */
export function isFeatureView(view: ViewId): view is FeatureView {
  return BY_ID.has(view as FeatureView);
}

/** Metadata of a feature view. */
export function featureInfo(view: FeatureView): FeatureViewInfo {
  return BY_ID.get(view) as FeatureViewInfo;
}

type Lazy = LazyExoticComponent<ComponentType>;

const named = <K extends string>(p: Promise<Record<K, ComponentType>>, k: K) => p.then((m) => ({ default: m[k] }));

const VIEWS: Readonly<Record<FeatureView, Lazy>> = {
  cleanup: lazy(() => named(import("./CleanupView"), "CleanupView")),
  recommendations: lazy(() => named(import("./RecommendationsView"), "RecommendationsView")),
  largest: lazy(() => named(import("./LargestView"), "LargestView")),
  filetypes: lazy(() => named(import("./FileTypesView"), "FileTypesView")),
  apps: lazy(() => named(import("./AppsView"), "AppsView")),
  categories: lazy(() => named(import("./CategoriesView"), "CategoriesView")),
  duplicates: lazy(() => named(import("./DuplicatesView"), "DuplicatesView")),
  history: lazy(() => named(import("./HistoryView"), "HistoryView")),
  activity: lazy(() => named(import("./ActivityView"), "ActivityView")),
  tools: lazy(() => named(import("./ToolsView"), "ToolsView")),
  settings: lazy(() => named(import("./SettingsView"), "SettingsView")),
};

/** Renders the active feature view (lazy), or the "open a volume" state. */
export function FeatureRouter({ view }: { view: FeatureView }) {
  const hasVolume = useApp((s) => s.volumeId !== null);
  const info = featureInfo(view);
  if (info.needsVolume && !hasVolume) {
    return (
      <div className="state state--quiet">
        <h1>{info.label}</h1>
        <p>Open a scanned volume first. {info.label} works on the volume you are exploring.</p>
        <button
          type="button"
          className="btn btn--primary"
          onClick={() => {
            useApp.getState().setView("home");
          }}
        >
          Go to volumes
        </button>
      </div>
    );
  }
  const View = VIEWS[view];
  return (
    <Suspense
      fallback={
        <div className="state state--quiet" role="status">
          <span className="spinner" aria-hidden="true" />
          <p>Loading {info.label.toLowerCase()}…</p>
        </div>
      }
    >
      <View />
    </Suspense>
  );
}
