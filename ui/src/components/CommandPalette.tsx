/**
 * Command palette (Ctrl+K / Ctrl+F): fuzzy-matched commands plus
 * streaming filename results from the backend search engine. Implements the
 * ARIA combobox + listbox pattern; the input keeps focus and arrow keys move
 * the active option.
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { errorMessage } from "../lib/backend";
import { formatBytes } from "../lib/format";
import { fuzzyMatch, type SearchResult, type SearchStream } from "../lib/search";
import { useServices } from "../services";
import { useApp } from "../store/app";
import { useSettings } from "../store/settings";
import { useVolumes } from "../store/volumes";
import { Icon } from "./icons";
import { buildCommands, type PaletteCommand } from "./paletteCommands";

/** Props for {@link CommandPalette}. */
export interface CommandPaletteProps {
  /** Initial text (Ctrl+F opens in file-search mode). */
  initial: string;
  onClose: () => void;
}

/** Highlights matched characters. */
function Highlight({ text, positions }: { text: string; positions: number[] }) {
  if (positions.length === 0) return <>{text}</>;
  const set = new Set(positions);
  // Group consecutive characters so the accessible name stays one text run.
  const runs: { text: string; hit: boolean }[] = [];
  Array.from(text).forEach((ch, i) => {
    const hit = set.has(i);
    const last = runs[runs.length - 1];
    if (last?.hit === hit) last.text += ch;
    else runs.push({ text: ch, hit });
  });
  return (
    <>
      {runs.map((r, i) =>
        r.hit ? (
          <mark key={i} className="hl">
            {r.text}
          </mark>
        ) : (
          r.text
        ),
      )}
    </>
  );
}

/**
 * Ranks commands for `query`: enabled before disabled, then by fuzzy score.
 *
 * @returns Matching commands with highlight positions.
 */
export function rankCommands(commands: readonly PaletteCommand[], query: string): { cmd: PaletteCommand; positions: number[] }[] {
  const scored: { cmd: PaletteCommand; positions: number[]; score: number }[] = [];
  for (const cmd of commands) {
    const m = fuzzyMatch(query, cmd.title);
    if (m) scored.push({ cmd, positions: m.positions, score: m.score - (cmd.availability.enabled ? 0 : 1000) });
  }
  if (query.trim() !== "") scored.sort((a, b) => b.score - a.score);
  else scored.sort((a, b) => Number(b.cmd.availability.enabled) - Number(a.cmd.availability.enabled));
  return scored;
}

/** The palette dialog. */
export function CommandPalette({ initial, onClose }: CommandPaletteProps) {
  const services = useServices();
  const volumes = useVolumes((s) => s.volumes);
  const units = useSettings((s) => s.units);
  const [query, setQuery] = useState(initial);
  const [active, setActive] = useState(0);
  const [results, setResults] = useState<SearchResult[]>([]);
  const [searchState, setSearchState] = useState<{ busy: boolean; error: string | null; total: number | null }>({ busy: false, error: null, total: null });
  const inputRef = useRef<HTMLInputElement>(null);
  const returnFocus = useRef<Element | null>(null);
  const commands = useMemo(() => buildCommands(services, volumes ?? []), [services, volumes]);
  const ranked = rankCommands(commands, query).slice(0, 40);
  const streamRef = useRef<SearchStream | null>(null);

  useEffect(() => {
    returnFocus.current = document.activeElement;
    inputRef.current?.focus();
    inputRef.current?.select();
    return () => {
      if (returnFocus.current instanceof HTMLElement) returnFocus.current.focus();
    };
  }, []);

  useEffect(() => {
    const stream = services.createSearchStream();
    streamRef.current = stream;
    const un = stream.subscribe((batch) => {
      setResults((prev) => (batch.results.length === 0 && !batch.done ? prev : [...prev, ...batch.results].slice(0, 200)));
      setSearchState({ busy: !batch.done, error: null, total: batch.total });
    });
    const unErr = stream.onError((err) => {
      setSearchState({ busy: false, error: errorMessage(err), total: null });
    });
    return () => {
      un();
      unErr();
      stream.close();
      streamRef.current = null;
    };
  }, [services]);

  // Debounced re-query (~30 ms) while typing.
  useEffect(() => {
    const text = query.trim();
    if (text.length < 2) return;
    const t = setTimeout(() => {
      setResults([]);
      setSearchState((s) => ({ ...s, busy: true }));
      streamRef.current?.query({ text, regex: false, caseSensitive: false, volumeId: null });
    }, 30);
    return () => {
      clearTimeout(t);
    };
  }, [query, services]);

  const options: ({ kind: "cmd"; cmd: PaletteCommand; positions: number[] } | { kind: "file"; r: SearchResult })[] = [
    ...ranked.map((r) => ({ kind: "cmd" as const, ...r })),
    ...(query.trim().length >= 2 ? results : []).map((r) => ({ kind: "file" as const, r })),
  ];
  const activeIdx = Math.min(active, Math.max(0, options.length - 1));

  const runOption = (i: number) => {
    const o = options[i];
    if (!o) return;
    if (o.kind === "cmd") {
      if (!o.cmd.availability.enabled) {
        useApp.getState().notify(o.cmd.availability.reason);
        return;
      }
      onClose();
      o.cmd.run();
    } else {
      onClose();
      const app = useApp.getState();
      if (app.volumeId === o.r.volumeId) {
        app.select([o.r.id], o.r.id);
        app.setPane("detail", true);
      } else {
        app.notify("That file is on another volume; open its map first.");
      }
    }
  };

  return (
    <div
      className="palette-backdrop"
      onPointerDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="palette" role="dialog" aria-modal="true" aria-label="Command palette">
        <div className="palette__input">
          <Icon name="search" />
          <input
            ref={inputRef}
            role="combobox"
            aria-expanded="true"
            aria-controls="palette-list"
            aria-activedescendant={options.length > 0 ? `palette-opt-${activeIdx}` : undefined}
            aria-autocomplete="list"
            aria-label="Search files or type a command"
            placeholder="Search files or type a command"
            value={query}
            onChange={(e) => {
              setQuery(e.target.value);
              setActive(0);
            }}
            onKeyDown={(e) => {
              if (e.key === "ArrowDown") setActive((a) => Math.min(options.length - 1, a + 1));
              else if (e.key === "ArrowUp") setActive((a) => Math.max(0, a - 1));
              else if (e.key === "Home" && e.ctrlKey) setActive(0);
              else if (e.key === "End" && e.ctrlKey) setActive(options.length - 1);
              else if (e.key === "Enter") runOption(activeIdx);
              else if (e.key === "Escape") onClose();
              else if (e.key === "Tab") e.preventDefault();
              else return;
              e.preventDefault();
            }}
          />
        </div>
        <ul id="palette-list" role="listbox" aria-label="Results" className="palette__list">
          {options.map((o, i) => {
            const isActive = i === activeIdx;
            if (o.kind === "cmd") {
              const prev = options[i - 1];
              const header = !prev || prev.kind !== "cmd" || prev.cmd.group !== o.cmd.group;
              return (
                <li
                  key={o.cmd.id}
                  id={`palette-opt-${i}`}
                  role="option"
                  aria-selected={isActive}
                  aria-disabled={!o.cmd.availability.enabled}
                  className={isActive ? "palette__opt is-active" : "palette__opt"}
                  onPointerDown={(e) => {
                    e.preventDefault();
                    runOption(i);
                  }}
                  onPointerMove={() => {
                    if (!isActive) setActive(i);
                  }}
                >
                  {header && query.trim() === "" && <span className="palette__group">{o.cmd.group}</span>}
                  <span className="palette__title">
                    <Highlight text={o.cmd.title} positions={o.positions} />
                  </span>
                  {o.cmd.availability.enabled ? (
                    o.cmd.shortcut && <kbd>{o.cmd.shortcut}</kbd>
                  ) : (
                    <span className="palette__reason">{o.cmd.availability.reason}</span>
                  )}
                </li>
              );
            }
            return (
              <li
                key={`${o.r.volumeId}:${o.r.id}`}
                id={`palette-opt-${i}`}
                role="option"
                aria-selected={isActive}
                className={isActive ? "palette__opt is-active" : "palette__opt"}
                onPointerDown={(e) => {
                  e.preventDefault();
                  runOption(i);
                }}
              >
                <Icon name={o.r.isDir ? "folder" : "file"} />
                <span className="palette__title">
                  {o.r.name}
                  <span className="palette__path">{o.r.parentPath}</span>
                </span>
                <span className="palette__size">{formatBytes(o.r.allocated, { units })}</span>
              </li>
            );
          })}
        </ul>
        <div className="palette__status" role="status" aria-live="polite">
          {query.trim().length >= 2 &&
            (searchState.error
              ? `File search unavailable: ${searchState.error}`
              : searchState.busy
                ? "Searching…"
                : `${(searchState.total ?? results.length).toLocaleString()} files match`)}
        </div>
      </div>
    </div>
  );
}
