/**
 * The list / tree-table pane (SPEC §16.2): a virtualized ARIA treegrid of the
 * current root's children with lazy paging, sortable / resizable / choosable
 * columns, an inline %-of-parent bar, full keyboard navigation with
 * type-ahead, and copy/export. It is also the accessible alternative to every
 * visual view.
 */
import { useVirtualizer } from "@tanstack/react-virtual";
import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { errorMessage } from "../lib/backend";
import { formatPercent } from "../lib/format";
import { useEntryInfo } from "../lib/hooks";
import { categoryInfo } from "../lib/palette";
import { type Row, type SortKey } from "../lib/rows";
import { useServices } from "../services";
import { useApp } from "../store/app";
import { useSettings } from "../store/settings";
import { Icon } from "../components/icons";
import {
  COLUMNS,
  DEFAULT_COLUMNS,
  PAGE_SIZE,
  flatten,
  missingPages,
  rowSize,
  serializeRows,
  typeAhead,
  type CellContext,
  type ChildCache,
  type ColumnId,
  type FlatRow,
} from "./listModel";

const ROW_HEIGHT = 28;

/** Props for {@link ListPane}. */
export interface ListPaneProps {
  volumeId: string;
  root: number;
  /** Opens the context menu for entries at a point. */
  onContextMenu: (ids: number[], x: number, y: number) => void;
}

/** The tree-table pane. */
export function ListPane({ volumeId, root, onContextMenu }: ListPaneProps) {
  const services = useServices();
  const sizeMode = useApp((s) => s.sizeMode);
  const filters = useApp((s) => s.filters);
  const selection = useApp((s) => s.selection);
  const primary = useApp((s) => s.primary);
  const select = useApp((s) => s.select);
  const toggleSelect = useApp((s) => s.toggleSelect);
  const drillTo = useApp((s) => s.drillTo);
  const notify = useApp((s) => s.notify);
  const units = useSettings((s) => s.units);
  const provider = services.entryInfo(volumeId);
  const rootInfo = useEntryInfo(provider, root);

  const [sort, setSort] = useState<{ key: SortKey; desc: boolean }>({ key: "size", desc: true });
  const [columns, setColumns] = useState<ColumnId[]>([...DEFAULT_COLUMNS]);
  const [widths, setWidths] = useState<Partial<Record<ColumnId, number>>>({});
  const [expanded, setExpanded] = useState<ReadonlySet<number>>(new Set());
  const [caches, setCaches] = useState<ReadonlyMap<number, ChildCache>>(new Map());
  const [active, setActive] = useState(0);
  const [chooserOpen, setChooserOpen] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const gridRef = useRef<HTMLDivElement>(null);
  const typed = useRef({ text: "", at: 0 });
  const generation = useRef(0);

  const fetchPage = useCallback(
    (parent: number, page: number) => {
      const gen = generation.current;
      setCaches((prev) => {
        const next = new Map(prev);
        const c = next.get(parent) ?? { total: 0, rows: [], pages: new Set<number>(), error: null, loaded: false };
        next.set(parent, { ...c, pages: new Set(c.pages).add(page) });
        return next;
      });
      services.rows
        .fetchChildren({ volumeId, parent, sort, sizeMode, offset: page * PAGE_SIZE, limit: PAGE_SIZE, filters })
        .then((p) => {
          if (gen !== generation.current) return;
          setCaches((prev) => {
            const next = new Map(prev);
            const c = next.get(parent) ?? { total: 0, rows: [], pages: new Set<number>(), error: null, loaded: false };
            const rows = c.rows.slice();
            rows.length = p.total;
            p.rows.forEach((r, i) => {
              rows[p.offset + i] = r;
            });
            next.set(parent, { total: p.total, rows, pages: c.pages, error: null, loaded: true });
            return next;
          });
        })
        .catch((err: unknown) => {
          if (gen !== generation.current) return;
          setCaches((prev) => {
            const next = new Map(prev);
            const c = next.get(parent) ?? { total: 0, rows: [], pages: new Set<number>(), error: null, loaded: false };
            const pages = new Set(c.pages);
            pages.delete(page);
            next.set(parent, { ...c, pages, error: errorMessage(err) });
            return next;
          });
        });
    },
    [services, volumeId, sort, sizeMode, filters],
  );

  // Root, sort, size mode or filters changed: everything cached is stale.
  useEffect(() => {
    generation.current++;
    setCaches(new Map());
    setActive(0);
    fetchPage(root, 0);
  }, [root, fetchPage]);

  useEffect(() => {
    setExpanded(new Set());
  }, [root, volumeId]);

  const flat = useMemo(() => flatten(root, caches, expanded), [root, caches, expanded]);
  const rootCache = caches.get(root);

  // NOTE: TanStack Virtual returns a mutable instance the React Compiler cannot
  // memoize; this component opts out of compilation, which is what the
  // library documents for React 19.
  // eslint-disable-next-line react-hooks/incompatible-library
  const virtualizer = useVirtualizer({
    count: flat.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 12,
  });
  const items = virtualizer.getVirtualItems();
  const firstItem = items[0]?.index ?? 0;
  const lastItem = items[items.length - 1]?.index ?? 0;

  useEffect(() => {
    for (const [parent, page] of missingPages(flat, firstItem, lastItem + 1, caches)) fetchPage(parent, page);
  }, [flat, firstItem, lastItem, caches, fetchPage]);

  // Keep the active row on the selection made elsewhere (treemap clicks).
  useEffect(() => {
    if (primary === null) return;
    const i = flat.findIndex((f) => f.row?.id === primary);
    if (i >= 0 && i !== active) {
      setActive(i);
      virtualizer.scrollToIndex(i, { align: "auto" });
    }
    // Only react to selection changes, not to every page load.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [primary]);

  const visibleColumns = COLUMNS.filter((c) => columns.includes(c.id));
  const widthOf = (id: ColumnId) => widths[id] ?? COLUMNS.find((c) => c.id === id)?.width ?? 100;
  const template = visibleColumns.map((c) => `${widthOf(c.id)}px`).join(" ");
  const totalWidth = visibleColumns.reduce((a, c) => a + widthOf(c.id), 0);

  const sizeById = useMemo(() => {
    const m = new Map<number, number>();
    for (const c of caches.values()) for (const r of c.rows) if (r) m.set(r.id, rowSize(r, sizeMode));
    return m;
  }, [caches, sizeMode]);
  const nameById = useMemo(() => {
    const m = new Map<number, { name: string; parent: number }>();
    for (const c of caches.values()) for (const r of c.rows) if (r) m.set(r.id, { name: r.name, parent: r.parent });
    return m;
  }, [caches]);
  const rootSize = rootInfo ? (sizeMode === "allocated" ? rootInfo.allocated : rootInfo.logical) : 0;

  const ctx: CellContext = {
    units,
    sizeMode,
    appName: (id) => services.appName(id),
    now: Date.now(),
    pathOf: (row) => {
      const parts = [row.name];
      let p = row.parent;
      for (let guard = 0; p !== root && guard < 512; guard++) {
        const n = nameById.get(p);
        if (!n) break;
        parts.push(n.name);
        p = n.parent;
      }
      parts.push(rootInfo?.name ?? "");
      return parts.reverse().join("\\");
    },
  };

  const focusRow = (i: number) => {
    const clamped = Math.max(0, Math.min(flat.length - 1, i));
    setActive(clamped);
    virtualizer.scrollToIndex(clamped, { align: "auto" });
    requestAnimationFrame(() => {
      gridRef.current?.querySelector<HTMLElement>(`[data-index="${clamped}"]`)?.focus();
    });
    const row = flat[clamped]?.row;
    if (row) select([row.id], row.id);
  };

  const toggle = (row: Row, open?: boolean) => {
    const isOpen = expanded.has(row.id);
    const want = open ?? !isOpen;
    if (want === isOpen) return;
    const next = new Set(expanded);
    if (want) {
      next.add(row.id);
      if (!caches.has(row.id)) fetchPage(row.id, 0);
    } else {
      next.delete(row.id);
    }
    setExpanded(next);
  };

  const selectedRows = () => flat.flatMap((f) => (f.row && selection.includes(f.row.id) ? [f.row] : []));

  const copy = () => {
    const rows = selectedRows();
    if (rows.length === 0) return;
    void navigator.clipboard
      .writeText(serializeRows(rows, visibleColumns, ctx, "tsv"))
      .then(() => {
        notify(`${rows.length} ${rows.length === 1 ? "row" : "rows"} copied.`);
      })
      .catch(() => {
        notify("Copy failed: clipboard unavailable.");
      });
  };

  const exportCsv = () => {
    const rows = flat.flatMap((f) => (f.row ? [f.row] : []));
    const blob = new Blob([serializeRows(rows, visibleColumns, ctx, "csv")], { type: "text/csv" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `${rootInfo?.name ?? "strata"}-list.csv`;
    a.click();
    URL.revokeObjectURL(url);
    notify(`Exported ${rows.length} loaded rows.`);
  };

  const onKeyDown = (e: ReactKeyboardEvent) => {
    const f = flat[active];
    const page = Math.max(1, Math.floor((scrollRef.current?.clientHeight ?? 300) / ROW_HEIGHT) - 1);
    switch (e.key) {
      case "ArrowDown":
        focusRow(active + 1);
        break;
      case "ArrowUp":
        focusRow(active - 1);
        break;
      case "PageDown":
        focusRow(active + page);
        break;
      case "PageUp":
        focusRow(active - page);
        break;
      case "Home":
        focusRow(0);
        break;
      case "End":
        focusRow(flat.length - 1);
        break;
      case "ArrowRight":
        if (f?.row?.isDir) {
          if (!expanded.has(f.row.id)) toggle(f.row, true);
          else focusRow(active + 1);
        }
        break;
      case "ArrowLeft":
        if (f?.row && expanded.has(f.row.id)) toggle(f.row, false);
        else if (f && f.level > 1) focusRow(flat.findIndex((x) => x.row?.id === f.parent));
        break;
      case "Enter":
        if (f?.row?.isDir) drillTo(chainTo(f.row.id));
        break;
      case " ":
        if (f?.row) {
          if (e.ctrlKey) toggleSelect(f.row.id);
          else select([f.row.id], f.row.id);
        }
        break;
      case "c":
      case "C":
        if (e.ctrlKey && !e.shiftKey) copy();
        else {
          typeKey(e);
          return;
        }
        break;
      default:
        {
          typeKey(e);
          return;
        }
    }
    e.preventDefault();
  };

  const typeKey = (e: ReactKeyboardEvent) => {
    if (e.key.length !== 1 || e.ctrlKey || e.altKey || e.metaKey) return;
    const now = performance.now();
    typed.current = { text: now - typed.current.at < 600 ? typed.current.text + e.key : e.key, at: now };
    const i = typeAhead(flat, typed.current.text.length > 1 ? active - 1 : active, typed.current.text);
    if (i >= 0) focusRow(i);
    e.preventDefault();
  };

  /** Ids from below the root down to `id`, via loaded parents. */
  const chainTo = (id: number): number[] => {
    const chain = [id];
    let p = nameById.get(id)?.parent;
    for (let guard = 0; p !== undefined && p !== root && guard < 512; guard++) {
      chain.unshift(p);
      p = nameById.get(p)?.parent;
    }
    return chain;
  };

  const onSort = (key: SortKey | null) => {
    if (!key) return;
    setSort((s) => (s.key === key ? { key, desc: !s.desc } : { key, desc: key === "size" || key === "items" || key === "modified" }));
  };

  const startResize = (id: ColumnId, startX: number) => {
    const start = widthOf(id);
    const move = (ev: PointerEvent) => {
      setWidths((w) => ({ ...w, [id]: Math.max(48, start + ev.clientX - startX) }));
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };

  const ariaSort = (key: SortKey | null): "ascending" | "descending" | "none" | undefined =>
    key === null ? undefined : sort.key === key ? (sort.desc ? "descending" : "ascending") : "none";

  let body;
  if (rootCache?.error && rootCache.total === 0) {
    body = (
      <div className="state state--error" role="alert">
        <h2>Can’t list this folder</h2>
        <p>{rootCache.error}</p>
      </div>
    );
  } else if (!rootCache?.loaded) {
    body = (
      <div className="state state--quiet" role="status">
        <span className="spinner" aria-hidden="true" />
        <p>Loading…</p>
      </div>
    );
  } else if (rootCache.total === 0) {
    body = (
      <div className="state state--quiet" role="status">
        <p>{filters.excluded.length > 0 || filters.categories.length > 0 || filters.minBytes > 0 ? "No items match the current filters." : "This folder is empty."}</p>
      </div>
    );
  }

  return (
    <section className="list" aria-label="List">
      <div className="list__toolbar">
        <span className="list__count" aria-live="polite">
          {rootCache ? `${rootCache.total.toLocaleString()} items` : ""}
        </span>
        <button type="button" className="btn btn--small" onClick={copy} disabled={selection.length === 0} title="Copy selected rows (Ctrl+C)">
          Copy
        </button>
        <button type="button" className="btn btn--small" onClick={exportCsv} disabled={flat.length === 0}>
          Export CSV
        </button>
        <div className="chooser">
          <button
            type="button"
            className="btn btn--small"
            aria-expanded={chooserOpen}
            aria-controls="column-chooser"
            onClick={() => {
              setChooserOpen(!chooserOpen);
            }}
          >
            Columns <Icon name="caret" size={10} />
          </button>
          {chooserOpen && (
            <fieldset id="column-chooser" className="chooser__panel">
              <legend className="visually-hidden">Visible columns</legend>
              {COLUMNS.map((c) => (
                <label key={c.id}>
                  <input
                    type="checkbox"
                    checked={columns.includes(c.id)}
                    disabled={c.id === "name"}
                    onChange={(e) => {
                      const on = e.target.checked;
                      setColumns((cols) => (on ? COLUMNS.map((x) => x.id).filter((id) => id === c.id || cols.includes(id)) : cols.filter((id) => id !== c.id)));
                    }}
                    onKeyDown={(e) => {
                      if (e.key === "Escape") setChooserOpen(false);
                    }}
                  />
                  {c.label}
                </label>
              ))}
            </fieldset>
          )}
        </div>
      </div>
      <div ref={scrollRef} className="list__scroll">
        <div
          ref={gridRef}
          role="treegrid"
          aria-label={`Contents of ${rootInfo?.name ?? "the current folder"}`}
          aria-rowcount={flat.length + 1}
          aria-colcount={visibleColumns.length}
          aria-multiselectable="true"
          className="grid"
          style={{ width: totalWidth }}
          onKeyDown={onKeyDown}
        >
          <div role="row" aria-rowindex={1} className="grid__header" style={{ gridTemplateColumns: template }}>
            {visibleColumns.map((c) => (
              <div key={c.id} role="columnheader" aria-sort={ariaSort(c.sort)} className={c.numeric ? "grid__th grid__th--num" : "grid__th"}>
                {c.sort ? (
                  <button
                    type="button"
                    className="grid__sort"
                    onClick={() => {
                      onSort(c.sort);
                    }}
                  >
                    {c.label}
                    {sort.key === c.sort && c.id !== "percent" && <span aria-hidden="true">{sort.desc ? " ▾" : " ▴"}</span>}
                  </button>
                ) : (
                  <span>{c.label}</span>
                )}
                <div
                  role="separator"
                  aria-orientation="vertical"
                  aria-label={`Resize ${c.label} column`}
                  aria-valuenow={widthOf(c.id)}
                  aria-valuemin={48}
                  aria-valuemax={800}
                  tabIndex={0}
                  className="grid__resize"
                  onPointerDown={(e) => {
                    e.preventDefault();
                    startResize(c.id, e.clientX);
                  }}
                  onKeyDown={(e) => {
                    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
                    e.preventDefault();
                    e.stopPropagation();
                    const w = widthOf(c.id) + (e.key === "ArrowRight" ? 16 : -16);
                    setWidths((ws) => ({ ...ws, [c.id]: Math.min(800, Math.max(48, w)) }));
                  }}
                />
              </div>
            ))}
          </div>
          {body ?? (
            <div className="grid__body" style={{ height: virtualizer.getTotalSize() }}>
              {items.map((vi) => {
                const f = flat[vi.index];
                if (!f) return null;
                return (
                  <GridRow
                    key={vi.key}
                    f={f}
                    index={vi.index}
                    top={vi.start}
                    template={template}
                    columns={visibleColumns.map((c) => c.id)}
                    ctx={ctx}
                    active={vi.index === active}
                    selected={f.row ? selection.includes(f.row.id) : false}
                    expanded={f.row ? expanded.has(f.row.id) : false}
                    parentSize={f.parent === root ? rootSize : (sizeById.get(f.parent) ?? 0)}
                    onToggle={toggle}
                    onClick={(e) => {
                      setActive(vi.index);
                      if (!f.row) return;
                      if (e.ctrlKey) toggleSelect(f.row.id);
                      else select([f.row.id], f.row.id);
                    }}
                    onDoubleClick={() => {
                      if (f.row?.isDir) drillTo(chainTo(f.row.id));
                    }}
                    onContextMenu={(e) => {
                      e.preventDefault();
                      if (!f.row) return;
                      const ids = selection.includes(f.row.id) ? selection : [f.row.id];
                      if (!selection.includes(f.row.id)) select([f.row.id], f.row.id);
                      onContextMenu([...ids], e.clientX, e.clientY);
                    }}
                  />
                );
              })}
            </div>
          )}
        </div>
      </div>
    </section>
  );
}

interface GridRowProps {
  f: FlatRow;
  index: number;
  top: number;
  template: string;
  columns: ColumnId[];
  ctx: CellContext;
  active: boolean;
  selected: boolean;
  expanded: boolean;
  parentSize: number;
  onToggle: (row: Row) => void;
  onClick: (e: React.MouseEvent) => void;
  onDoubleClick: () => void;
  onContextMenu: (e: React.MouseEvent) => void;
}

function GridRow({ f, index, top, template, columns, ctx, active, selected, expanded, parentSize, onToggle, onClick, onDoubleClick, onContextMenu }: GridRowProps) {
  const row = f.row;
  return (
    <div
      role="row"
      data-index={index}
      aria-rowindex={index + 2}
      aria-level={f.level}
      aria-posinset={f.pos + 1}
      aria-setsize={f.setSize}
      aria-selected={selected}
      aria-expanded={row?.isDir ? expanded : undefined}
      aria-busy={row ? undefined : true}
      tabIndex={active ? 0 : -1}
      className={`grid__row${selected ? " is-selected" : ""}`}
      style={{ transform: `translateY(${top}px)`, gridTemplateColumns: template }}
      onClick={onClick}
      onDoubleClick={onDoubleClick}
      onContextMenu={onContextMenu}
    >
      {columns.map((id) => {
        const col = COLUMNS.find((c) => c.id === id);
        if (!col) return null;
        if (!row) {
          return (
            <div key={id} role="gridcell" className="grid__cell">
              {id === "name" && <span className="skeleton" style={{ marginLeft: (f.level - 1) * 16 + 20 }} />}
            </div>
          );
        }
        if (id === "name") {
          return (
            <div key={id} role="gridcell" className="grid__cell grid__name" style={{ paddingLeft: (f.level - 1) * 16 + 4 }}>
              {row.isDir ? (
                <button
                  type="button"
                  tabIndex={-1}
                  className={expanded ? "twisty is-open" : "twisty"}
                  aria-hidden="true"
                  onClick={(e) => {
                    e.stopPropagation();
                    onToggle(row);
                  }}
                >
                  <Icon name="chevron" size={12} />
                </button>
              ) : (
                <span className="twisty-spacer" />
              )}
              <span className="cat-dot" style={{ background: categoryInfo(row.category).light }} aria-hidden="true" />
              <span className="grid__text">{row.name}</span>
            </div>
          );
        }
        if (id === "percent") {
          const pct = parentSize > 0 ? rowSize(row, ctx.sizeMode) / parentSize : 0;
          return (
            <div key={id} role="gridcell" className="grid__cell grid__pct">
              <span className="pct-bar" aria-hidden="true">
                <span style={{ width: `${Math.min(100, pct * 100)}%` }} />
              </span>
              <span>{formatPercent(pct)}</span>
            </div>
          );
        }
        return (
          <div key={id} role="gridcell" className={col.numeric ? "grid__cell grid__cell--num" : "grid__cell"}>
            {col.text(row, ctx)}
          </div>
        );
      })}
    </div>
  );
}
