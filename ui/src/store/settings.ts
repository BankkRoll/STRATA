/**
 * User-facing appearance settings.
 *
 * Persistence moves to the SQLite settings store (`strata-store`,
 * `appearance.*` keys) when the settings commands are wired; until then these
 * live only for the session.
 */
import { create } from "zustand";
import type { SizeUnits } from "../lib/format";

/** Theme preference; `system` follows the Windows light/dark setting. */
export type ThemePreference = "system" | "light" | "dark";

/** Appearance settings state and setters. */
export interface SettingsState {
  theme: ThemePreference;
  /** Size units (`appearance.units`): binary shown as KB/MB/GB, or SI. */
  units: SizeUnits;
  /** Overlay category patterns for color-blind users (SPEC §12.5). */
  patterns: boolean;
  setTheme: (theme: ThemePreference) => void;
  setUnits: (units: SizeUnits) => void;
  setPatterns: (on: boolean) => void;
}

/** Global settings store. */
export const useSettings = create<SettingsState>()((set) => ({
  theme: "system",
  units: "binary",
  patterns: false,
  setTheme: (theme) => {
    set({ theme });
  },
  setUnits: (units) => {
    set({ units });
  },
  setPatterns: (patterns) => {
    set({ patterns });
  },
}));
