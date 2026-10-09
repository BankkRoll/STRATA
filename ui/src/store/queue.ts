/**
 * Mirror of the backend cleanup queue for the nav badge, the queue view and
 * the "already queued" state of add buttons. The backend owns the queue;
 * this store is replaced wholesale on every `cleanup://queue-changed`.
 */
import { useEffect } from "react";
import { create } from "zustand";
import { useFeatures } from "../features";
import { BackendUnavailableError, errorMessage } from "../lib/backend";
import { describeRefusals, type QueueAddResult, type QueueEntry } from "../lib/cleanup";
import { useApp } from "./app";

/** Queue store shape. */
export interface QueueState {
  /** `null` until loaded or when the queue is not in this build. */
  items: QueueEntry[] | null;
  /** Refusals from the latest add, shown in the queue view ("why not added"). */
  refused: QueueAddResult["refused"];
  setItems: (items: QueueEntry[] | null) => void;
  /** Applies an add result: merges added items and records refusals. */
  applyAdd: (r: QueueAddResult) => void;
}

/** The queue store. */
export const useQueue = create<QueueState>()((set) => ({
  items: null,
  refused: [],
  setItems(items) {
    set({ items });
  },
  applyAdd(r) {
    set((s) => {
      const have = new Set((s.items ?? []).map((i) => i.id));
      return { items: [...(s.items ?? []), ...r.added.filter((i) => !have.has(i.id))], refused: r.refused };
    });
  },
}));

/**
 * Applies an add result to the store and describes it for the user.
 *
 * @param r - Add result.
 * @returns Status message (added count and refusal reasons).
 */
export function applyAddResult(r: QueueAddResult): string {
  useQueue.getState().applyAdd(r);
  const refused = describeRefusals(r);
  const added = r.added.length;
  const parts = [added > 0 ? `${added === 1 ? "1 item" : `${added} items`} added to the cleanup queue.` : "", refused ?? ""].filter(Boolean);
  // Both lists empty only happens with a backend that answers `null` (no details).
  return parts.join(" ") || "Added to the cleanup queue.";
}

/**
 * Applies an add result and reports it in the status bar.
 *
 * @param r - Add result.
 */
export function reportAdd(r: QueueAddResult): void {
  useApp.getState().notify(applyAddResult(r));
}

/**
 * Loads the queue and follows `cleanup://queue-changed`. Mounted once by the
 * shell. Not gated on capabilities: they may still be loading at startup, and
 * a build without the queue rejects with BackendUnavailableError, which keeps
 * the store at `null` ("not available").
 */
export function useQueueSync(): void {
  const features = useFeatures();
  useEffect(() => {
    let cancelled = false;
    features.cleanup
      .fetchQueue()
      .then((items) => {
        if (!cancelled) useQueue.getState().setItems(items);
      })
      .catch((err: unknown) => {
        if (!cancelled && !(err instanceof BackendUnavailableError)) useApp.getState().notify(`Couldn’t load the cleanup queue: ${errorMessage(err)}`);
      });
    const un = features.cleanup.watchQueue((items) => {
      useQueue.getState().setItems(items);
    });
    return () => {
      cancelled = true;
      un();
    };
  }, [features]);
}
