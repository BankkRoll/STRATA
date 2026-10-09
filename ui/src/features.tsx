/**
 * Dependency injection for the wave-2 views (cleanup, tools, insights,
 * duplicates, activity, history, settings).
 *
 * Kept separate from `services.tsx` so the two evolve independently: the
 * context's default value is the Tauri implementation, so production needs
 * no wiring, while tests and the dev harness provide fakes through
 * {@link FeaturesContext}. Availability still comes from
 * `Services.capabilities()` (`app_capabilities`).
 */
import { createContext, useContext } from "react";
import * as activity from "./lib/activity";
import * as cleanup from "./lib/cleanup";
import * as dupes from "./lib/dupes";
import * as history from "./lib/history";
import * as insights from "./lib/insights";
import * as settings from "./lib/settings";
import * as tools from "./lib/tools";

/** Cleanup queue, review, execution and undo. */
export type CleanupApi = Pick<
  typeof cleanup,
  | "fetchQueue"
  | "addToQueue"
  | "removeFromQueue"
  | "clearQueue"
  | "watchQueue"
  | "planCleanup"
  | "preflightCleanup"
  | "prepareClose"
  | "closeApp"
  | "executeCleanup"
  | "cancelCleanup"
  | "retryPlan"
  | "deleteOnReboot"
  | "fetchUndoHistory"
  | "restoreItems"
>;

/** Built-in tools. */
export type ToolsApi = Pick<typeof tools, "fetchToolsStatus" | "prepareTool" | "runTool">;

/** Index-backed insights. */
export type InsightsApi = Pick<
  typeof insights,
  | "fetchLargest"
  | "fetchFileTypes"
  | "fetchCategories"
  | "fetchApps"
  | "fetchOrphans"
  | "queueAppCaches"
  | "fetchRecommendations"
  | "previewRecommendation"
  | "queueRecommendation"
>;

/** Duplicate finder. */
export type DupesApi = Pick<
  typeof dupes,
  | "fetchDupeStatus"
  | "startDupeScan"
  | "cancelDupeScan"
  | "watchDupeStatus"
  | "fetchDupeGroups"
  | "queueDupes"
  | "prepareHardlinks"
  | "replaceWithHardlinks"
>;

/** ETW activity. */
export type ActivityApi = Pick<typeof activity, "fetchActivityStatus" | "setActivityEnabled" | "fetchTopWriters" | "fetchDirWriters" | "clearActivity">;

/** Snapshots and diffs. */
export type HistoryApi = Pick<typeof history, "fetchSnapshots" | "fetchUsage" | "fetchDiff" | "fetchDirSeries">;

/** Settings, rules, data, helper service and about. */
export type SettingsApi = Pick<
  typeof settings,
  | "loadSettings"
  | "saveSettings"
  | "exportSettings"
  | "importSettings"
  | "fetchRules"
  | "openRulesFolder"
  | "reloadRules"
  | "explainPath"
  | "clearData"
  | "fetchHelperService"
  | "installHelperService"
  | "uninstallHelperService"
  | "fetchLicenses"
  | "checkForUpdates"
  | "restartToUpdate"
  | "reportIssue"
>;

/** Everything the wave-2 views need from the backend. */
export interface FeatureServices {
  cleanup: CleanupApi;
  tools: ToolsApi;
  insights: InsightsApi;
  dupes: DupesApi;
  activity: ActivityApi;
  history: HistoryApi;
  settings: SettingsApi;
}

/** The production implementation over Tauri commands. */
export const tauriFeatures: FeatureServices = { cleanup, tools, insights, dupes, activity, history, settings };

/** Context carrying the active {@link FeatureServices}; defaults to Tauri. */
export const FeaturesContext = createContext<FeatureServices>(tauriFeatures);

/** Reads the active feature services. */
export function useFeatures(): FeatureServices {
  return useContext(FeaturesContext);
}
