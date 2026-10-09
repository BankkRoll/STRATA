/**
 * Bridges persisted settings (`settings_load`) to the live UI stores: the
 * appearance section drives theme, units, color mode, treemap style and the
 * default size mode.
 */
import { useEffect } from "react";
import { useFeatures } from "../features";
import type { ColorMode } from "../lib/palette";
import type { Settings } from "../lib/settings";
import { useApp } from "./app";
import { useSettings } from "./settings";

const COLOR: Readonly<Record<Settings["appearance"]["color_mode"], ColorMode>> = {
  category: "category",
  age: "age",
  file_type: "fileType",
  safety: "safety",
  app: "app",
};

/**
 * Applies the appearance and default size mode to the UI stores.
 *
 * @param s - Persisted settings.
 */
export function applyAppearance(s: Settings): void {
  const ui = useSettings.getState();
  ui.setTheme(s.appearance.theme);
  ui.setUnits(s.appearance.units === "decimal" ? "si" : "binary");
  const app = useApp.getState();
  app.setColorMode(COLOR[s.appearance.color_mode]);
  app.setTreemapStyle(s.appearance.treemap_style);
  app.setSizeMode(s.scan.default_size_mode);
  document.documentElement.toggleAttribute("data-compact", s.appearance.compact_density);
}

/**
 * Loads settings once at startup and applies the appearance. Silent when
 * the build has no settings store (session defaults stay).
 */
export function useAppearanceSync(): void {
  const features = useFeatures();
  useEffect(() => {
    let cancelled = false;
    features.settings.loadSettings().then(
      (s) => {
        if (!cancelled) applyAppearance(s);
      },
      () => {
        // No persisted settings in this build: keep the session defaults.
      },
    );
    return () => {
      cancelled = true;
    };
  }, [features]);
}
