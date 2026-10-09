/**
 * Commands the palette can run (SPEC §17), built from the current state so
 * labels like "Scan D:" match the real volume list.
 */
import { call, errorMessage } from "../lib/backend";
import type { Availability } from "../lib/commands";
import { COLOR_MODES } from "../lib/palette";
import { NO_FILTERS, hasFilters } from "../lib/types";
import { volumeName, type VolumeInfo } from "../lib/volumes";
import type { Services } from "../services";
import { useApp } from "../store/app";
import { useSettings } from "../store/settings";
import { FEATURE_VIEWS } from "../views/featureViews";
import { AREAS, VISUAL_VIEWS } from "../shell/areas";
import { shortcutOf } from "../shell/keymap";
import { useLayout } from "../shell/layout";
import { goToArea } from "../shell/navigation";
import { activeTab, closeTab, openTab, useTabs } from "../shell/tabs";

/** One palette command. */
export interface PaletteCommand {
  id: string;
  title: string;
  /** Group heading. */
  group: "Go to" | "View" | "Appearance" | "Volumes" | "Actions";
  shortcut?: string | undefined;
  availability: Availability;
  run(): void;
}

const ok: Availability = { enabled: true };

/**
 * Builds the palette's command list.
 *
 * @param services - Services (volume actions, capabilities).
 * @param volumes - Known volumes.
 * @returns Commands in display order.
 */
export function buildCommands(services: Services, volumes: readonly VolumeInfo[]): PaletteCommand[] {
  const app = useApp.getState();
  const settings = useSettings.getState();
  const hasVolume = app.volumeId !== null;
  const caps = services.capabilities();
  const need = (cmd: string, reason: string): Availability => (caps.has(cmd) ? ok : { enabled: false, reason });
  const out: PaletteCommand[] = [];

  const needVolume: Availability = hasVolume ? ok : { enabled: false, reason: "Open a scanned volume first." };
  for (const a of AREAS.filter((x) => x.id !== "settings")) {
    out.push({
      id: `area.${a.id}`,
      title: `Go to ${a.label}`,
      group: "Go to",
      shortcut: a.shortcut,
      availability: ok,
      run: () => {
        goToArea(a.id);
      },
    });
  }
  out.push({
    id: "view.home",
    title: "Show Volumes",
    group: "Go to",
    availability: ok,
    run: () => {
      useApp.getState().setView("home");
    },
  });
  for (const v of VISUAL_VIEWS) {
    out.push({
      id: `view.${v.id}`,
      title: `Show ${v.label}`,
      group: "Go to",
      shortcut: v.shortcut,
      availability: needVolume,
      run: () => {
        useApp.getState().setView(v.id);
        if (useLayout.getState().split === "list") useLayout.getState().set({ split: "split" });
      },
    });
  }
  // Settings and Largest files have their own "Actions" entries below.
  for (const v of FEATURE_VIEWS.filter((f) => f.id !== "settings" && f.id !== "largest")) {
    out.push({
      id: `view.${v.id}`,
      title: `Show ${v.label}`,
      group: "Go to",
      availability: !v.needsVolume || hasVolume ? ok : { enabled: false, reason: "Open a scanned volume first." },
      run: () => {
        useApp.getState().setView(v.id);
      },
    });
  }
  out.push({
    id: "nav.up",
    title: "Go up one folder",
    group: "Go to",
    shortcut: "Backspace",
    availability: app.path.length > 1 ? ok : { enabled: false, reason: "Already at the volume root." },
    run: () => {
      useApp.getState().goUp();
    },
  });
  for (const [pane, label, command] of [
    ["list", "list pane", "toggleList"],
    ["detail", "inspector", "toggleInspector"],
  ] as const) {
    out.push({
      id: `pane.${pane}`,
      title: `${app.panes[pane] ? "Hide" : "Show"} ${label}`,
      group: "View",
      shortcut: shortcutOf(command),
      availability: ok,
      run: () => {
        useApp.getState().togglePane(pane);
      },
    });
  }
  const layout = useLayout.getState();
  out.push({
    id: "pane.sidebar",
    title: `${layout.sidebarOpen ? "Hide" : "Show"} sidebar`,
    group: "View",
    shortcut: shortcutOf("toggleSidebar"),
    availability: ok,
    run: () => {
      const l = useLayout.getState();
      l.set({ sidebarOpen: !l.sidebarOpen });
    },
  });
  out.push({
    id: "tab.new",
    title: "Open the current folder in a new tab",
    group: "View",
    shortcut: shortcutOf("newTab"),
    availability: needVolume,
    run: () => {
      const s = useApp.getState();
      if (s.volumeId !== null) openTab(s.volumeId, s.path);
    },
  });
  out.push({
    id: "tab.close",
    title: "Close the tab",
    group: "View",
    shortcut: shortcutOf("closeTab"),
    availability: activeTab(useTabs.getState()) ? ok : { enabled: false, reason: "No tab is open." },
    run: () => {
      const t = activeTab(useTabs.getState());
      if (t) closeTab(t.id);
    },
  });
  out.push({
    id: "path.edit",
    title: "Edit the path",
    group: "Go to",
    shortcut: shortcutOf("editPath"),
    availability: needVolume,
    run: () => {
      const s = useApp.getState();
      if (s.view === "home") s.setView(s.lastVisual);
      useLayout.getState().setEditingPath(true);
    },
  });
  out.push({
    id: "help.shortcuts",
    title: "Show keyboard shortcuts",
    group: "Actions",
    shortcut: "?",
    availability: ok,
    run: () => {
      useLayout.getState().setCheatSheet(true);
    },
  });
  out.push({
    id: "size.toggle",
    title: app.sizeMode === "allocated" ? "Size mode: Logical" : "Size mode: On disk (allocated)",
    group: "View",
    availability: ok,
    run: () => {
      const s = useApp.getState();
      s.setSizeMode(s.sizeMode === "allocated" ? "logical" : "allocated");
    },
  });
  for (const m of COLOR_MODES) {
    out.push({
      id: `color.${m.id}`,
      title: `Color by ${m.label.toLowerCase()}`,
      group: "View",
      availability: ok,
      run: () => {
        useApp.getState().setColorMode(m.id);
      },
    });
  }
  out.push({
    id: "style.toggle",
    title: app.treemapStyle === "flat" ? "Treemap style: Cushion" : "Treemap style: Flat",
    group: "View",
    availability: ok,
    run: () => {
      const s = useApp.getState();
      s.setTreemapStyle(s.treemapStyle === "flat" ? "cushion" : "flat");
    },
  });
  out.push({
    id: "filters.clear",
    title: "Clear filters and show excluded items",
    group: "View",
    availability: hasFilters(app.filters) ? ok : { enabled: false, reason: "No filters are active." },
    run: () => {
      useApp.getState().setFilters(NO_FILTERS);
    },
  });
  for (const t of ["system", "light", "dark"] as const) {
    out.push({
      id: `theme.${t}`,
      title: t === "system" ? "Theme: Follow Windows" : `Theme: ${t === "light" ? "Light" : "Dark"}`,
      group: "Appearance",
      availability: ok,
      run: () => {
        useSettings.getState().setTheme(t);
      },
    });
  }
  out.push({
    id: "units.toggle",
    title: settings.units === "binary" ? "Units: SI (1000)" : "Units: Binary (1024, shown as KB/MB/GB)",
    group: "Appearance",
    availability: ok,
    run: () => {
      const s = useSettings.getState();
      s.setUnits(s.units === "binary" ? "si" : "binary");
    },
  });
  out.push({
    id: "patterns.toggle",
    title: settings.patterns ? "Turn off color-blind patterns" : "Turn on color-blind patterns",
    group: "Appearance",
    availability: ok,
    run: () => {
      const s = useSettings.getState();
      s.setPatterns(!s.patterns);
    },
  });
  for (const v of volumes) {
    out.push({
      id: `scan.${v.id}`,
      title: `Scan ${volumeName(v)}`,
      group: "Volumes",
      availability: v.bitlocker === "locked" ? { enabled: false, reason: "Volume is BitLocker-locked." } : need("scan_start", "Scanning is not connected in this build yet."),
      run: () => {
        void services.volumes.startScan(v.id, "auto").catch(() => undefined);
      },
    });
  }
  out.push({
    id: "helper.elevate",
    title: "Enable fast scan (administrator)",
    group: "Volumes",
    availability: need("helper_elevate", "The elevated helper is not part of this build yet."),
    run: () => {
      void services.volumes.elevate().catch(() => undefined);
    },
  });
  out.push({
    id: "recycle.empty",
    title: "Empty Recycle Bin",
    group: "Actions",
    availability: need("recycle_bin_empty", "Cleanup tools are not part of this build yet."),
    run: () => {
      // The backend shows its own confirmation before emptying (SPEC §15.6).
      call<null>("recycle_bin_empty").catch((err: unknown) => {
        useApp.getState().notify(errorMessage(err));
      });
    },
  });
  out.push({
    id: "settings.open",
    title: "Open settings",
    group: "Actions",
    shortcut: "Ctrl+,",
    availability: ok,
    run: () => {
      useApp.getState().setView("settings");
    },
  });
  out.push({
    id: "largest.show",
    title: "Show largest files",
    group: "Actions",
    availability: hasVolume ? ok : { enabled: false, reason: "Open a scanned volume first." },
    run: () => {
      useApp.getState().setView("largest");
    },
  });
  return out;
}
