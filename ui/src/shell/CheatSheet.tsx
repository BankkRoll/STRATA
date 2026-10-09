/**
 * Keyboard shortcut sheet (`?`): every binding from the keymap, grouped and
 * filterable. A modal dialog: focus starts in the filter, Tab is trapped,
 * Escape closes and focus returns to where it was.
 */
import { useEffect, useId, useRef, useState } from "react";
import { Icon } from "../components/icons";
import { SHORTCUT_GROUPS, SHORTCUTS, keycaps } from "./keymap";

/** Props for {@link CheatSheet}. */
export interface CheatSheetProps {
  onClose: () => void;
}

/** The shortcut sheet dialog. */
export function CheatSheet({ onClose }: CheatSheetProps) {
  const titleId = useId();
  const ref = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const [query, setQuery] = useState("");

  useEffect(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    inputRef.current?.focus();
    return () => {
      opener?.focus();
    };
  }, []);

  const q = query.trim().toLowerCase();
  const shown = SHORTCUTS.filter((s) => q === "" || s.description.toLowerCase().includes(q) || s.keys.some((k) => k.toLowerCase().includes(q)));

  return (
    <div
      className="overlay-backdrop"
      onPointerDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        ref={ref}
        className="sheet"
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.stopPropagation();
            onClose();
            return;
          }
          if (e.key !== "Tab" || !ref.current) return;
          const items = [...ref.current.querySelectorAll<HTMLElement>("input, button")];
          const first = items[0];
          const last = items[items.length - 1];
          if (e.shiftKey && document.activeElement === first) {
            e.preventDefault();
            last?.focus();
          } else if (!e.shiftKey && document.activeElement === last) {
            e.preventDefault();
            first?.focus();
          }
        }}
      >
        <div className="sheet__head">
          <h2 id={titleId} className="sheet__title">
            Keyboard shortcuts
          </h2>
          <div className="sheet__search">
            <Icon name="search" size={14} />
            <input
              ref={inputRef}
              type="search"
              aria-label="Filter shortcuts"
              placeholder="Filter shortcuts"
              value={query}
              onChange={(e) => {
                setQuery(e.target.value);
              }}
            />
          </div>
          <button type="button" className="icon-btn icon-btn--sm" aria-label="Close" onClick={onClose}>
            <Icon name="close" size={16} />
          </button>
        </div>
        <div className="sheet__body" tabIndex={-1}>
          {shown.length === 0 && <p className="sheet__empty">No shortcuts match “{query}”.</p>}
          {SHORTCUT_GROUPS.map((g) => {
            const rows = shown.filter((s) => s.group === g);
            if (rows.length === 0) return null;
            return (
              <section key={g} className="sheet__group" aria-label={g}>
                <h3 className="sheet__group-title">{g}</h3>
                <dl className="sheet__list">
                  {rows.map((s) => (
                    <div key={`${s.group}:${s.description}`} className="sheet__row">
                      <dt>{s.description}</dt>
                      <dd>
                        {s.keys.map((k, i) => (
                          <span key={k} className="keys">
                            {i > 0 && <span className="keys__or">or</span>}
                            {keycaps(k).map((c, j) => (
                              <kbd key={j}>{c}</kbd>
                            ))}
                          </span>
                        ))}
                      </dd>
                    </div>
                  ))}
                </dl>
              </section>
            );
          })}
        </div>
        <p className="sheet__foot">
          Press <kbd>F6</kbd> to move between the title bar, areas, sidebar, workspace, inspector and status bar.
        </p>
      </div>
    </div>
  );
}
