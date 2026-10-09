/**
 * Left navigation between views. Visual views are disabled (with a reason)
 * until a volume is open; switching keeps root, selection and modes.
 */
import type { ViewId } from "../lib/types";
import { useApp } from "../store/app";
import { Icon, type IconName } from "./icons";

/** Views in nav order with labels and shortcuts. */
export const NAV_VIEWS: readonly { id: ViewId; label: string; icon: IconName; shortcut: string }[] = [
  { id: "home", label: "Volumes", icon: "home", shortcut: "Ctrl+1" },
  { id: "treemap", label: "Treemap", icon: "treemap", shortcut: "Ctrl+2" },
  { id: "sunburst", label: "Sunburst", icon: "sunburst", shortcut: "Ctrl+3" },
  { id: "icicle", label: "Icicle", icon: "icicle", shortcut: "Ctrl+4" },
  { id: "flame", label: "Flame", icon: "flame", shortcut: "Ctrl+5" },
  { id: "bubbles", label: "Bubbles", icon: "bubbles", shortcut: "Ctrl+6" },
  { id: "mindmap", label: "Mind map", icon: "mindmap", shortcut: "Ctrl+7" },
];

/** The left navigation rail. */
export function NavRail() {
  const view = useApp((s) => s.view);
  const setView = useApp((s) => s.setView);
  const hasVolume = useApp((s) => s.volumeId !== null);
  const open = useApp((s) => s.panes.nav);
  return (
    <nav className={open ? "nav" : "nav nav--collapsed"} aria-label="Views">
      <ul>
        {NAV_VIEWS.map((v) => {
          const disabled = v.id !== "home" && !hasVolume;
          return (
            <li key={v.id}>
              <button
                type="button"
                className="nav__item"
                aria-current={view === v.id ? "page" : undefined}
                aria-disabled={disabled}
                aria-keyshortcuts={v.shortcut.replace("Ctrl", "Control")}
                title={disabled ? `${v.label}: open a scanned volume first` : `${v.label} (${v.shortcut})`}
                onClick={() => {
                  if (!disabled) setView(v.id);
                }}
              >
                <Icon name={v.icon} size={18} />
                <span className="nav__label">{v.label}</span>
              </button>
            </li>
          );
        })}
      </ul>
    </nav>
  );
}
