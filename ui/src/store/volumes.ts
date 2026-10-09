/**
 * Volume list and helper state, shared by the home screen, breadcrumbs and
 * status bar. Filled from `list_volumes` and kept current by the
 * `volumes://changed` event.
 */
import { create } from "zustand";
import type { HelperStatus, VolumeInfo } from "../lib/volumes";

/** Volume store shape. */
export interface VolumesState {
  /** `null` until the first load completes. */
  volumes: VolumeInfo[] | null;
  helper: HelperStatus | null;
  /** User-facing reason the list could not be loaded, or `null`. */
  error: string | null;
  /** The backend has no volume discovery in this build (designed empty state, not an error). */
  unavailable: boolean;
  setVolumes: (volumes: VolumeInfo[]) => void;
  setHelper: (helper: HelperStatus | null) => void;
  setError: (error: string | null, unavailable?: boolean) => void;
}

/** The volume store. */
export const useVolumes = create<VolumesState>()((set) => ({
  volumes: null,
  helper: null,
  error: null,
  unavailable: false,
  setVolumes(volumes) {
    set({ volumes, error: null });
  },
  setHelper(helper) {
    set({ helper });
  },
  setError(error, unavailable = false) {
    set({ error, unavailable });
  },
}));
