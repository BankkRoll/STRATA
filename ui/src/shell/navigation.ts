/**
 * Navigation between areas, shared by the activity bar, the keyboard map
 * and the palette so they all behave the same way.
 */
import { useEffect } from "react";
import { isVisualView, useApp } from "../store/app";
import { areaInfo, areaOf, type AreaId } from "./areas";
import { useLayout } from "./layout";

/**
 * Opens an area at the view the user last had there.
 *
 * @param area - Target area.
 * @param fromBar - Invoked by clicking the activity bar; clicking the area
 *   that is already open shows or hides the sidebar instead, as in most
 *   desktop tools.
 */
export function goToArea(area: AreaId, fromBar = false): void {
  const app = useApp.getState();
  const layout = useLayout.getState();
  if (fromBar && areaOf(app.view) === area && area !== "settings") {
    layout.set({ sidebarOpen: !layout.sidebarOpen });
    return;
  }
  let target = layout.lastView[area] ?? areaInfo(area).defaultView;
  if (area === "explore" && isVisualView(target) && app.volumeId === null) target = "home";
  app.setView(target);
}

/**
 * Opens Settings, optionally scrolled to one category.
 *
 * @param category - Settings category id.
 */
export function openSettings(category: string | null = null): void {
  useLayout.setState({ settingsFocus: category });
  useApp.getState().setView("settings");
}

/** Records the last view of each area as the user moves around. */
export function useLastViewTracking(): void {
  useEffect(
    () =>
      useApp.subscribe((s, prev) => {
        if (s.view === prev.view) return;
        const area = areaOf(s.view);
        const layout = useLayout.getState();
        if (layout.lastView[area] !== s.view) layout.set({ lastView: { ...layout.lastView, [area]: s.view } });
      }),
    [],
  );
}
