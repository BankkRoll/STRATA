/**
 * App updates found by the backend's updater (`strata://update-ready`,
 * `strata://update-available`). A downloaded update installs on exit or
 * right away through "Restart now" (`updates_restart`).
 */
import { useEffect } from "react";
import { create } from "zustand";
import { watchUpdates, type UpdateInfo } from "../lib/settings";
import { useApp } from "./app";

/** Update store shape. */
export interface UpdatesState {
  /** `ready`: downloaded, restart to install; `available`: found, not downloaded. */
  state: "ready" | "available" | null;
  info: UpdateInfo | null;
  set: (state: "ready" | "available", info: UpdateInfo) => void;
}

/** The update store. */
export const useUpdates = create<UpdatesState>()((set) => ({
  state: null,
  info: null,
  set(state, info) {
    set({ state, info });
  },
}));

/** Follows the updater's events and announces them. Mounted once by the shell. */
export function useUpdateSync(): void {
  useEffect(
    () =>
      watchUpdates((state, info) => {
        useUpdates.getState().set(state, info);
        useApp
          .getState()
          .notify(
            state === "ready"
              ? `Strata ${info.version} is ready. Restart to install it (palette: "Restart to update").`
              : `Strata ${info.version} is available. Check for updates in Settings to download it.`,
          );
      }),
    [],
  );
}
