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
import { NAV_VIEWS } from "./NavRail";

/** One palette command. */
export interface PaletteCommand {
  id: string;
  title: string;
  /** Group heading. */
  group: "Go to" | "View" | "Appearance" | "Volumes" | "Actions";
  shortcut?: string;
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

  for (const v of NAV_VIEWS) {
    out.push({
      id: `view.${v.id}`,
      title: `Show ${v.label}`,
      group: "Go to",
      shortcut: v.shortcut,
      availability: v.id === "home" || hasVolume ? ok : { enabled: false, reason: "Open a scanned volume first." },
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
  for (const [pane, label] of [
    ["list", "list pane"],
    ["detail", "details pane"],
    ["nav", "navigation"],
  ] as const) {
    out.push({
      id: `pane.${pane}`,
      title: `${app.panes[pane] ? "Hide" : "Show"} ${label}`,
      group: "View",
      availability: ok,
      run: () => {
        useApp.getState().togglePane(pane);
      },
    });
  }
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
    availability: { enabled: false, reason: "The settings screen is not part of this build yet." },
    run: () => undefined,
  });
  out.push({
    id: "largest.show",
    title: "Show largest files",
    group: "Actions",
    availability: { enabled: false, reason: "The largest-files view is not part of this build yet." },
    run: () => undefined,
  });
  return out;
}
