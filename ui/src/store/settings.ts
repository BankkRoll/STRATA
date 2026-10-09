/**
 * User-facing appearance settings.
 *
 * Persistence moves to the SQLite settings store in M13; until then these live
 * only for the session.
 */
import { create } from "zustand";

/** Theme preference; `system` follows the Windows light/dark setting. */
export type ThemePreference = "system" | "light" | "dark";

/** Appearance settings state and setters. */
export interface SettingsState {
  theme: ThemePreference;
  setTheme: (theme: ThemePreference) => void;
}

/** Global settings store. */
export const useSettings = create<SettingsState>()((set) => ({
  theme: "system",
  setTheme: (theme) => {
    set({ theme });
  },
}));
