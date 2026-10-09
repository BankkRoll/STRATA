/**
 * Snapshots, usage series and diffs, from `strata-store`.
 * All times are Unix ms UTC; the UI formats them in local time.
 */
import { call } from "./backend";
import type { SizeMode } from "./types";

/** One stored snapshot (`strata_store::SnapshotInfo`). */
export interface SnapshotInfo {
  id: number;
  takenMs: number;
  totalBytes: number;
  usedBytes: number;
  freeBytes: number;
  allocatedSum: number;
  logicalSum: number;
  files: number;
  dirs: number;
  scanner: "mft" | "walker";
}

/** One point of the volume usage series (`strata_store::UsagePoint`). */
export interface UsagePoint {
  snapshotId: number;
  atMs: number;
  totalBytes: number;
  usedBytes: number;
  freeBytes: number;
}

/** Directory sizes in a snapshot. */
export interface DirSizes {
  allocated: number;
  logical: number;
  files: number;
}

/** One directory's change between two snapshots (`strata_store::DirChange`). */
export interface DirChange {
  path: string;
  before: DirSizes | null;
  after: DirSizes | null;
  /** Signed delta in the requested size mode. */
  delta: number;
  /** Entry id in the current index when the folder still exists. */
  entryId: number | null;
}

/** Diff between two snapshots (`strata_store::SnapshotDiff`). */
export interface SnapshotDiff {
  from: SnapshotInfo;
  to: SnapshotInfo;
  usedDelta: number;
  scannedDelta: number;
  grown: DirChange[];
  shrunk: DirChange[];
  newLarge: DirChange[];
  deletedLarge: DirChange[];
}

/** One point of a directory's history. */
export interface DirPoint {
  atMs: number;
  /** `null` when the directory was below the snapshot's minimum size or absent. */
  allocated: number | null;
  logical: number | null;
}

/** Snapshots of a volume, oldest first (`history_snapshots`). */
export function fetchSnapshots(volumeId: string): Promise<SnapshotInfo[]> {
  return call<SnapshotInfo[]>("history_snapshots", { volumeId });
}

/** Usage series (`history_usage`); `null` bounds mean open-ended. */
export function fetchUsage(volumeId: string, fromMs: number | null, toMs: number | null): Promise<UsagePoint[]> {
  return call<UsagePoint[]>("history_usage", { volumeId, fromMs, toMs });
}

/** Diff of two snapshots (`history_diff`). */
export function fetchDiff(fromId: number, toId: number, sizeMode: SizeMode, topN: number): Promise<SnapshotDiff> {
  return call<SnapshotDiff>("history_diff", { fromId, toId, sizeMode, topN });
}

/** A directory's size history, oldest first (`history_dir_series`). */
export function fetchDirSeries(volumeId: string, id: number, lastN: number): Promise<DirPoint[]> {
  return call<DirPoint[]>("history_dir_series", { volumeId, id, lastN });
}

/**
 * Normalizes a diff pick so `from` is the older snapshot.
 *
 * @param snapshots - Snapshots (any order).
 * @param a - One picked id.
 * @param b - The other picked id.
 * @returns `[fromId, toId]`, or `null` when either is unknown or they are equal.
 */
export function orderPick(snapshots: readonly SnapshotInfo[], a: number, b: number): [number, number] | null {
  if (a === b) return null;
  const sa = snapshots.find((s) => s.id === a);
  const sb = snapshots.find((s) => s.id === b);
  if (!sa || !sb) return null;
  return sa.takenMs <= sb.takenMs ? [sa.id, sb.id] : [sb.id, sa.id];
}
