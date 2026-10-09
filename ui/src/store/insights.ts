/**
 * Filters shared between insight views, so "click a file type" or "click a
 * category" can open Largest files already filtered.
 */
import { create } from "zustand";
import { NO_LARGEST_FILTERS, type LargestFilters } from "../lib/insights";

/** Shared insight state. */
export interface InsightsState {
  largest: LargestFilters;
  setLargest: (f: LargestFilters) => void;
}

/** Shared insight filters. */
export const useInsights = create<InsightsState>()((set) => ({
  largest: NO_LARGEST_FILTERS,
  setLargest(largest) {
    set({ largest });
  },
}));
