/**
 * Index-backed analysis views (SPEC §16.2): largest files, file types,
 * categories, apps (SPEC §12.3) and recommendations ("Free up space").
 *
 * Every result is computed by the backend from the index, classifier and
 * app catalog; the UI only renders and filters. Sizes follow the requested
 * size mode.
 */
import { call } from "./backend";
import type { QueueAddResult } from "./cleanup";
import type { Confidence } from "./detail";
import type { ToolAction } from "./tools";
import type { Safety, SizeMode } from "./types";

// -----------------------------------------------------------------------------
// Largest files
// -----------------------------------------------------------------------------

/** Filters for the top-N query (`Index::top_n` + `Filter`). All combine (AND). */
export interface LargestFilters {
  /** Lowercase extensions without the dot; empty = any. */
  extensions: string[];
  /** Category ids; empty = any. */
  categories: number[];
  /** Safety tiers; empty = any. */
  safety: Safety[];
  minBytes: number;
  /** Only entries modified within N days. */
  modifiedWithinDays: number | null;
  /** Only entries untouched for at least N days ("old & large"). */
  untouchedForDays: number | null;
}

/** Filters with nothing applied. */
export const NO_LARGEST_FILTERS: Readonly<LargestFilters> = Object.freeze({
  extensions: [],
  categories: [],
  safety: [],
  minBytes: 0,
  modifiedWithinDays: null,
  untouchedForDays: null,
});

/** Request for `insights_largest`. */
export interface LargestQuery {
  volumeId: string;
  /** Entry id to search under, or `null` for the whole volume. */
  scope: number | null;
  kind: "files" | "folders";
  /** Default 1000 (SPEC §16.2). */
  limit: number;
  sizeMode: SizeMode;
  filters: LargestFilters;
}

/** One ranked entry. */
export interface LargestEntry {
  id: number;
  name: string;
  path: string;
  /** Size in the requested mode. */
  bytes: number;
  modifiedMs: number | null;
  category: number;
  safety: Safety | null;
  app: string | null;
  /** Lowercase extension without the dot, or `null`. */
  extension: string | null;
}

/** Result of `insights_largest`. */
export interface LargestResult {
  entries: LargestEntry[];
  /** Entries matching the filters (may exceed `entries.length`). */
  matched: number;
}

/** Global top-N (`insights_largest`). */
export function fetchLargest(query: LargestQuery): Promise<LargestResult> {
  return call<LargestResult>("insights_largest", { query });
}

// -----------------------------------------------------------------------------
// File types
// -----------------------------------------------------------------------------

/** One extension's totals (`Index::extension_breakdown`). */
export interface ExtensionRow {
  /** Lowercase extension without the dot; "" for files without one. */
  extension: string;
  /** Display group ("Video", "Archive", …) from the classifier's extension table. */
  group: string;
  files: number;
  bytes: number;
  /** Files whose sniffed content disagrees with the extension (SPEC §12.4). */
  mismatched: number;
}

/** Totals by sniffed content type. */
export interface DetectedTypeRow {
  /** e.g. "GGUF model", "ZIP archive". */
  label: string;
  files: number;
  bytes: number;
}

/** Result of `insights_file_types`. */
export interface FileTypeBreakdown {
  byExtension: ExtensionRow[];
  /** Only files that were sniffed (large files, or opened in the detail panel). */
  byDetectedType: DetectedTypeRow[];
  totalBytes: number;
}

/** Extension and detected-type breakdown under `scope` (`insights_file_types`). */
export function fetchFileTypes(volumeId: string, scope: number | null, sizeMode: SizeMode): Promise<FileTypeBreakdown> {
  return call<FileTypeBreakdown>("insights_file_types", { volumeId, scope, sizeMode });
}

// -----------------------------------------------------------------------------
// Categories
// -----------------------------------------------------------------------------

/** One category's totals (`Index::category_breakdown`). */
export interface CategoryTotal {
  category: number;
  bytes: number;
  files: number;
  /** Largest top-level contributors, for drill-through. */
  top: { id: number; name: string; path: string; bytes: number }[];
}

/** Totals per category under `scope` (`insights_categories`). */
export function fetchCategories(volumeId: string, scope: number | null, sizeMode: SizeMode): Promise<CategoryTotal[]> {
  return call<CategoryTotal[]>("insights_categories", { volumeId, scope, sizeMode });
}

// -----------------------------------------------------------------------------
// Apps
// -----------------------------------------------------------------------------

/** Kind of location in an app's footprint. */
export type FootprintKind = "install" | "data" | "cache" | "logs" | "updates" | "other";

/** One location of an app's footprint. */
export interface FootprintLocation {
  kind: FootprintKind;
  path: string;
  volumeId: string;
  /** Index entry, when the location is on a scanned volume. */
  entryId: number | null;
  bytes: number;
  safety: Safety | null;
}

/** An installed app and everything attributed to it (SPEC §12.3). */
export interface AppFootprint {
  /** Catalog id (stable across sessions). */
  id: string;
  name: string;
  publisher: string | null;
  version: string | null;
  source: "registry" | "appx" | "launcher" | "rule" | "heuristic";
  confidence: Confidence;
  evidence: string[];
  /** Measured bytes over all locations. */
  totalBytes: number;
  locations: FootprintLocation[];
  /** Registry `EstimatedSize`, or `null`. */
  registryEstimateBytes: number | null;
  /** Measured and registry sizes differ wildly (backend's threshold). */
  mismatch: boolean;
  /** An `UninstallString` exists. */
  canUninstall: boolean;
  /** Bytes in safe/probably cache locations. */
  cacheBytes: number;
  /** A process of the app is running (warn before cleaning its caches). */
  running: boolean;
}

/** Leftover app data with no installed owner (careful). */
export interface OrphanFolder {
  path: string;
  volumeId: string;
  entryId: number | null;
  bytes: number;
  lastActivityMs: number | null;
  /** Best guess at the former owner. */
  guessedApp: string | null;
  /** Why it looks orphaned. */
  reason: string;
}

/** Apps with their footprint (`apps_footprint`). */
export function fetchApps(): Promise<AppFootprint[]> {
  return call<AppFootprint[]>("apps_footprint");
}

/** Orphaned app data (`apps_orphans`). */
export function fetchOrphans(): Promise<OrphanFolder[]> {
  return call<OrphanFolder[]>("apps_orphans");
}

/** Queues an app's safe/probably cache locations (`apps_queue_caches`). */
export function queueAppCaches(appId: string): Promise<QueueAddResult> {
  return call<QueueAddResult>("apps_queue_caches", { appId });
}

// -----------------------------------------------------------------------------
// Recommendations
// -----------------------------------------------------------------------------

/** What acting on a recommendation does. */
export type RecommendationAction =
  | { kind: "queue" }
  | { kind: "tool"; tool: ToolAction }
  | { kind: "view"; view: "duplicates" | "apps" | "largest" };

/** One ranked, explainable finding. */
export interface Recommendation {
  id: string;
  /** e.g. "caches", "stale_node_modules", "old_installers", "recycle_bin", "windows_update", "duplicates". */
  kind: string;
  title: string;
  /** One-line summary, e.g. "11.2 GB in 48 projects". */
  summary: string;
  /** Why it is safe or what to check, shown on expand. */
  explain: string;
  bytes: number;
  items: number;
  /** Strictest tier among its items. */
  safety: Safety;
  action: RecommendationAction;
}

/** A preview item of a recommendation. */
export interface RecommendationItem {
  volumeId: string;
  entryId: number;
  path: string;
  bytes: number;
  safety: Safety;
  explain: string;
}

/** Ranked recommendations (`recommendations_list`). */
export function fetchRecommendations(): Promise<Recommendation[]> {
  return call<Recommendation[]>("recommendations_list");
}

/** The items a recommendation covers (`recommendations_preview`). */
export function previewRecommendation(id: string, limit: number): Promise<{ items: RecommendationItem[]; total: number }> {
  return call<{ items: RecommendationItem[]; total: number }>("recommendations_preview", { id, limit });
}

/** Queues a recommendation's items (`recommendations_queue`). */
export function queueRecommendation(id: string, exclude: number[]): Promise<QueueAddResult> {
  return call<QueueAddResult>("recommendations_queue", { id, exclude });
}
