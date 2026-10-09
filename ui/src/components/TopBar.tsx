/**
 * Top bar: breadcrumbs, the search / command palette trigger, the size-mode
 * toggle, color mode and style choices, and pane toggles.
 */
import { COLOR_MODES, type ColorMode } from "../lib/palette";
import type { SizeMode, TreemapStyle } from "../lib/types";
import { isVisualView, useApp, type Pane } from "../store/app";
import { useSettings } from "../store/settings";
import { Breadcrumbs } from "./Breadcrumbs";
import { Icon, type IconName } from "./icons";

/** Props for {@link TopBar}. */
export interface TopBarProps {
  /** Opens the command palette. */
  onOpenPalette: () => void;
}

const SIZE_MODES: readonly { id: SizeMode; label: string; hint: string }[] = [
  { id: "allocated", label: "On disk", hint: "Allocated size: what it actually costs" },
  { id: "logical", label: "Logical", hint: "Logical size: what files claim to be" },
];

function PaneToggle({ pane, icon, label }: { pane: Pane; icon: IconName; label: string }) {
  const open = useApp((s) => s.panes[pane]);
  const toggle = useApp((s) => s.togglePane);
  return (
    <button
      type="button"
      className="icon-btn"
      aria-pressed={open}
      aria-label={label}
      title={label}
      onClick={() => {
        toggle(pane);
      }}
    >
      <Icon name={icon} />
    </button>
  );
}

/** The app's top bar. */
export function TopBar({ onOpenPalette }: TopBarProps) {
  const sizeMode = useApp((s) => s.sizeMode);
  const setSizeMode = useApp((s) => s.setSizeMode);
  const colorMode = useApp((s) => s.colorMode);
  const setColorMode = useApp((s) => s.setColorMode);
  const style = useApp((s) => s.treemapStyle);
  const setStyle = useApp((s) => s.setTreemapStyle);
  const view = useApp((s) => s.view);
  const patterns = useSettings((s) => s.patterns);
  const setPatterns = useSettings((s) => s.setPatterns);
  const visual = isVisualView(view);

  return (
    <header className="topbar">
      <Breadcrumbs />
      <button type="button" className="palette-trigger" onClick={onOpenPalette} aria-keyshortcuts="Control+K Control+F">
        <Icon name="search" />
        <span>Search files or run a command</span>
        <kbd>Ctrl+K</kbd>
      </button>
      <div className="topbar__controls">
        <div className="segmented" role="radiogroup" aria-label="Size mode">
          {SIZE_MODES.map((m) => (
            <button
              key={m.id}
              type="button"
              role="radio"
              data-mode={m.id}
              aria-checked={sizeMode === m.id}
              title={m.hint}
              tabIndex={sizeMode === m.id ? 0 : -1}
              onClick={() => {
                setSizeMode(m.id);
              }}
              onKeyDown={(e) => {
                if (e.key === "ArrowLeft" || e.key === "ArrowRight") {
                  e.preventDefault();
                  const next = sizeMode === "allocated" ? "logical" : "allocated";
                  setSizeMode(next);
                  e.currentTarget.parentElement?.querySelector<HTMLElement>(`[data-mode="${next}"]`)?.focus();
                }
              }}
            >
              {m.label}
            </button>
          ))}
        </div>
        {visual && (
          <>
            <label className="select">
              <span className="visually-hidden">Color by</span>
              <select
                value={colorMode}
                onChange={(e) => {
                  setColorMode(e.target.value as ColorMode);
                }}
                aria-label="Color by"
              >
                {COLOR_MODES.map((m) => (
                  <option key={m.id} value={m.id}>
                    Color: {m.label}
                  </option>
                ))}
              </select>
            </label>
            {view === "treemap" && (
              <label className="select">
                <span className="visually-hidden">Treemap style</span>
                <select
                  value={style}
                  onChange={(e) => {
                    setStyle(e.target.value as TreemapStyle);
                  }}
                  aria-label="Treemap style"
                >
                  <option value="flat">Style: Flat</option>
                  <option value="cushion">Style: Cushion</option>
                </select>
              </label>
            )}
            <button
              type="button"
              className="icon-btn icon-btn--text"
              aria-pressed={patterns}
              title="Add patterns to category colors (color-blind friendly)"
              onClick={() => {
                setPatterns(!patterns);
              }}
            >
              Patterns
            </button>
          </>
        )}
        <PaneToggle pane="nav" icon="nav" label="Show navigation" />
        <PaneToggle pane="list" icon="list" label="Show list pane" />
        <PaneToggle pane="detail" icon="detail" label="Show details pane" />
      </div>
    </header>
  );
}
