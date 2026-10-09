/**
 * App shell: title bar, activity bar, the active area's sidebar, the
 * workspace (tabs, path bar and toolbar, visual view over the list, and the
 * inspector) and the status bar.
 *
 * Responsibilities:
 * - Mounts the stores' backend syncs (volumes, queue, appearance) and the
 *   shell's own (layout persistence, tabs, last view per area).
 * - Dispatches the global keyboard map (`shell/keymap.ts`) and the entry
 *   action shortcuts.
 * - Adapts down to 900×600: below 1100 px the sidebar collapses to the
 *   activity bar (opening as an overlay) and the inspector becomes an
 *   overlay that appears once something is selected.
 */
import { Suspense, lazy, useCallback, useEffect, useState, type CSSProperties } from "react";
import { ENTRY_ACTIONS, type EntryAction } from "../lib/commands";
import { useMediaQuery } from "../lib/hooks";
import type { ContextMenuRequest } from "../render/ViewController";
import { useServices } from "../services";
import { ActivityBar } from "../shell/ActivityBar";
import { areaOf, type AreaId } from "../shell/areas";
import { commandFor, type ShellCommand } from "../shell/keymap";
import { LIMITS, useLayout, useLayoutPersistence } from "../shell/layout";
import { goToArea, openSettings, useLastViewTracking } from "../shell/navigation";
import { cycleRegion } from "../shell/regions";
import { Sidebar } from "../shell/Sidebar";
import { StatusBar } from "../shell/StatusBar";
import { TabStrip, WORKSPACE_PANEL_ID } from "../shell/TabStrip";
import { activeTab, closeTab, cycleTab, moveTab, openTab, useTabSync, useTabs } from "../shell/tabs";
import { TipLayer } from "../shell/TipLayer";
import { TitleBar } from "../shell/TitleBar";
import "../shell/shell.css";
import { ExploreToolbar, PaneToggles, ViewSwitcher } from "../shell/Toolbar";
import { VolumeBanner } from "../shell/VolumeBanner";
import { useWindowChrome } from "../shell/window";
import { isVisualView, useApp } from "../store/app";
import { useAppearanceSync } from "../store/prefs";
import { useQueueSync } from "../store/queue";
import { DetailPanel } from "../views/DetailPanel";
import { Home, useVolumeSync } from "../views/Home";
import { ListPane } from "../views/ListPane";
import { VisualPane } from "../views/VisualPane";
import { FeatureRouter, featureInfo, isFeatureView } from "../views/featureViews";
import { ContextMenu, type ContextMenuState } from "./ContextMenu";
import { Splitter } from "./Splitter";

const CommandPalette = lazy(() => import("./CommandPalette").then((m) => ({ default: m.CommandPalette })));
const CheatSheet = lazy(() => import("../shell/CheatSheet").then((m) => ({ default: m.CheatSheet })));

function isTyping(el: EventTarget | null): boolean {
  return el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement || el instanceof HTMLSelectElement || (el instanceof HTMLElement && el.isContentEditable);
}

/**
 * Runs a keyboard command.
 *
 * @param cmd - Command from the keymap.
 * @param narrow - The window is in its narrow layout.
 * @param openPalette - Opens the command palette.
 */
function runCommand(cmd: ShellCommand, narrow: boolean, openPalette: () => void): void {
  const app = useApp.getState();
  const layout = useLayout.getState();
  const tabs = useTabs.getState();
  const needVolume = () => {
    if (app.volumeId !== null) return true;
    app.notify("Open a scanned volume first.");
    return false;
  };
  if (cmd.startsWith("area:")) {
    goToArea(cmd.slice(5) as AreaId);
    return;
  }
  if (cmd.startsWith("view:")) {
    const view = cmd.slice(5);
    if (!needVolume() || !isVisualView(view as never)) return;
    app.setView(view as never);
    if (layout.split === "list") layout.set({ split: "split" });
    return;
  }
  switch (cmd) {
    case "palette":
    case "fileSearch":
      openPalette();
      return;
    case "settings":
      openSettings();
      return;
    case "cheatSheet":
      layout.setCheatSheet(true);
      return;
    case "nextRegion":
    case "prevRegion":
      cycleRegion(cmd === "nextRegion" ? 1 : -1);
      return;
    case "toggleSidebar":
      if (areaOf(app.view) === "settings") return;
      if (narrow) layout.setSidebarOverlay(!layout.sidebarOverlay);
      else layout.set({ sidebarOpen: !layout.sidebarOpen });
      return;
    case "toggleList":
      if (layout.split === "list") layout.set({ split: "split" });
      app.togglePane("list");
      return;
    case "toggleInspector":
      app.togglePane("detail");
      return;
    case "editPath":
      if (!needVolume()) return;
      if (!isVisualView(app.view)) app.setView(app.lastVisual);
      layout.setEditingPath(true);
      return;
    case "goUp":
      if (isVisualView(app.view)) app.goUp();
      return;
    case "listMode":
      if (!needVolume()) return;
      if (!isVisualView(app.view)) app.setView(app.lastVisual);
      layout.set({ split: "list" });
      app.setPane("list", true);
      return;
    case "newTab":
      if (needVolume() && app.volumeId !== null) openTab(app.volumeId, app.path);
      return;
    case "closeTab": {
      const t = activeTab(tabs);
      if (t && app.view !== "home" && areaOf(app.view) === "explore") closeTab(t.id);
      return;
    }
    case "nextTab":
    case "prevTab":
      cycleTab(cmd === "nextTab" ? 1 : -1);
      return;
    case "moveTabLeft":
    case "moveTabRight":
      if (tabs.activeId) moveTab(tabs.activeId, cmd === "moveTabLeft" ? -1 : 1);
      return;
  }
}

/** Visual view over the list, per the split mode. */
function ExploreSplit({ volumeId, root, onContextMenu }: { volumeId: string; root: number; onContextMenu: (req: ContextMenuRequest) => void }) {
  const view = useApp((s) => s.view);
  const listOpen = useApp((s) => s.panes.list);
  const split = useLayout((s) => s.split);
  const listHeight = useLayout((s) => s.listHeight);
  const setLayout = useLayout((s) => s.set);
  const short = useMediaQuery("(max-height: 699px)");
  const maxList = short ? 220 : LIMITS.list.max;
  const height = Math.min(listHeight, maxList);
  const list = (
    <ListPane
      volumeId={volumeId}
      root={root}
      onContextMenu={(ids, x, y) => {
        onContextMenu({ ids, clientX: x, clientY: y });
      }}
    />
  );
  if (split === "list") return <div className="ws-list ws-list--full">{list}</div>;
  if (!isVisualView(view)) return null;
  return (
    <div className="ws-split">
      <VisualPane volumeId={volumeId} view={view} onContextMenu={onContextMenu} />
      {listOpen && (
        <>
          <Splitter
            orientation="horizontal"
            label="Resize list pane"
            value={height}
            min={LIMITS.list.min}
            max={maxList}
            direction={-1}
            onChange={(listHeight) => {
              setLayout({ listHeight });
            }}
          />
          <div className="ws-list" style={{ height }}>
            {list}
          </div>
        </>
      )}
    </div>
  );
}

/** The right-hand inspector (detail panel), docked or as an overlay. */
function Inspector({ overlay }: { overlay: boolean }) {
  const width = useLayout((s) => s.inspectorWidth);
  const setLayout = useLayout((s) => s.set);
  const setPane = useApp((s) => s.setPane);
  if (overlay) {
    return (
      <div
        className="inspector inspector--overlay"
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.stopPropagation();
            setPane("detail", false);
          }
        }}
      >
        <DetailPanel
          onClose={() => {
            setPane("detail", false);
          }}
        />
      </div>
    );
  }
  return (
    <>
      <Splitter
        orientation="vertical"
        label="Resize inspector"
        value={width}
        min={LIMITS.inspector.min}
        max={LIMITS.inspector.max}
        direction={-1}
        onChange={(inspectorWidth) => {
          setLayout({ inspectorWidth });
        }}
      />
      <div className="inspector" style={{ width }} data-region="inspector">
        <DetailPanel />
      </div>
    </>
  );
}

/** The whole window's layout. */
export function AppShell() {
  const services = useServices();
  useVolumeSync();
  useQueueSync();
  useAppearanceSync();
  useLayoutPersistence();
  useTabSync();
  useLastViewTracking();
  const chrome = useWindowChrome();
  const view = useApp((s) => s.view);
  const volumeId = useApp((s) => s.volumeId);
  const root = useApp((s) => s.path[s.path.length - 1] ?? null);
  const detailOpen = useApp((s) => s.panes.detail);
  const primary = useApp((s) => s.primary);
  const sidebarOpen = useLayout((s) => s.sidebarOpen);
  const sidebarOverlay = useLayout((s) => s.sidebarOverlay);
  const sidebarWidth = useLayout((s) => s.sidebarWidth);
  const cheatSheet = useLayout((s) => s.cheatSheet);
  const setLayout = useLayout((s) => s.set);
  const activeTabId = useTabs((s) => s.activeId);
  const [palette, setPalette] = useState<string | null>(null);
  const [menu, setMenu] = useState<ContextMenuState | null>(null);
  const narrow = useMediaQuery("(max-width: 1099px)");
  const area = areaOf(view);

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

  // The overlay sidebar closes whenever the user navigates.
  useEffect(() => {
    useLayout.getState().setSidebarOverlay(false);
  }, [view, volumeId, narrow]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const typing = isTyping(e.target);
      const cmd = commandFor(e, typing);
      if (cmd) {
        e.preventDefault();
        runCommand(cmd, narrow, () => {
          setPalette("");
        });
        return;
      }
      if (typing || e.defaultPrevented) return;
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
  }, [services, narrow]);

  const hasSidebar = area !== "settings";
  const showSidebar = hasSidebar && (narrow ? sidebarOverlay : sidebarOpen);
  const exploring = area === "explore" && view !== "home" && volumeId !== null && root !== null;
  const inspectorWanted = detailOpen && volumeId !== null && (exploring || (isFeatureView(view) && featureInfo(view).showsDetail));
  // In narrow windows the inspector covers the map, so it appears only once something is selected.
  const showInspector = inspectorWanted && (!narrow || primary !== null);

  let content;
  if (area === "explore") {
    content = (
      <>
        <TabStrip
          actions={
            exploring ? (
              <>
                <ViewSwitcher />
                <PaneToggles />
              </>
            ) : undefined
          }
        />
        <div id={WORKSPACE_PANEL_ID} className="ws-panel" role="tabpanel" aria-labelledby={view === "home" || !activeTabId ? "tab-home" : `tab-${activeTabId}`}>
          {exploring ? (
            <>
              <ExploreToolbar />
              <VolumeBanner />
              <div className="ws-body">
                <div className="ws-main">
                  <ExploreSplit volumeId={volumeId} root={root} onContextMenu={openMenu} />
                </div>
                {showInspector && <Inspector overlay={narrow} />}
              </div>
            </>
          ) : (
            <Home />
          )}
        </div>
      </>
    );
  } else {
    content = (
      <div className="ws-body">
        <div className="ws-main">{isFeatureView(view) && <FeatureRouter view={view} />}</div>
        {showInspector && <Inspector overlay={narrow} />}
      </div>
    );
  }

  const classes = ["shell", narrow ? "shell--narrow" : "", chrome.mode === "custom" ? "shell--custom-frame" : ""].filter(Boolean).join(" ");
  return (
    <div className={classes} style={{ "--sidebar-w": `${sidebarWidth}px` } as CSSProperties}>
      <a className="skip-link" href="#main">
        Skip to main content
      </a>
      <TitleBar
        chrome={chrome}
        onOpenPalette={() => {
          setPalette("");
        }}
      />
      <div className="shell__body">
        <ActivityBar />
        {showSidebar && <Sidebar area={area} overlay={narrow} />}
        {showSidebar && !narrow && (
          <Splitter
            orientation="vertical"
            label="Resize sidebar"
            value={sidebarWidth}
            min={LIMITS.sidebar.min}
            max={LIMITS.sidebar.max}
            direction={1}
            onChange={(w) => {
              setLayout({ sidebarWidth: w });
            }}
          />
        )}
        {showSidebar && narrow && (
          <div
            className="scrim"
            aria-hidden="true"
            onPointerDown={() => {
              useLayout.getState().setSidebarOverlay(false);
            }}
          />
        )}
        <main id="main" className={`workspace workspace--${area}`} data-region="workspace" tabIndex={-1} aria-label="Workspace">
          {content}
        </main>
      </div>
      <StatusBar />
      <TipLayer />
      {menu && <ContextMenu menu={menu} bus={services.bus} onClose={closeMenu} />}
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
      {cheatSheet && (
        <Suspense fallback={null}>
          <CheatSheet
            onClose={() => {
              useLayout.getState().setCheatSheet(false);
            }}
          />
        </Suspense>
      )}
    </div>
  );
}
