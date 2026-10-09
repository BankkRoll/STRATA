/**
 * App shell (SPEC §16.4): top bar, left nav, main visual area with a split
 * list pane, right detail panel and status bar. Panels collapse responsively
 * down to 900×600 (the detail panel becomes a drawer, the nav an icon rail).
 *
 * Global keyboard: Ctrl+K / Ctrl+F palette, Ctrl+1–7 views, F6 / Shift+F6
 * cycle regions, and the entry-action shortcuts listed in the context menu.
 */
import { Suspense, lazy, useCallback, useEffect, useState } from "react";
import type { AppInfo } from "../lib/backend";
import { ENTRY_ACTIONS, type EntryAction } from "../lib/commands";
import { useMediaQuery } from "../lib/hooks";
import type { ContextMenuRequest } from "../render/ViewController";
import { useServices } from "../services";
import { isVisualView, useApp } from "../store/app";
import { useAppearanceSync } from "../store/prefs";
import { useQueueSync } from "../store/queue";
import { DetailPanel } from "../views/DetailPanel";
import { Home, useVolumeSync } from "../views/Home";
import { ListPane } from "../views/ListPane";
import { VisualPane } from "../views/VisualPane";
import { FeatureRouter, featureInfo, isFeatureView } from "../views/featureViews";
import { ContextMenu, type ContextMenuState } from "./ContextMenu";
import { NAV_VIEWS, NavRail } from "./NavRail";
import { StatusBar } from "./StatusBar";
import { Splitter } from "./Splitter";
import { TopBar } from "./TopBar";

const CommandPalette = lazy(() => import("./CommandPalette").then((m) => ({ default: m.CommandPalette })));

/** Props for {@link AppShell}. */
export interface AppShellProps {
  info: AppInfo | null;
}

const REGIONS = [".topbar", ".nav", ".visual__surface, .home, .fview", ".list [role=treegrid]", ".detail"];

function isTyping(el: EventTarget | null): boolean {
  return el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement || el instanceof HTMLSelectElement || (el instanceof HTMLElement && el.isContentEditable);
}

/** The whole window's layout. */
export function AppShell({ info }: AppShellProps) {
  const services = useServices();
  useVolumeSync();
  useQueueSync();
  useAppearanceSync();
  const view = useApp((s) => s.view);
  const volumeId = useApp((s) => s.volumeId);
  const root = useApp((s) => s.path[s.path.length - 1] ?? null);
  const panes = useApp((s) => s.panes);
  const primary = useApp((s) => s.primary);
  const setPane = useApp((s) => s.setPane);
  const [palette, setPalette] = useState<string | null>(null);
  const [menu, setMenu] = useState<ContextMenuState | null>(null);
  const [listHeight, setListHeight] = useState(260);
  const narrow = useMediaQuery("(max-width: 1099px)");
  const short = useMediaQuery("(max-height: 699px)");

  const openMenu = useCallback(
    (req: ContextMenuRequest) => {
      if (volumeId === null || req.ids.length === 0) return;
      setMenu({ target: { volumeId, ids: req.ids }, x: req.clientX, y: req.clientY });
    },
    [volumeId],
  );

  const closeMenu = useCallback(() => {
    setMenu(null);
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.ctrlKey && !e.altKey && (e.key === "k" || e.key === "K" || e.key === "f" || e.key === "F")) {
        e.preventDefault();
        setPalette("");
        return;
      }
      if (e.ctrlKey && !e.shiftKey && /^[1-7]$/.test(e.key)) {
        const v = NAV_VIEWS[Number(e.key) - 1];
        const s = useApp.getState();
        if (v && (v.id === "home" || s.volumeId !== null)) {
          e.preventDefault();
          s.setView(v.id);
        }
        return;
      }
      if (e.key === "F6") {
        e.preventDefault();
        const els = REGIONS.map((sel) => document.querySelector<HTMLElement>(sel)).filter((x): x is HTMLElement => x !== null);
        if (els.length === 0) return;
        const cur = els.findIndex((el) => el.contains(document.activeElement));
        const next = els[(cur + (e.shiftKey ? -1 : 1) + els.length) % els.length];
        const focusable = next?.matches("[tabindex], button, input") ? next : next?.querySelector<HTMLElement>("button, [tabindex='0'], input");
        focusable?.focus();
        return;
      }
      if (isTyping(e.target)) return;
      const s = useApp.getState();
      if (s.volumeId === null || s.selection.length === 0) return;
      let action: EntryAction | null = null;
      if (e.ctrlKey && e.shiftKey && (e.key === "C" || e.key === "c")) action = "copyPath";
      else if (e.ctrlKey && !e.shiftKey && (e.key === "o" || e.key === "O")) action = "open";
      else if (e.ctrlKey && !e.shiftKey && (e.key === "e" || e.key === "E")) action = "reveal";
      else if (e.altKey && e.key === "Enter") action = "properties";
      else if (e.key === "Delete" && !e.ctrlKey) action = "addToCleanup";
      else if ((e.key === "i" || e.key === "I") && !e.ctrlKey && !e.altKey) action = "explain";
      if (!action || !ENTRY_ACTIONS.some((a) => a.type === action)) return;
      e.preventDefault();
      void services.bus.dispatch({ type: action, target: { volumeId: s.volumeId, ids: s.selection } });
    };
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("keydown", onKey);
    };
  }, [services]);

  const visual = isVisualView(view) && volumeId !== null && root !== null;
  // Narrow windows show details as a drawer over the map, so it opens only
  // once something is selected instead of covering the view up front.
  const detailOpen =
    panes.detail && volumeId !== null && (!narrow || primary !== null) && (!isFeatureView(view) || featureInfo(view).showsDetail);
  const maxList = short ? 170 : 520;

  return (
    <div className={`shell${panes.nav ? "" : " shell--nav-collapsed"}${narrow ? " shell--narrow" : ""}`}>
      <a className="skip-link" href="#main">
        Skip to main content
      </a>
      <TopBar
        onOpenPalette={() => {
          setPalette("");
        }}
      />
      <NavRail />
      <div id="main" className="main" tabIndex={-1}>
        {visual ? (
          <div className="workspace">
            <VisualPane volumeId={volumeId} view={view} onContextMenu={openMenu} />
            {panes.list && (
              <>
                <Splitter
                  orientation="horizontal"
                  label="Resize list pane"
                  value={Math.min(listHeight, maxList)}
                  min={100}
                  max={maxList}
                  direction={-1}
                  onChange={setListHeight}
                />
                <div className="workspace__list" style={{ height: Math.min(listHeight, maxList) }}>
                  <ListPane
                    volumeId={volumeId}
                    root={root}
                    onContextMenu={(ids, x, y) => {
                      openMenu({ ids, clientX: x, clientY: y });
                    }}
                  />
                </div>
              </>
            )}
          </div>
        ) : isFeatureView(view) ? (
          <FeatureRouter view={view} />
        ) : (
          <Home />
        )}
      </div>
      {detailOpen && (
        <div className={narrow ? "drawer" : "side"}>
          <DetailPanel
            {...(narrow
              ? {
                  onClose: () => {
                    setPane("detail", false);
                  },
                }
              : {})}
          />
        </div>
      )}
      <StatusBar info={info} />
      {menu && (
        <ContextMenu
          menu={menu}
          bus={services.bus}
          onClose={closeMenu}
        />
      )}
      {palette !== null && (
        <Suspense fallback={null}>
          <CommandPalette
            initial={palette}
            onClose={() => {
              setPalette(null);
            }}
          />
        </Suspense>
      )}
    </div>
  );
}
