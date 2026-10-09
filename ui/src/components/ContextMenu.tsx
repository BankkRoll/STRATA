/**
 * Entry context menu. Every action is listed; unavailable ones
 * are disabled with their reason shown, never hidden or faked. Keyboard:
 * Up/Down/Home/End move, Enter/Space run, Escape closes and returns focus.
 */
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { ENTRY_ACTIONS, type CommandBus, type EntryTarget } from "../lib/commands";

/** An open menu. */
export interface ContextMenuState {
  target: EntryTarget;
  x: number;
  y: number;
}

/** Props for {@link ContextMenu}. */
export interface ContextMenuProps {
  menu: ContextMenuState;
  bus: CommandBus;
  onClose: () => void;
}

/** The floating context menu. */
export function ContextMenu({ menu, bus, onClose }: ContextMenuProps) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ left: menu.x, top: menu.y });
  const returnFocus = useRef<Element | null>(null);

  useLayoutEffect(() => {
    returnFocus.current = document.activeElement;
    const el = ref.current;
    if (!el) return;
    const w = el.offsetWidth;
    const h = el.offsetHeight;
    setPos({
      left: Math.max(4, Math.min(menu.x, window.innerWidth - w - 4)),
      top: Math.max(4, Math.min(menu.y, window.innerHeight - h - 4)),
    });
    el.querySelector<HTMLElement>("[role=menuitem]")?.focus();
  }, [menu]);

  useEffect(() => {
    const close = (e: Event) => {
      if (e instanceof MouseEvent && ref.current?.contains(e.target as Node)) return;
      onClose();
    };
    window.addEventListener("pointerdown", close, true);
    window.addEventListener("blur", close);
    window.addEventListener("resize", close);
    return () => {
      window.removeEventListener("pointerdown", close, true);
      window.removeEventListener("blur", close);
      window.removeEventListener("resize", close);
      if (returnFocus.current instanceof HTMLElement) returnFocus.current.focus();
    };
  }, [onClose]);

  const items = ENTRY_ACTIONS.map((a) => ({ ...a, availability: bus.availability({ type: a.type, target: menu.target }) }));

  const move = (from: HTMLElement, dir: 1 | -1 | "first" | "last") => {
    const all = Array.from(ref.current?.querySelectorAll<HTMLElement>("[role=menuitem]") ?? []);
    const i = all.indexOf(from);
    const next = dir === "first" ? all[0] : dir === "last" ? all[all.length - 1] : all[(i + dir + all.length) % all.length];
    next?.focus();
  };

  return (
    <div
      ref={ref}
      className="menu"
      role="menu"
      aria-label="Actions"
      style={pos}
      onContextMenu={(e) => {
        e.preventDefault();
      }}
    >
      {items.map((a, i) => (
        <div key={a.type} role="none">
          {i > 0 && items[i - 1]?.group !== a.group && <div role="separator" className="menu__sep" />}
          <button
            type="button"
            role="menuitem"
            className="menu__item"
            aria-disabled={!a.availability.enabled}
            aria-describedby={a.availability.enabled ? undefined : `menu-reason-${a.type}`}
            tabIndex={-1}
            onKeyDown={(e) => {
              const el = e.currentTarget;
              if (e.key === "ArrowDown") move(el, 1);
              else if (e.key === "ArrowUp") move(el, -1);
              else if (e.key === "Home") move(el, "first");
              else if (e.key === "End") move(el, "last");
              else if (e.key === "Escape" || e.key === "Tab") onClose();
              else return;
              e.preventDefault();
            }}
            onClick={() => {
              if (!a.availability.enabled) return;
              onClose();
              void bus.dispatch({ type: a.type, target: menu.target });
            }}
          >
            <span className="menu__label">{a.label}</span>
            {a.shortcut && <kbd className="menu__key">{a.shortcut}</kbd>}
            {!a.availability.enabled && (
              <span id={`menu-reason-${a.type}`} className="menu__reason">
                {a.availability.reason}
              </span>
            )}
          </button>
        </div>
      ))}
    </div>
  );
}
