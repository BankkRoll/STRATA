/**
 * Domain types shared by the store, the backend contracts and the views.
 * Serialized names match the Rust `serde` names in `strata-core`.
 */

/** Which size every view aggregates; serde name of `strata_core::SizeMode`. */
export type SizeMode = "allocated" | "logical";

/** Cleanup safety tier (`strata_core::Safety`). */
export type Safety = "safe" | "probably" | "careful" | "never";

/** Treemap rendering style. */
export type TreemapStyle = "flat" | "cushion";

/** Visual views that render a layout. */
export type VisualView = "treemap" | "sunburst" | "icicle" | "flame" | "bubbles" | "mindmap";

/** Non-visual views (insights, cleanup, settings); each is a lazy chunk. */
export type FeatureView =
  | "cleanup"
  | "recommendations"
  | "largest"
  | "filetypes"
  | "apps"
  | "categories"
  | "duplicates"
  | "history"
  | "activity"
  | "tools"
  | "settings";

/** Every top-level view in the left nav. */
export type ViewId = "home" | VisualView | FeatureView;

/** Filters applied by the backend when it lays out and lists entries. */
export interface ViewFilters {
  /** Entry ids hidden via "Exclude from view". */
  excluded: number[];
  /** Category ids to show; empty = all. */
  categories: number[];
  /** Hide entries smaller than this many bytes (0 = no limit). */
  minBytes: number;
  /** Only entries modified within this many days (`null` = any time). */
  modifiedWithinDays: number | null;
}

/** Filters with nothing applied. */
export const NO_FILTERS: Readonly<ViewFilters> = Object.freeze({
  excluded: [],
  categories: [],
  minBytes: 0,
  modifiedWithinDays: null,
});

/** Whether any filter is active. */
export function hasFilters(f: ViewFilters): boolean {
  return f.excluded.length > 0 || f.categories.length > 0 || f.minBytes > 0 || f.modifiedWithinDays !== null;
}
