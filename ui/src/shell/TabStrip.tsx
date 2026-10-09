/**
 * The Explore tab strip: a fixed "Volumes" tab, then one tab per opened
 * volume or folder root.
 *
 * Keyboard (ARIA tabs pattern, automatic activation): Left/Right/Home/End
 * move between tabs, Delete closes the focused tab, Ctrl+Shift+Left/Right
 * reorders it. Middle-click closes. Global shortcuts (Ctrl+T, Ctrl+W,
 * Ctrl+Tab, Ctrl+Shift+PageUp/PageDown) live in the keymap.
 */
import type { KeyboardEvent, ReactNode } from "react";
import { Icon } from "../components/icons";
import { useEntryInfo } from "../lib/hooks";
import { volumeName } from "../lib/volumes";
import { useServices } from "../services";
import { useApp } from "../store/app";
import { useVolumes } from "../store/volumes";
import { activateTab, closeTab, moveTab, useTabs, type WorkspaceTab } from "./tabs";

/** Id of the workspace panel the tabs control. */
export const WORKSPACE_PANEL_ID = "workspace-panel";

function useTabLabel(tab: WorkspaceTab): { label: string; volume: string } {
  const services = useServices();
  const volume = useVolumes((s) => s.volumes?.find((v) => v.id === tab.volumeId) ?? null);
  const last = tab.path[tab.path.length - 1] ?? null;
  const info = useEntryInfo(services.entryInfo(tab.volumeId), tab.path.length > 1 ? last : null);
  const vol = volume ? volumeName(volume) : "Volume";
  return { label: tab.path.length > 1 ? (info?.name ?? "…") : vol, volume: vol };
}

function Tab({ selected, icon, label, title, onSelect, onClose, onMove, id }: { id: string; selected: boolean; icon: ReactNode; label: string; title: string; onSelect: () => void; onClose?: () => void; onMove?: (d: -1 | 1) => void }) {
  return (
    <div
      role="tab"
      id={`tab-${id}`}
      data-tab={id}
      aria-selected={selected}
      aria-controls={WORKSPACE_PANEL_ID}
      tabIndex={selected ? 0 : -1}
      aria-label={label}
      className={selected ? "tab is-active" : "tab"}
      title={title}
      onClick={onSelect}
      onAuxClick={(e) => {
        if (e.button === 1 && onClose) {
          e.preventDefault();
          onClose();
        }
      }}
      onKeyDown={(e) => {
        if ((e.key === "Delete" || (e.key === "w" && e.ctrlKey)) && onClose) {
          e.preventDefault();
          onClose();
        } else if (e.ctrlKey && e.shiftKey && (e.key === "ArrowLeft" || e.key === "ArrowRight") && onMove) {
          e.preventDefault();
          onMove(e.key === "ArrowLeft" ? -1 : 1);
        } else if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onSelect();
        }
      }}
    >
      {icon}
      <span className="tab__label">{label}</span>
      {onClose && (
        <button
          type="button"
          className="tab__close"
          tabIndex={-1}
          aria-label={`Close ${label}`}
          onClick={(e) => {
            e.stopPropagation();
            onClose();
          }}
        >
          <Icon name="close" size={12} />
        </button>
      )}
    </div>
  );
}

function VolumeTab({ tab, selected }: { tab: WorkspaceTab; selected: boolean }) {
  const { label, volume } = useTabLabel(tab);
  return (
    <Tab
      id={tab.id}
      selected={selected}
      icon={<Icon name={tab.path.length > 1 ? "folder" : "drive"} size={14} />}
      label={label}
      title={tab.path.length > 1 ? `${label} — ${volume}` : volume}
      onSelect={() => {
        activateTab(tab.id);
      }}
      onClose={() => {
        closeTab(tab.id);
        requestAnimationFrame(() => document.querySelector<HTMLElement>(".tabs [role=tab][aria-selected=true]")?.focus());
      }}
      onMove={(d) => {
        moveTab(tab.id, d);
        requestAnimationFrame(() => document.querySelector<HTMLElement>(`[data-tab="${tab.id}"]`)?.focus());
      }}
    />
  );
}

function onListKeys(e: KeyboardEvent<HTMLElement>): void {
  if (e.ctrlKey || e.altKey) return;
  const tabs = [...e.currentTarget.querySelectorAll<HTMLElement>("[role=tab]")];
  const i = tabs.indexOf((e.target as HTMLElement).closest<HTMLElement>("[role=tab]") as HTMLElement);
  if (i < 0) return;
  let next: HTMLElement | undefined;
  if (e.key === "ArrowRight") next = tabs[(i + 1) % tabs.length];
  else if (e.key === "ArrowLeft") next = tabs[(i - 1 + tabs.length) % tabs.length];
  else if (e.key === "Home") next = tabs[0];
  else if (e.key === "End") next = tabs[tabs.length - 1];
  if (!next) return;
  e.preventDefault();
  next.focus();
  next.click();
}

/** The tab strip with trailing actions (view switcher, pane toggles). */
export function TabStrip({ actions }: { actions?: ReactNode }) {
  const tabs = useTabs((s) => s.tabs);
  const activeId = useTabs((s) => s.activeId);
  const home = useApp((s) => s.view === "home");
  return (
    <div className="tabbar">
      <div className="tabs" role="tablist" aria-label="Open locations" onKeyDown={onListKeys}>
        <Tab
          id="home"
          selected={home}
          icon={<Icon name="home" size={14} />}
          label="Volumes"
          title="All volumes"
          onSelect={() => {
            useApp.getState().setView("home");
          }}
        />
        {tabs.map((t) => (
          <VolumeTab key={t.id} tab={t} selected={!home && t.id === activeId} />
        ))}
      </div>
      {actions && <div className="tabbar__actions">{actions}</div>}
    </div>
  );
}
