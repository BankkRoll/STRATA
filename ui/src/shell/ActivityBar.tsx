/**
 * The activity bar: a narrow icon rail switching between the app's areas,
 * with the cleanup queue count, the shortcut sheet and Settings at the
 * bottom. Arrow keys move between items; clicking the open area shows or
 * hides the sidebar.
 */
import type { KeyboardEvent } from "react";
import { Icon } from "../components/icons";
import { useApp } from "../store/app";
import { useQueue } from "../store/queue";
import { AREAS, areaOf, type AreaInfo } from "./areas";
import { ariaKeys, shortcutOf } from "./keymap";
import { useLayout } from "./layout";
import { goToArea } from "./navigation";

function moveFocus(e: KeyboardEvent<HTMLElement>): void {
  const keys: Record<string, number> = { ArrowDown: 1, ArrowUp: -1, ArrowRight: 1, ArrowLeft: -1 };
  const step = keys[e.key];
  const items = [...e.currentTarget.querySelectorAll<HTMLElement>(".activity__item")];
  const i = items.indexOf(document.activeElement as HTMLElement);
  if (i < 0) return;
  let next: HTMLElement | undefined;
  if (step !== undefined) next = items[(i + step + items.length) % items.length];
  else if (e.key === "Home") next = items[0];
  else if (e.key === "End") next = items[items.length - 1];
  if (!next) return;
  e.preventDefault();
  next.focus();
}

function AreaButton({ area, current, badge }: { area: AreaInfo; current: boolean; badge?: number }) {
  const sidebarOpen = useLayout((s) => s.sidebarOpen);
  return (
    <button
      type="button"
      className="activity__item"
      aria-current={current ? "page" : undefined}
      aria-label={badge ? `${area.label}, ${badge} queued` : area.label}
      aria-keyshortcuts={ariaKeys(area.shortcut)}
      aria-expanded={current && area.id !== "settings" ? sidebarOpen : undefined}
      data-tip={`${area.label} (${area.shortcut})`}
      onClick={() => {
        goToArea(area.id, true);
      }}
    >
      <Icon name={area.icon} size={20} />
      {badge !== undefined && badge > 0 && (
        <span className="activity__badge" aria-hidden="true">
          {badge > 99 ? "99+" : badge}
        </span>
      )}
    </button>
  );
}

/** The left icon rail. */
export function ActivityBar() {
  const area = useApp((s) => areaOf(s.view));
  const queued = useQueue((s) => s.items?.length ?? 0);
  const setCheatSheet = useLayout((s) => s.setCheatSheet);
  const settings = AREAS.find((a) => a.id === "settings") as AreaInfo;
  const help = shortcutOf("cheatSheet");
  return (
    <nav className="activity" aria-label="Areas" data-region="activitybar" onKeyDown={moveFocus}>
      <ul className="activity__list">
        {AREAS.filter((a) => a.id !== "settings").map((a) => (
          <li key={a.id}>
            <AreaButton area={a} current={area === a.id} {...(a.id === "cleanup" ? { badge: queued } : {})} />
          </li>
        ))}
      </ul>
      <ul className="activity__list activity__list--end">
        <li>
          <button
            type="button"
            className="activity__item"
            aria-label="Keyboard shortcuts"
            aria-keyshortcuts={help === "?" ? "Shift+?" : ariaKeys(help)}
            data-tip="Keyboard shortcuts (?)"
            onClick={() => {
              setCheatSheet(true);
            }}
          >
            <Icon name="keyboard" size={20} />
          </button>
        </li>
        <li>
          <AreaButton area={settings} current={area === "settings"} />
        </li>
      </ul>
    </nav>
  );
}
