/**
 * Explore toolbar and view switcher.
 *
 * - {@link ViewSwitcher}: treemap / sunburst / icicle / flame / bubbles /
 *   mind map / list, as an icon radio group (Ctrl+1–7).
 * - {@link PaneToggles}: list pane and inspector.
 * - {@link ExploreToolbar}: up, the path bar, filter chips and the filter
 *   menu, size mode, color mode, treemap style, patterns and the cleanup
 *   queue count.
 */
import { useEffect, useId, useRef, useState, type KeyboardEvent } from "react";
import { Icon } from "../components/icons";
import { formatBytes } from "../lib/format";
import { CATEGORIES, COLOR_MODES, type ColorMode } from "../lib/palette";
import { NO_FILTERS, hasFilters, type SizeMode, type TreemapStyle, type ViewFilters } from "../lib/types";
import { isVisualView, useApp, type Pane } from "../store/app";
import { useQueue } from "../store/queue";
import { useSettings } from "../store/settings";
import { LIST_VIEW_SHORTCUT, VISUAL_VIEWS } from "./areas";
import { MIN_SIZE_PRESETS, MODIFIED_PRESETS, filterChips, modifiedLabel } from "./filters";
import { ariaKeys, shortcutOf } from "./keymap";
import { useLayout } from "./layout";
import { PathBar } from "./PathBar";

// -----------------------------------------------------------------------------
// Shared
// -----------------------------------------------------------------------------

/** Arrow-key movement inside a radio group (roving tabindex). */
function radioKeys(e: KeyboardEvent<HTMLElement>): void {
  const step = e.key === "ArrowRight" || e.key === "ArrowDown" ? 1 : e.key === "ArrowLeft" || e.key === "ArrowUp" ? -1 : 0;
  if (step === 0) return;
  const radios = [...e.currentTarget.querySelectorAll<HTMLElement>("[role=radio]")];
  const i = radios.indexOf((e.target as HTMLElement).closest<HTMLElement>("[role=radio]") as HTMLElement);
  if (i < 0) return;
  e.preventDefault();
  const next = radios[(i + step + radios.length) % radios.length];
  next?.focus();
  next?.click();
}

/**
 * Closes a popover on Escape or a pointer press outside `ref`.
 *
 * @param open - Whether it is open.
 * @param close - Close handler.
 */
function useDismiss(ref: React.RefObject<HTMLElement | null>, open: boolean, close: () => void): void {
  useEffect(() => {
    if (!open) return;
    const down = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) close();
    };
    const key = (e: globalThis.KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        close();
      }
    };
    window.addEventListener("pointerdown", down, true);
    window.addEventListener("keydown", key, true);
    return () => {
      window.removeEventListener("pointerdown", down, true);
      window.removeEventListener("keydown", key, true);
    };
  }, [ref, open, close]);
}

// -----------------------------------------------------------------------------
// View switcher and pane toggles
// -----------------------------------------------------------------------------

/** Visual view radio group, plus the list-only mode. */
export function ViewSwitcher() {
  const view = useApp((s) => s.view);
  const setView = useApp((s) => s.setView);
  const split = useLayout((s) => s.split);
  const setLayout = useLayout((s) => s.set);
  const listOnly = split === "list";
  return (
    <div className="seg seg--icons" role="radiogroup" aria-label="View" onKeyDown={radioKeys}>
      {VISUAL_VIEWS.map((v) => {
        const on = !listOnly && view === v.id;
        return (
          <button
            key={v.id}
            type="button"
            role="radio"
            aria-checked={on}
            aria-label={v.label}
            aria-keyshortcuts={ariaKeys(v.shortcut)}
            tabIndex={on ? 0 : -1}
            data-tip={`${v.label} (${v.shortcut})`}
            onClick={() => {
              setView(v.id);
              if (listOnly) setLayout({ split: "split" });
            }}
          >
            <Icon name={v.icon} size={16} />
          </button>
        );
      })}
      <button
        type="button"
        role="radio"
        aria-checked={listOnly}
        aria-label="List"
        aria-keyshortcuts={ariaKeys(LIST_VIEW_SHORTCUT)}
        tabIndex={listOnly ? 0 : -1}
        data-tip={`List (${LIST_VIEW_SHORTCUT})`}
        onClick={() => {
          setLayout({ split: "list" });
          useApp.getState().setPane("list", true);
        }}
      >
        <Icon name="list" size={16} />
      </button>
    </div>
  );
}

function PaneToggle({ pane, icon, label, command }: { pane: Pane; icon: "panelBottom" | "detail"; label: string; command: "toggleList" | "toggleInspector" }) {
  const open = useApp((s) => s.panes[pane]);
  const toggle = useApp((s) => s.togglePane);
  const keys = shortcutOf(command);
  return (
    <button
      type="button"
      className="icon-btn icon-btn--sm"
      aria-pressed={open}
      aria-label={label}
      aria-keyshortcuts={ariaKeys(keys)}
      data-tip={`${label} (${keys ?? ""})`}
      onClick={() => {
        toggle(pane);
      }}
    >
      <Icon name={icon} size={16} />
    </button>
  );
}

/** List pane and inspector toggles. */
export function PaneToggles() {
  return (
    <div className="tool-group">
      <PaneToggle pane="list" icon="panelBottom" label="List pane" command="toggleList" />
      <PaneToggle pane="detail" icon="detail" label="Inspector" command="toggleInspector" />
    </div>
  );
}

// -----------------------------------------------------------------------------
// Filters
// -----------------------------------------------------------------------------

function FilterMenu({ onClose }: { onClose: () => void }) {
  const filters = useApp((s) => s.filters);
  const setFilters = useApp((s) => s.setFilters);
  const units = useSettings((s) => s.units);
  const saveFilter = useLayout((s) => s.saveFilter);
  const [name, setName] = useState("");
  const [saved, setSaved] = useState<string | null>(null);
  const id = useId();
  const patch = (p: Partial<ViewFilters>) => {
    setFilters({ ...filters, ...p });
    setSaved(null);
  };
  return (
    <div className="popover filter-menu" role="dialog" aria-label="Filters">
      <div className="filter-menu__row">
        <label htmlFor={`${id}-min`}>Minimum size</label>
        <select
          id={`${id}-min`}
          className="select-input"
          value={filters.minBytes}
          onChange={(e) => {
            patch({ minBytes: Number(e.target.value) });
          }}
        >
          {MIN_SIZE_PRESETS.map((b) => (
            <option key={b} value={b}>
              {b === 0 ? "Any size" : `${formatBytes(b, { units })} or more`}
            </option>
          ))}
        </select>
      </div>
      <div className="filter-menu__row">
        <label htmlFor={`${id}-mod`}>Modified</label>
        <select
          id={`${id}-mod`}
          className="select-input"
          value={filters.modifiedWithinDays ?? ""}
          onChange={(e) => {
            patch({ modifiedWithinDays: e.target.value === "" ? null : Number(e.target.value) });
          }}
        >
          {MODIFIED_PRESETS.map((d) => (
            <option key={d ?? "any"} value={d ?? ""}>
              {modifiedLabel(d)}
            </option>
          ))}
        </select>
      </div>
      <fieldset className="filter-menu__cats">
        <legend>Categories</legend>
        <div className="filter-menu__grid">
          {CATEGORIES.map((c) => (
            <label key={c.id} className="check">
              <input
                type="checkbox"
                checked={filters.categories.includes(c.id)}
                onChange={(e) => {
                  patch({ categories: e.target.checked ? [...filters.categories, c.id] : filters.categories.filter((x) => x !== c.id) });
                }}
              />
              <span className="cat-dot" style={{ background: `light-dark(${c.light}, ${c.dark})` }} aria-hidden="true" />
              {c.label}
            </label>
          ))}
        </div>
      </fieldset>
      <form
        className="filter-menu__save"
        onSubmit={(e) => {
          e.preventDefault();
          const s = saveFilter(name, filters);
          setSaved(s.name);
          setName("");
        }}
      >
        <label htmlFor={`${id}-name`} className="visually-hidden">
          Name for these filters
        </label>
        <input
          id={`${id}-name`}
          className="text-input"
          placeholder="Name these filters"
          value={name}
          onChange={(e) => {
            setName(e.target.value);
          }}
        />
        <button type="submit" className="btn btn--sm" disabled={!hasFilters(filters) || name.trim() === ""}>
          Save
        </button>
      </form>
      <div className="filter-menu__foot">
        <span role="status" className="filter-menu__status">
          {saved ? `Saved “${saved}” to the sidebar.` : ""}
        </span>
        <button
          type="button"
          className="btn btn--sm btn--ghost"
          disabled={!hasFilters(filters)}
          onClick={() => {
            setFilters(NO_FILTERS);
          }}
        >
          Clear all
        </button>
        <button type="button" className="btn btn--sm" onClick={onClose}>
          Done
        </button>
      </div>
    </div>
  );
}

function Filters() {
  const filters = useApp((s) => s.filters);
  const setFilters = useApp((s) => s.setFilters);
  const units = useSettings((s) => s.units);
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const close = () => {
    setOpen(false);
  };
  useDismiss(ref, open, close);
  const chips = filterChips(filters, units);
  return (
    <div className="filters-bar" ref={ref}>
      {chips.length > 0 && (
        <ul className="fchips" aria-label="Active filters">
          {chips.map((c) => (
            <li key={c.id} className="fchip">
              <span>{c.label}</span>
              <button
                type="button"
                className="fchip__x"
                aria-label={`Remove filter: ${c.label}`}
                onClick={() => {
                  setFilters(c.without);
                }}
              >
                <Icon name="close" size={10} />
              </button>
            </li>
          ))}
        </ul>
      )}
      <button
        type="button"
        className={chips.length > 0 ? "tool-btn is-on" : "tool-btn"}
        aria-expanded={open}
        aria-haspopup="dialog"
        onClick={() => {
          setOpen(!open);
        }}
      >
        <Icon name="filter" size={14} />
        <span className="tool-btn__label">Filter</span>
      </button>
      {open && <FilterMenu onClose={close} />}
    </div>
  );
}

// -----------------------------------------------------------------------------
// Toolbar
// -----------------------------------------------------------------------------

const SIZE_MODES: readonly { id: SizeMode; label: string; hint: string }[] = [
  { id: "allocated", label: "On disk", hint: "Allocated size: what it actually costs" },
  { id: "logical", label: "Logical", hint: "Logical size: what files claim to be" },
];

function SizeModeSwitch() {
  const sizeMode = useApp((s) => s.sizeMode);
  const setSizeMode = useApp((s) => s.setSizeMode);
  return (
    <div className="seg" role="radiogroup" aria-label="Size mode" onKeyDown={radioKeys}>
      {SIZE_MODES.map((m) => (
        <button
          key={m.id}
          type="button"
          role="radio"
          aria-checked={sizeMode === m.id}
          tabIndex={sizeMode === m.id ? 0 : -1}
          data-tip={m.hint}
          onClick={() => {
            setSizeMode(m.id);
          }}
        >
          {m.label}
        </button>
      ))}
    </div>
  );
}

function QueueButton() {
  const queue = useQueue((s) => s.items);
  const units = useSettings((s) => s.units);
  const setView = useApp((s) => s.setView);
  if (queue === null) return null;
  const bytes = queue.reduce((a, i) => a + i.bytes, 0);
  return (
    <button
      type="button"
      className={queue.length > 0 ? "tool-btn is-on" : "tool-btn"}
      aria-label={`Cleanup queue: ${queue.length} ${queue.length === 1 ? "item" : "items"}${queue.length > 0 ? `, ${formatBytes(bytes, { units })}` : ""}`}
      data-tip="Review the cleanup queue"
      onClick={() => {
        setView("cleanup");
      }}
    >
      <Icon name="broom" size={14} />
      <span className="tool-btn__count">{queue.length}</span>
      {queue.length > 0 && <span className="tool-btn__label tool-btn__meta">{formatBytes(bytes, { units })}</span>}
    </button>
  );
}

/** The Explore toolbar under the tabs. */
export function ExploreToolbar() {
  const colorMode = useApp((s) => s.colorMode);
  const setColorMode = useApp((s) => s.setColorMode);
  const style = useApp((s) => s.treemapStyle);
  const setStyle = useApp((s) => s.setTreemapStyle);
  const view = useApp((s) => s.view);
  const canGoUp = useApp((s) => s.path.length > 1);
  const patterns = useSettings((s) => s.patterns);
  const setPatterns = useSettings((s) => s.setPatterns);
  const listOnly = useLayout((s) => s.split === "list");
  const visual = isVisualView(view) && !listOnly;
  const up = shortcutOf("goUp");
  return (
    <div className="toolbar-row" role="toolbar" aria-label="Explore">
      <button
        type="button"
        className="icon-btn icon-btn--sm"
        aria-label="Up one folder"
        aria-keyshortcuts={ariaKeys(up)}
        data-tip={`Up one folder (${up ?? ""})`}
        disabled={!canGoUp}
        onClick={() => {
          useApp.getState().goUp();
        }}
      >
        <Icon name="up" size={16} />
      </button>
      <PathBar />
      <Filters />
      <span className="toolbar-row__sep" aria-hidden="true" />
      <SizeModeSwitch />
      {visual && (
        <>
          <label className="select-wrap" data-tip="What block colors mean">
            <span className="select-wrap__label">Color</span>
            <select
              className="select-input select-input--bare"
              value={colorMode}
              aria-label="Color by"
              onChange={(e) => {
                setColorMode(e.target.value as ColorMode);
              }}
            >
              {COLOR_MODES.map((m) => (
                <option key={m.id} value={m.id}>
                  {m.label}
                </option>
              ))}
            </select>
          </label>
          {view === "treemap" && (
            <label className="select-wrap">
              <span className="select-wrap__label">Style</span>
              <select
                className="select-input select-input--bare"
                value={style}
                aria-label="Treemap style"
                onChange={(e) => {
                  setStyle(e.target.value as TreemapStyle);
                }}
              >
                <option value="flat">Flat</option>
                <option value="cushion">Cushion</option>
              </select>
            </label>
          )}
          <button
            type="button"
            className="icon-btn icon-btn--sm"
            aria-pressed={patterns}
            aria-label="Category patterns"
            data-tip="Add patterns to category colors (color-blind friendly)"
            onClick={() => {
              setPatterns(!patterns);
            }}
          >
            <Icon name="tag" size={16} />
          </button>
        </>
      )}
      <span className="toolbar-row__sep" aria-hidden="true" />
      <QueueButton />
    </div>
  );
}
