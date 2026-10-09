/**
 * The title bar: app mark, the current context (volume and folder), the
 * centered command field that opens the palette, and, when Strata draws its
 * own frame, the window controls.
 *
 * The bar is a drag region (`data-tauri-drag-region`; double-click
 * maximizes). Only the bar and its non-interactive children carry the
 * attribute, so buttons keep working.
 */
import { useEffect } from "react";
import { Icon } from "../components/icons";
import { useEntryInfo } from "../lib/hooks";
import { volumeName } from "../lib/volumes";
import { useServices } from "../services";
import { useApp } from "../store/app";
import { useVolumes } from "../store/volumes";
import { areaInfo, areaOf } from "./areas";
import { ariaKeys } from "./keymap";
import { windowControls, type WindowChrome } from "./window";

/** The Strata mark: three stacked bars, as on the website and installer. */
export function AppMark({ size = 16 }: { size?: number }) {
  return (
    <svg className="app-mark" width={size} height={size} viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      <rect x="1" y="2" width="14" height="3" rx="1" className="app-mark__a" />
      <rect x="1" y="6.5" width="9" height="3" rx="1" className="app-mark__b" />
      <rect x="1" y="11" width="5" height="3" rx="1" className="app-mark__c" />
    </svg>
  );
}

/** Context label: the volume and folder in Explore, the area name elsewhere. */
function useContextLabel(): string {
  const services = useServices();
  const view = useApp((s) => s.view);
  const volumeId = useApp((s) => s.volumeId);
  const root = useApp((s) => s.path[s.path.length - 1] ?? null);
  const depth = useApp((s) => s.path.length);
  const volume = useVolumes((s) => s.volumes?.find((v) => v.id === volumeId) ?? null);
  const info = useEntryInfo(volumeId ? services.entryInfo(volumeId) : null, depth > 1 ? root : null);
  const area = areaOf(view);
  if (area !== "explore") return areaInfo(area).label;
  if (view === "home" || volumeId === null) return "Volumes";
  const vol = volume ? volumeName(volume) : "Volume";
  return depth > 1 ? `${info?.name ?? "…"} — ${vol}` : vol;
}

function WindowControls({ chrome }: { chrome: WindowChrome }) {
  return (
    <div className="caption" role="group" aria-label="Window">
      <button type="button" className="caption__btn" aria-label="Minimize" title="Minimize" onClick={() => void windowControls.minimize()}>
        <Icon name="minimize" size={16} />
      </button>
      <button
        type="button"
        className={chrome.maximizeHover ? "caption__btn is-hover" : "caption__btn"}
        data-snap-target=""
        aria-label={chrome.maximized ? "Restore" : "Maximize"}
        title={chrome.maximized ? "Restore Down" : "Maximize"}
        onClick={() => void windowControls.toggleMaximize()}
      >
        <Icon name={chrome.maximized ? "restore" : "maximize"} size={16} />
      </button>
      <button type="button" className="caption__btn caption__btn--close" aria-label="Close" title="Close" onClick={() => void windowControls.close()}>
        <Icon name="close" size={16} />
      </button>
    </div>
  );
}

/** Props for {@link TitleBar}. */
export interface TitleBarProps {
  chrome: WindowChrome;
  onOpenPalette: () => void;
}

/** The app's title bar. */
export function TitleBar({ chrome, onOpenPalette }: TitleBarProps) {
  const context = useContextLabel();
  useEffect(() => {
    document.title = context === "Volumes" ? "Strata" : `${context} — Strata`;
  }, [context]);
  const custom = chrome.mode === "custom";
  return (
    <header className={custom ? "titlebar titlebar--custom" : "titlebar"} data-region="titlebar" data-tauri-drag-region="">
      <div className="titlebar__start" data-tauri-drag-region="">
        <AppMark />
        <span className="titlebar__name" data-tauri-drag-region="">
          Strata
        </span>
        <span className="titlebar__sep" aria-hidden="true" data-tauri-drag-region="" />
        <span className="titlebar__context" data-tauri-drag-region="" title={context}>
          {context}
        </span>
      </div>
      <div className="titlebar__center" data-tauri-drag-region="">
        <button type="button" className="command-field" data-region-focus="" onClick={onOpenPalette} aria-keyshortcuts={ariaKeys("Ctrl+K")}>
          <Icon name="search" size={14} />
          <span className="command-field__text">Search files or run a command</span>
          <kbd>Ctrl K</kbd>
        </button>
      </div>
      <div className="titlebar__end" data-tauri-drag-region="">
        {custom && <WindowControls chrome={chrome} />}
      </div>
    </header>
  );
}
