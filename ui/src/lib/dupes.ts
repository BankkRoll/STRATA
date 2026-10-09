/**
 * Duplicate finder: background scan status, groups sorted by
 * wasted bytes, keep suggestions, guardrailed selection and the optional
 * replace-with-hardlinks action.
 *
 * The guardrail ("never every copy") is enforced here for the UI and again
 * by the backend on `dupes_queue` / `dupes_hardlink_prompt`.
 */
import { call } from "./backend";
import { listenEvent } from "./bridge";
import type { QueueAddResult } from "./cleanup";
import type { Safety } from "./types";

/** Background scan state. */
export interface DupeScanStatus {
  state: "idle" | "running" | "done" | "cancelled" | "error";
  phase: "grouping" | "partial_hash" | "full_hash" | null;
  progress: { filesDone: number; filesTotal: number; bytesDone: number; bytesTotal: number; etaSecs: number | null } | null;
  /** Unix ms of the last completed run. */
  lastRunMs: number | null;
  groups: number;
  wastedBytes: number;
  /** Error text when `state === "error"`. */
  message: string | null;
}

/** One copy in a group. */
export interface DupeFile {
  /** Index within the group (stable for selection). */
  fileId: number;
  volumeId: string;
  entryId: number;
  path: string;
  modifiedMs: number | null;
  safety: Safety | null;
  inDownloads: boolean;
}

/** Why a copy is suggested to keep. */
export type KeepReason = "oldest" | "shortest_path" | "not_in_downloads" | "user_rule";

/** A set of identical files (same size and BLAKE3 hash). */
export interface DupeGroup {
  id: number;
  /** Size of one copy. */
  size: number;
  /** `size × (copies − 1)`. */
  wastedBytes: number;
  files: DupeFile[];
  keep: { fileId: number; reason: KeepReason; explain: string };
  /** All copies on one volume (hardlinking possible). */
  sameVolume: boolean;
}

/** Selected copies per group. */
export interface DupeSelection {
  groupId: number;
  fileIds: number[];
}

/** Reads scan status (`dupes_status`). */
export function fetchDupeStatus(): Promise<DupeScanStatus> {
  return call<DupeScanStatus>("dupes_status");
}

/** Starts (or resumes) a scan over volumes (`dupes_start`). Hashes are cached; cloud files are never hydrated. */
export function startDupeScan(volumeIds: string[], minBytes: number): Promise<null> {
  return call<null>("dupes_start", { volumeIds, minBytes });
}

/** Cancels the scan (`dupes_cancel`); it resumes from the hash cache next time. */
export function cancelDupeScan(): Promise<null> {
  return call<null>("dupes_cancel");
}

/** Subscribes to `dupes://status`. */
export function watchDupeStatus(onStatus: (s: DupeScanStatus) => void): () => void {
  return listenEvent<DupeScanStatus>("dupes://status", onStatus);
}

/** Pages groups by wasted bytes (`dupes_groups`). */
export function fetchDupeGroups(offset: number, limit: number): Promise<{ total: number; groups: DupeGroup[] }> {
  return call<{ total: number; groups: DupeGroup[] }>("dupes_groups", { offset, limit });
}

/** Queues the selected copies for cleanup (`dupes_queue`). */
export function queueDupes(selections: DupeSelection[]): Promise<QueueAddResult> {
  return call<QueueAddResult>("dupes_queue", { selections });
}

/** Consent prompt for hardlink replacement. */
export interface HardlinkPrompt {
  promptId: number;
  /** Warning text shown verbatim ("editing one changes all"). */
  message: string;
  files: number;
  bytesSaved: number;
  /** Selected copies refused because they are on another volume than the kept copy. */
  refusedCrossVolume: number;
  expiresMs: number;
}

/** Prepares replacing selected copies with hardlinks to the kept copy (`dupes_hardlink_prompt`). */
export function prepareHardlinks(selections: DupeSelection[]): Promise<HardlinkPrompt> {
  return call<HardlinkPrompt>("dupes_hardlink_prompt", { selections });
}

/** Replaces after consent (`dupes_hardlink`). */
export function replaceWithHardlinks(promptId: number): Promise<{ replaced: number; bytesSaved: number; failed: { path: string; message: string }[] }> {
  return call<{ replaced: number; bytesSaved: number; failed: { path: string; message: string }[] }>("dupes_hardlink", { promptId });
}

// -----------------------------------------------------------------------------
// Guardrails
// -----------------------------------------------------------------------------

/** Outcome of a selection change. */
export type SelectionChange = { ok: true; selected: number[] } | { ok: false; selected: number[]; reason: string };

/**
 * Toggles one copy, refusing to select the last unselected copy or a
 * never-tier copy.
 *
 * @param group - The group.
 * @param selected - Currently selected file ids.
 * @param fileId - Copy to toggle.
 * @returns The new selection, or the unchanged one with a reason.
 */
export function toggleDupe(group: DupeGroup, selected: readonly number[], fileId: number): SelectionChange {
  if (selected.includes(fileId)) return { ok: true, selected: selected.filter((x) => x !== fileId) };
  const file = group.files.find((f) => f.fileId === fileId);
  if (!file) return { ok: false, selected: [...selected], reason: "Unknown copy." };
  if (file.safety === "never") return { ok: false, selected: [...selected], reason: "This copy is in a protected location and can’t be removed." };
  if (selected.length + 1 >= group.files.length) {
    return { ok: false, selected: [...selected], reason: "At least one copy must stay. Strata never removes every copy." };
  }
  return { ok: true, selected: [...selected, fileId] };
}

/**
 * Selects every copy except the suggested keep (and never-tier copies).
 *
 * @param group - The group.
 * @returns Selected file ids; always leaves at least one copy.
 */
export function selectAllButKeep(group: DupeGroup): number[] {
  const out = group.files.filter((f) => f.fileId !== group.keep.fileId && f.safety !== "never").map((f) => f.fileId);
  return out.length >= group.files.length ? out.slice(0, group.files.length - 1) : out;
}

/**
 * Whether every selection keeps at least one copy (the backend re-checks).
 *
 * @param groups - Groups by id.
 * @param selections - Selections.
 */
export function selectionIsSafe(groups: ReadonlyMap<number, DupeGroup>, selections: readonly DupeSelection[]): boolean {
  return selections.every((s) => {
    const g = groups.get(s.groupId);
    return g !== undefined && new Set(s.fileIds).size < g.files.length;
  });
}

/** User-facing keep reasons. */
export const KEEP_REASON_TEXT: Readonly<Record<KeepReason, string>> = {
  oldest: "oldest copy",
  shortest_path: "shortest path",
  not_in_downloads: "not in Downloads",
  user_rule: "your keep rule",
};
