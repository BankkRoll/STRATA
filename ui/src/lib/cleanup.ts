/**
 * Cleanup queue, review, pre-flight, execution and undo.
 *
 * The queue lives in the backend: it resolves entry ids to paths, scan
 * identities (file reference, size, mtime) and safety tiers, and refuses
 * never-tier and never-list items authoritatively. The UI mirrors it through
 * `cleanup_queue_list` and the `cleanup://queue-changed` event.
 *
 * Responsibilities:
 * - Typed contracts for every cleanup command (the backend maps
 *   `strata_clean::flow` types onto these camelCase DTOs; tagged enums keep
 *   the Rust serde tags).
 * - Pure review rules ({@link reviewBlockers}) shared by the review screen
 *   and its tests, so "can I press Delete?" has one definition.
 */
import { call } from "./backend";
import { callWithChannel, listenEvent } from "./bridge";
import type { Safety } from "./types";

// -----------------------------------------------------------------------------
// Queue
// -----------------------------------------------------------------------------

/** Where an item was queued from. */
export type QueueSource = "manual" | "recommendation" | "duplicates" | "app_caches";

/** One item in the cleanup queue. */
export interface QueueEntry {
  /** Queue item id (stable across plan, pre-flight and execute). */
  id: number;
  volumeId: string;
  /** Index entry id at the time it was queued. */
  entryId: number;
  /** Full Win32 path. */
  path: string;
  name: string;
  isDir: boolean;
  /** Scan size in bytes (allocated for the item or folder subtree). */
  bytes: number;
  safety: Safety;
  /** `strata_core::Category` discriminant. */
  category: number;
  /** Classifier rule that decided the tier, or `null` when unclassified. */
  ruleId: string | null;
  ruleName: string | null;
  /** Why the item is in its tier (rule explanation). */
  explain: string;
  /** Deleting it is undone by the app re-creating it. */
  regenerable: boolean;
  /** Owning app, when attributed. */
  app: string | null;
  /** Unix ms when it was queued. */
  addedMs: number;
  source: QueueSource;
}

/** Why the backend refused to queue an entry. */
export type QueueRefusalReason =
  | "never_tier"
  | "never_list"
  | "already_queued"
  | "inside_queued"
  | "not_found"
  | "virtual"
  | "unverifiable";

/** An entry that was not queued, with the user-facing reason. */
export interface QueueRefusal {
  entryId: number;
  path: string;
  reason: QueueRefusalReason;
  /** Sentence shown verbatim, e.g. "C:\Windows is part of Windows and is never deleted." */
  message: string;
}

/** Result of adding to the queue. */
export interface QueueAddResult {
  added: QueueEntry[];
  refused: QueueRefusal[];
}

/** Lists the queue (`cleanup_queue_list`). */
export function fetchQueue(): Promise<QueueEntry[]> {
  return call<QueueEntry[]>("cleanup_queue_list");
}

/**
 * Adds entries (`cleanup_queue_add`). Older builds resolve `null`; that is
 * treated as "added, nothing refused".
 *
 * @param volumeId - Volume of the entries.
 * @param ids - Entry ids.
 * @returns What was added and what was refused (with reasons).
 */
export async function addToQueue(volumeId: string, ids: number[]): Promise<QueueAddResult> {
  const r = await call<QueueAddResult | null>("cleanup_queue_add", { volumeId, ids });
  return r ?? { added: [], refused: [] };
}

/** Removes queue items (`cleanup_queue_remove`). */
export function removeFromQueue(ids: number[]): Promise<null> {
  return call<null>("cleanup_queue_remove", { ids });
}

/** Empties the queue (`cleanup_queue_clear`). */
export function clearQueue(): Promise<null> {
  return call<null>("cleanup_queue_clear");
}

/**
 * Subscribes to `cleanup://queue-changed` (payload: the full queue).
 *
 * @param onChange - Receives the queue.
 * @returns Unsubscribe.
 */
export function watchQueue(onChange: (queue: QueueEntry[]) => void): () => void {
  return listenEvent<QueueEntry[]>("cleanup://queue-changed", onChange);
}

/**
 * Summarizes a refusal list for a status message.
 *
 * @param r - Add result.
 * @returns A sentence, or `null` when nothing was refused.
 */
export function describeRefusals(r: QueueAddResult): string | null {
  if (r.refused.length === 0) return null;
  const first = r.refused[0] as QueueRefusal;
  if (r.refused.length === 1) return `Not added: ${first.message}`;
  return `${r.refused.length} items not added. ${first.message}`;
}

// -----------------------------------------------------------------------------
// Plan (review)
// -----------------------------------------------------------------------------

/** Delete method (`strata_clean::DeleteMethod`). */
export type DeleteMethod = "recycle_bin" | "permanent";

/** Why a volume or item has no usable Recycle Bin (`RecycleUnavailable`). */
export type RecycleUnavailable =
  | "removable_drive"
  | "network_drive"
  | "optical_drive"
  | "ram_disk"
  | "disabled_for_volume"
  | "no_recycle_bin"
  | "unknown_volume"
  | "path_too_long";

/** Recycle Bin support of a volume (`RecycleBinSupport`). */
export type RecycleBinSupport =
  | { state: "available"; capacity: number | null; used: number | null }
  | { state: "unavailable"; reason: RecycleUnavailable };

/** Whether one item fits in the Recycle Bin (`RecycleFit`). */
export type RecycleFit =
  | { fit: "fits" }
  | { fit: "unknown" }
  | { fit: "too_large"; capacity: number }
  | { fit: "unavailable"; reason: RecycleUnavailable };

/** Items and bytes per tier. */
export interface TierTotal {
  safety: Safety;
  items: number;
  bytes: number;
}

/** Items on one volume with its Recycle Bin support. */
export interface PlanVolume {
  mountPoint: string;
  recycleBin: RecycleBinSupport;
  items: number;
  bytes: number;
}

/** Restart Manager app type (`locks::AppKind`). */
export type AppKind = "unknown" | "main_window" | "other_window" | "service" | "explorer" | "console" | "critical";

/** A process holding a file open (`locks::LockHolder`). */
export interface LockHolder {
  pid: number;
  /** Process start time (FILETIME as a number); with `pid` identifies the process. */
  startTime: number;
  appName: string;
  exePath: string | null;
  /** Service short name when the holder is a service. */
  service: string | null;
  kind: AppKind;
  restartable: boolean;
}

/** "Close X first" warning (`apps::RunningAppWarning`). */
export interface RunningAppWarning {
  app: string;
  pids: number[];
  reason: "owns_cache" | "holds_files";
}

/** Something the review screen must show (`flow::PlanWarning`). */
export type PlanWarning =
  | { kind: "refused"; id: number; message: string }
  | { kind: "duplicate"; id: number }
  | { kind: "nested"; id: number; inside: number }
  | { kind: "never_tier"; id: number }
  | { kind: "needs_acknowledgement"; id: number }
  | { kind: "cannot_recycle"; id: number; fit: RecycleFit }
  | { kind: "running_app"; id: number; warning: RunningAppWarning };

/** The reviewed plan (`flow::Plan` plus the thresholds the UI confirms). */
export interface CleanupPlan {
  /** Backend handle for pre-flight / execute / retry. */
  planId: number;
  /** Items pre-flight and execute will consider (never-list hits, duplicates and nested items removed). */
  items: QueueEntry[];
  totals: TierTotal[];
  volumes: PlanVolume[];
  warnings: PlanWarning[];
  /** `cleanup.large_delete_confirm_bytes`: permanent deletes above it need a second confirmation. */
  largeDeleteBytes: number;
  /** `cleanup.default_method`. */
  defaultMethod: DeleteMethod;
}

/** Confirmations the user gave on the review screen (`flow::Acknowledgements`). */
export interface Acknowledgements {
  /** Careful-tier queue ids the user ticked. */
  careful: number[];
  /** The extra confirmation for permanent deletion. */
  permanent: boolean;
  /** The second confirmation for permanent deletes above the threshold. */
  largePermanent: boolean;
  /** Items that cannot be recycled which the user chose to delete permanently. */
  permanentInsteadOfRecycle: number[];
}

/** Method plus confirmations (`flow::Decision`) plus items the user skipped. */
export interface Decision {
  method: DeleteMethod;
  acks: Acknowledgements;
  /** Queue ids deselected on the review screen or skipped in pre-flight; the backend drops them. */
  skip: number[];
}

/**
 * Builds the review plan for the selected queue items (`cleanup_plan`).
 *
 * @param queueIds - Queue ids to review (all, unless the user narrowed it).
 * @returns The plan.
 */
export function planCleanup(queueIds: number[]): Promise<CleanupPlan> {
  return call<CleanupPlan>("cleanup_plan", { queueIds });
}

// -----------------------------------------------------------------------------
// Pre-flight and execution
// -----------------------------------------------------------------------------

/**
 * A typed cleanup failure (`strata_clean::CleanError`) flattened for display:
 * `kind` is the serde tag, `message` is `CleanError::message()`.
 */
export interface CleanErrorInfo {
  kind:
    | "refused"
    | "not_found"
    | "changed"
    | "locked"
    | "access_denied"
    | "recycle_bin_unavailable"
    | "too_large_for_recycle_bin"
    | "would_delete_permanently"
    | "never_tier"
    | "needs_permanent_confirmation"
    | "audit_log_failed"
    | "needs_acknowledgement"
    | "needs_large_delete_confirmation"
    | "cancelled"
    | "partial"
    | "os";
  message: string;
  /** `CleanError::is_retryable()`. */
  retryable: boolean;
  path: string | null;
  /** Lock holders for `locked`. */
  holders: LockHolder[];
}

/** Pre-flight result (`preflight::Verdict`). */
export type Verdict = { status: "ready"; recycle: RecycleFit } | { status: "blocked"; error: CleanErrorInfo };

/** Pre-flight result for one item (`preflight::ItemVerdict`). */
export interface ItemVerdict {
  id: number;
  path: string;
  verdict: Verdict;
  holders: LockHolder[];
  runningApps: RunningAppWarning[];
}

/**
 * Re-verifies every planned item right before acting (`cleanup_preflight`):
 * existence, identity (TOCTOU), locks and Recycle Bin fit.
 *
 * @param planId - Plan handle.
 * @param decision - Current method and confirmations.
 * @returns One verdict per non-skipped item.
 */
export function preflightCleanup(planId: number, decision: Decision): Promise<ItemVerdict[]> {
  return call<ItemVerdict[]>("cleanup_preflight", { planId, decision });
}

/** A consent prompt minted by the backend for a polite close (`consent::Prompt`). */
export interface ClosePrompt {
  promptId: number;
  /** Text to show verbatim before the user confirms. */
  message: string;
  app: string;
  pid: number;
  /** Unix ms after which the prompt is void (backend consents expire after 120 s). */
  expiresMs: number;
}

/** What a polite close did (`locks::CloseOutcome`). */
export type CloseOutcome = { kind: "asked_windows"; windows: number } | { kind: "shut_down" };

/**
 * Asks the backend for the consent text to close a lock holder politely
 * (`cleanup_close_prompt`). Nothing is closed by this call.
 */
export function prepareClose(holder: Pick<LockHolder, "pid" | "startTime">): Promise<ClosePrompt> {
  return call<ClosePrompt>("cleanup_close_prompt", { pid: holder.pid, startTime: holder.startTime });
}

/**
 * Closes the app politely (WM_CLOSE / RmShutdown) after the user confirmed
 * the prompt (`cleanup_close_app`). Never a silent kill.
 */
export function closeApp(promptId: number): Promise<CloseOutcome> {
  return call<CloseOutcome>("cleanup_close_app", { promptId });
}

/** What happened to one item (`audit::ItemOutcome`, ticket/stats flattened). */
export type ItemOutcome =
  | { kind: "recycled"; restoreItemId: number | null }
  | { kind: "deleted"; bytes: number }
  | { kind: "failed"; error: CleanErrorInfo }
  | { kind: "skipped"; reason: CleanErrorInfo };

/** Per-item outcome. */
export interface ItemResult {
  id: number;
  path: string;
  outcome: ItemOutcome;
}

/** Totals of a finished action (`audit::ActionSummary`). */
export interface ActionSummary {
  succeeded: number;
  failed: number;
  skipped: number;
  /** Bytes freed (scan sizes of removed items). */
  bytes: number;
  cancelled: boolean;
}

/** Result of `cleanup_execute`. */
export interface ExecutionReport {
  /** Undo-log action id, when the log accepted the action. */
  actionId: number | null;
  results: ItemResult[];
  summary: ActionSummary;
}

/** Progress events streamed during execution (`flow::Progress`). */
export type CleanupProgress =
  | { event: "started"; items: number; bytes: number }
  | { event: "item_started"; id: number }
  | { event: "item_finished"; id: number; removed: boolean }
  | { event: "finished"; summary: ActionSummary };

/**
 * Executes the plan (`cleanup_execute`), streaming progress. The backend
 * writes the undo log before each item acts and never falls back from
 * recycling to permanent deletion on its own.
 *
 * @param planId - Plan handle.
 * @param decision - Method, confirmations and skipped items.
 * @param onProgress - Progress events in order.
 * @returns The per-item report.
 */
export function executeCleanup(planId: number, decision: Decision, onProgress: (p: CleanupProgress) => void): Promise<ExecutionReport> {
  return callWithChannel<ExecutionReport, CleanupProgress>("cleanup_execute", "onProgress", { planId, decision }, onProgress);
}

/** Cancels a running execution (`cleanup_cancel`); items already acted on stay done. */
export function cancelCleanup(planId: number): Promise<null> {
  return call<null>("cleanup_cancel", { planId });
}

/**
 * Builds a plan of the retryable failures of a finished run
 * (`cleanup_retry_plan`, `Plan::retry`). Pre-flight runs again before it acts.
 */
export function retryPlan(planId: number): Promise<CleanupPlan> {
  return call<CleanupPlan>("cleanup_retry_plan", { planId });
}

/**
 * Schedules a stubborn locked file for deletion at the next restart
 * (`cleanup_delete_on_reboot`; helper only, plain single-link files).
 */
export function deleteOnReboot(planId: number, id: number): Promise<null> {
  return call<null>("cleanup_delete_on_reboot", { planId, id });
}

// -----------------------------------------------------------------------------
// Undo history and restore
// -----------------------------------------------------------------------------

/** Undo-log action kind. */
export type ActionKind = "cleanup" | "duplicates" | "tool";

/** Undo-log action status. */
export type ActionStatus = "in_progress" | "completed" | "partial" | "failed" | "cancelled" | "interrupted";

/** One item of a logged action (`strata_store::ItemRecord`). */
export interface UndoItem {
  itemId: number;
  path: string;
  bytes: number;
  method: "recycle" | "permanent" | "reboot_delete" | "tool";
  tier: Safety;
  result: "pending" | "done" | "failed" | "skipped";
  error: string | null;
  completedMs: number | null;
  /** In the Recycle Bin with a restore ticket and not yet restored. */
  restorable: boolean;
  restoredMs: number | null;
}

/** One logged action (`strata_store::ActionRecord`). */
export interface UndoAction {
  actionId: number;
  kind: ActionKind;
  status: ActionStatus;
  startedMs: number;
  finishedMs: number | null;
  itemCount: number;
  doneCount: number;
  failedCount: number;
  bytesDone: number;
  items: UndoItem[];
}

/** Result of restoring one item. */
export interface RestoreResult {
  itemId: number;
  ok: boolean;
  /** Why it failed (e.g. "Something already exists at the original location"); `null` on success. */
  message: string | null;
}

/**
 * Pages the undo history, newest first (`cleanup_history`).
 *
 * @param limit - Page size.
 * @param beforeActionId - Continue below this action id, or `null` for the newest.
 */
export function fetchUndoHistory(limit: number, beforeActionId: number | null): Promise<UndoAction[]> {
  return call<UndoAction[]>("cleanup_history", { limit, beforeActionId });
}

/** Restores recycled items to their original paths (`cleanup_restore`); never overwrites. */
export function restoreItems(itemIds: number[]): Promise<RestoreResult[]> {
  return call<RestoreResult[]>("cleanup_restore", { itemIds });
}

// -----------------------------------------------------------------------------
// Review rules
// -----------------------------------------------------------------------------

/** What still has to happen before the user may execute. */
export interface ReviewBlocker {
  /** Queue id the blocker is about, or `null` for plan-wide ones. */
  id: number | null;
  message: string;
}

/**
 * Lists everything that blocks execution, mirroring the backend gate
 * (`flow::gate`) so the Delete button is disabled for the same reasons the
 * backend would refuse. The backend re-checks everything; this is UX only.
 *
 * @param plan - Reviewed plan.
 * @param decision - Current decision.
 * @returns Blockers; empty means ready.
 */
export function reviewBlockers(plan: CleanupPlan, decision: Decision): ReviewBlocker[] {
  const out: ReviewBlocker[] = [];
  const skip = new Set(decision.skip);
  const careful = new Set(decision.acks.careful);
  const instead = new Set(decision.acks.permanentInsteadOfRecycle);
  const active = plan.items.filter((i) => !skip.has(i.id) && i.safety !== "never");
  if (active.length === 0) out.push({ id: null, message: "Select at least one item." });
  for (const item of active) {
    if (item.safety === "careful" && !careful.has(item.id)) {
      out.push({ id: item.id, message: `Confirm you reviewed “${item.name}” (Careful).` });
    }
  }
  const permanentIds = new Set<number>();
  if (decision.method === "permanent") {
    for (const i of active) permanentIds.add(i.id);
    if (!decision.acks.permanent) out.push({ id: null, message: "Confirm permanent deletion." });
  } else {
    for (const w of plan.warnings) {
      if (w.kind !== "cannot_recycle" || skip.has(w.id)) continue;
      if (!active.some((i) => i.id === w.id)) continue;
      if (instead.has(w.id)) permanentIds.add(w.id);
      else out.push({ id: w.id, message: "Choose “Delete permanently” or “Skip” for an item the Recycle Bin can’t take." });
    }
    if (permanentIds.size > 0 && !decision.acks.permanent) out.push({ id: null, message: "Confirm permanent deletion of the items the Recycle Bin can’t take." });
  }
  const large = active.filter((i) => permanentIds.has(i.id) && i.bytes > plan.largeDeleteBytes);
  if (large.length > 0 && !decision.acks.largePermanent) {
    out.push({ id: null, message: `Confirm again: ${large.length === 1 ? "one item is" : `${large.length} items are`} above the large-delete threshold.` });
  }
  return out;
}

/**
 * Totals by tier over the items that will be acted on.
 *
 * @param items - Items.
 * @param skip - Skipped queue ids.
 * @returns One total per tier in safe → never order (zero rows included).
 */
export function tierTotals(items: readonly QueueEntry[], skip: ReadonlySet<number> = new Set()): TierTotal[] {
  return (["safe", "probably", "careful", "never"] as const).map((safety) => {
    const of = items.filter((i) => i.safety === safety && !skip.has(i.id));
    return { safety, items: of.length, bytes: of.reduce((a, i) => a + i.bytes, 0) };
  });
}

/** User-facing reason a Recycle Bin is unavailable. */
export const RECYCLE_UNAVAILABLE_TEXT: Readonly<Record<RecycleUnavailable, string>> = {
  removable_drive: "Removable drives have no Recycle Bin.",
  network_drive: "Network drives have no Recycle Bin.",
  optical_drive: "Optical drives have no Recycle Bin.",
  ram_disk: "RAM disks have no Recycle Bin.",
  disabled_for_volume: "The Recycle Bin is turned off for this drive (items are deleted immediately).",
  no_recycle_bin: "Windows reports no Recycle Bin for this drive.",
  unknown_volume: "The drive could not be identified.",
  path_too_long: "The path is too long for the Recycle Bin.",
};
