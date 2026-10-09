/**
 * ETW activity tracking (SPEC §11): opt-in, helper-only, local-only.
 * Top writers per window, per-directory writers for the detail panel, and
 * one-click clearing.
 */
import { call } from "./backend";

/** Tracking state. */
export interface ActivityStatus {
  /** `activity.enabled` (opt-in, off by default). */
  enabled: boolean;
  /** The ETW session is running. */
  running: boolean;
  /** Tracking needs the elevated helper and none is connected. */
  needsHelper: boolean;
  /** Overhead guard is sampling because CPU use exceeded the cap. */
  throttled: boolean;
  /** Recent sustained CPU share of tracing, in percent, or `null`. */
  cpuPercent: number | null;
  /** Oldest data kept, Unix ms, or `null` when empty. */
  sinceMs: number | null;
  retentionDays: number;
}

/** Time window for top writers. */
export type ActivityWindow = "now" | "hour" | "today";

/** One process's writes in a window (`strata_store::WriterTotal`). */
export interface WriterRow {
  /** Full image path of the process. */
  image: string;
  /** Executable name. */
  name: string;
  bytesWritten: number;
  filesCreated: number;
  filesDeleted: number;
  /** Directories it wrote most to. */
  topDirs: { path: string; bytesWritten: number }[];
}

/** Reads tracking state (`activity_status`). */
export function fetchActivityStatus(): Promise<ActivityStatus> {
  return call<ActivityStatus>("activity_status");
}

/** Turns tracking on/off (`activity_set_enabled`); enabling may prompt for elevation. */
export function setActivityEnabled(enabled: boolean): Promise<ActivityStatus> {
  return call<ActivityStatus>("activity_set_enabled", { enabled });
}

/** Top writers in a window (`activity_top`). */
export function fetchTopWriters(window: ActivityWindow, limit: number): Promise<WriterRow[]> {
  return call<WriterRow[]>("activity_top", { window, limit });
}

/** Writers under one directory (`activity_dir_writers`), for the detail panel. */
export function fetchDirWriters(volumeId: string, id: number, days: number): Promise<WriterRow[]> {
  return call<WriterRow[]>("activity_dir_writers", { volumeId, id, days });
}

/** Deletes all activity data (`activity_clear`). */
export function clearActivity(): Promise<null> {
  return call<null>("activity_clear");
}
