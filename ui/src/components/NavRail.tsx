/**
 * Left navigation between views. Visual views are disabled (with a reason)
 * until a volume is open; switching keeps root, selection and modes.
 */
import type { ViewId } from "../lib/types";
import { useApp } from "../store/app";
import { useQueue } from "../store/queue";
import { FEATURE_VIEWS } from "../views/featureViews";
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
      <FeatureNav />
    </nav>
  );
}

/** Insights and Manage groups (wave-2 views), with the queue count badge. */
function FeatureNav() {
  const view = useApp((s) => s.view);
  const setView = useApp((s) => s.setView);
  const hasVolume = useApp((s) => s.volumeId !== null);
  const queued = useQueue((s) => s.items?.length ?? 0);
  return (
    <>
      {(["Insights", "Manage"] as const).map((group) => (
        <div key={group} className="nav__group" role="group" aria-labelledby={`nav-${group}`}>
          <h2 id={`nav-${group}`} className="nav__heading">
            {group}
          </h2>
          <ul>
            {FEATURE_VIEWS.filter((v) => v.group === group).map((v) => {
              const disabled = v.needsVolume && !hasVolume;
              const badge = v.id === "cleanup" && queued > 0 ? queued : null;
              return (
                <li key={v.id}>
                  <button
                    type="button"
                    className="nav__item"
                    aria-current={view === v.id ? "page" : undefined}
                    aria-disabled={disabled}
                    title={disabled ? `${v.label}: open a scanned volume first` : v.label}
                    onClick={() => {
                      if (!disabled) setView(v.id);
                    }}
                  >
                    <Icon name={v.icon} size={18} />
                    <span className="nav__label">
                      {v.label}
                      {badge !== null && <span className="visually-hidden">, {badge} queued</span>}
                    </span>
                    {badge !== null && (
                      <span className="nav__badge" aria-hidden="true">
                        {badge > 99 ? "99+" : badge}
                      </span>
                    )}
                  </button>
                </li>
              );
            })}
          </ul>
        </div>
      ))}
    </>
  );
}
