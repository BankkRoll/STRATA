/**
 * The path bar: a breadcrumb of the current folder whose segments jump on
 * click, and which turns into an editable path field (Ctrl+L, or a click on
 * its empty space) with folder completion from the index.
 *
 * Edit mode follows the ARIA combobox pattern: arrows pick a suggestion, Tab
 * accepts it, Enter goes, Escape cancels.
 */
import { useEffect, useId, useRef, useState, useSyncExternalStore } from "react";
import { Icon } from "../components/icons";
import type { EntryInfoProvider } from "../lib/entries";
import { volumeName, type VolumeInfo } from "../lib/volumes";
import { useServices } from "../services";
import { useApp } from "../store/app";
import { useVolumes } from "../store/volumes";
import { ariaKeys, shortcutOf } from "./keymap";
import { useLayout } from "./layout";
import { resolvePath, rootText, suggestPaths } from "./pathResolve";

/**
 * Names of the entries on a path, re-rendering as they load.
 *
 * @returns One name per id (`null` while loading); index 0 is the root.
 */
function usePathNames(provider: EntryInfoProvider | null, path: readonly number[]): (string | null)[] {
  const key = useSyncExternalStore(
    (cb) => provider?.subscribe(cb) ?? (() => undefined),
    () => (provider ? path.map((id, i) => (i === 0 ? "" : (provider.get(id)?.name ?? "\u0000"))).join("\u0001") : ""),
  );
  return key.split("\u0001").map((n) => (n === "\u0000" ? null : n));
}

/**
 * The full path text of a location.
 *
 * @param volume - Volume, if known.
 * @param names - Names below the root.
 */
export function pathText(volume: VolumeInfo | null, names: readonly (string | null)[]): string {
  const root = volume ? rootText(volume) : "";
  return [root, ...names.slice(1).map((n) => n ?? "…")].join("\\");
}

const MAX_CRUMBS = 6;

function PathEditor({ initial, onDone }: { initial: string; onDone: () => void }) {
  const services = useServices();
  const volumes = useVolumes((s) => s.volumes);
  const listId = useId();
  const errId = useId();
  const inputRef = useRef<HTMLInputElement>(null);
  const [text, setText] = useState(initial);
  const [suggestions, setSuggestions] = useState<string[]>([]);
  const [active, setActive] = useState(-1);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, []);

  const ctx = () => {
    const s = useApp.getState();
    return { volumes: volumes ?? [], rows: services.rows, current: s.volumeId !== null && s.path.length > 0 ? { volumeId: s.volumeId, path: s.path } : null };
  };

  useEffect(() => {
    let cancelled = false;
    const t = setTimeout(() => {
      suggestPaths(text, ctx()).then(
        (s) => {
          if (!cancelled) {
            setSuggestions(s);
            setActive(-1);
          }
        },
        () => {
          if (!cancelled) setSuggestions([]);
        },
      );
    }, 120);
    return () => {
      cancelled = true;
      clearTimeout(t);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps -- `ctx` reads live store state on purpose.
  }, [text]);

  const go = (value: string) => {
    setBusy(true);
    resolvePath(value, ctx()).then(
      (r) => {
        setBusy(false);
        if ("error" in r) {
          setError(r.error);
          return;
        }
        const app = useApp.getState();
        if (app.volumeId !== r.volumeId) app.openVolume(r.volumeId, r.path[0] as number);
        useApp.setState({ path: r.path, selection: [], primary: null });
        onDone();
      },
      (err: unknown) => {
        setBusy(false);
        setError(err instanceof Error ? err.message : "Couldn’t read the index.");
      },
    );
  };

  const open = suggestions.length > 0;
  return (
    <div className="pathedit">
      <input
        ref={inputRef}
        className="pathedit__input"
        role="combobox"
        aria-label="Path"
        aria-expanded={open}
        aria-controls={listId}
        aria-autocomplete="list"
        aria-activedescendant={active >= 0 ? `${listId}-${active}` : undefined}
        aria-invalid={error !== null}
        aria-describedby={error ? errId : undefined}
        spellCheck={false}
        value={text}
        onChange={(e) => {
          setText(e.target.value);
          setError(null);
        }}
        onBlur={() => {
          onDone();
        }}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.preventDefault();
            e.stopPropagation();
            onDone();
          } else if (e.key === "ArrowDown" && open) {
            e.preventDefault();
            setActive((a) => (a + 1) % suggestions.length);
          } else if (e.key === "ArrowUp" && open) {
            e.preventDefault();
            setActive((a) => (a <= 0 ? suggestions.length - 1 : a - 1));
          } else if (e.key === "Tab" && open && !e.shiftKey) {
            e.preventDefault();
            const pick = suggestions[active < 0 ? 0 : active];
            if (pick) setText(`${pick}\\`);
          } else if (e.key === "Enter") {
            e.preventDefault();
            go(active >= 0 ? (suggestions[active] ?? text) : text);
          }
        }}
      />
      {busy && <span className="spinner spinner--sm" aria-hidden="true" />}
      {open && (
        <ul id={listId} role="listbox" aria-label="Folders" className="pathedit__list">
          {suggestions.map((s, i) => (
            <li
              key={s}
              id={`${listId}-${i}`}
              role="option"
              aria-selected={i === active}
              className={i === active ? "pathedit__opt is-active" : "pathedit__opt"}
              onPointerDown={(e) => {
                e.preventDefault();
                go(s);
              }}
            >
              <Icon name="folder" size={14} />
              <span>{s}</span>
            </li>
          ))}
        </ul>
      )}
      {error && (
        <p id={errId} className="pathedit__error" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}

/** The breadcrumb / path field. */
export function PathBar() {
  const services = useServices();
  const volumeId = useApp((s) => s.volumeId);
  const path = useApp((s) => s.path);
  const jumpTo = useApp((s) => s.jumpTo);
  const volume = useVolumes((s) => s.volumes?.find((v) => v.id === volumeId) ?? null);
  const editing = useLayout((s) => s.editingPath);
  const setEditing = useLayout((s) => s.setEditingPath);
  const names = usePathNames(volumeId ? services.entryInfo(volumeId) : null, path);
  const edit = shortcutOf("editPath");
  if (editing) {
    return (
      <PathEditor
        initial={pathText(volume, names)}
        onDone={() => {
          setEditing(false);
        }}
      />
    );
  }
  const rootLabel = volume ? volumeName(volume) : "Volume";
  const crumbs = path.map((id, i) => ({ id, i, label: i === 0 ? rootLabel : (names[i] ?? "…") }));
  const shown = crumbs.length > MAX_CRUMBS ? [crumbs[0], null, ...crumbs.slice(-(MAX_CRUMBS - 2))] : crumbs;
  return (
    <nav
      className="pathbar"
      aria-label="Breadcrumb"
      onClick={(e) => {
        if (e.target === e.currentTarget || (e.target as HTMLElement).classList.contains("crumbs")) setEditing(true);
      }}
    >
      <ol className="crumbs">
        {shown.map((c, k) =>
          c ? (
            <li key={`${c.i}:${c.id}`} className="crumbs__item">
              {k > 0 && <Icon name="chevron" size={12} className="crumbs__sep" />}
              {c.i === path.length - 1 ? (
                <span className="crumbs__current" aria-current="location">
                  {c.i === 0 && <Icon name="drive" size={14} />}
                  {c.label}
                </span>
              ) : (
                <button
                  type="button"
                  className="crumbs__link"
                  onClick={() => {
                    jumpTo(c.i);
                  }}
                >
                  {c.i === 0 && <Icon name="drive" size={14} />}
                  {c.label}
                </button>
              )}
            </li>
          ) : (
            <li key="gap" className="crumbs__item">
              <Icon name="chevron" size={12} className="crumbs__sep" />
              <button
                type="button"
                className="crumbs__link"
                aria-label={`${crumbs.length - MAX_CRUMBS + 1} more folders; edit the path`}
                onClick={() => {
                  setEditing(true);
                }}
              >
                …
              </button>
            </li>
          ),
        )}
      </ol>
      <button
        type="button"
        className="icon-btn icon-btn--sm pathbar__edit"
        aria-label="Edit path"
        aria-keyshortcuts={ariaKeys(edit)}
        data-tip={`Edit path (${edit ?? ""})`}
        onClick={() => {
          setEditing(true);
        }}
      >
        <Icon name="folderOpen" size={14} />
      </button>
    </nav>
  );
}
