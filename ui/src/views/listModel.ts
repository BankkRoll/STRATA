/**
 * Pure model behind the tree-table: paged child caches, flattening of the
 * expanded tree into visible rows, columns, type-ahead and export. Kept free
 * of React so it is unit-tested directly.
 */
import { describeTimestamp, formatBytes, formatCount, formatDateTime, type SizeUnits } from "../lib/format";
import { categoryInfo } from "../lib/palette";
import { ATTRIBUTE_LABELS, type Row, type SortKey } from "../lib/rows";
import type { SizeMode } from "../lib/types";

/** Rows per `list_children` page. */
export const PAGE_SIZE = 200;

/** Loaded children of one parent; `undefined` slots are not fetched yet. */
export interface ChildCache {
  total: number;
  rows: (Row | undefined)[];
  /** Page indices requested (in flight or done). */
  pages: Set<number>;
  error: string | null;
  /** At least one page arrived, so `total` is real. */
  loaded: boolean;
}

/** One visible line of the tree-table. */
export interface FlatRow {
  /** Row data, or `null` while its page loads. */
  row: Row | null;
  /** Parent entry id. */
  parent: number;
  /** 1-based tree level (`aria-level`). */
  level: number;
  /** Position among siblings (0-based) and sibling count. */
  pos: number;
  setSize: number;
}

/**
 * Flattens the expanded tree under `root` into visible rows, depth first.
 *
 * @param root - Current layout root.
 * @param caches - Loaded children per parent.
 * @param expanded - Expanded directory ids.
 * @returns Visible rows in display order.
 */
export function flatten(root: number, caches: ReadonlyMap<number, ChildCache>, expanded: ReadonlySet<number>): FlatRow[] {
  const out: FlatRow[] = [];
  // Explicit stack: real folder chains can be deeper than the JS stack likes.
  const stack: { parent: number; level: number; next: number }[] = [{ parent: root, level: 1, next: 0 }];
  while (stack.length > 0) {
    const top = stack[stack.length - 1];
    if (!top) break;
    const cache = caches.get(top.parent);
    if (!cache || top.next >= cache.total) {
      stack.pop();
      continue;
    }
    const pos = top.next++;
    const row = cache.rows[pos] ?? null;
    out.push({ row, parent: top.parent, level: top.level, pos, setSize: cache.total });
    if (row?.isDir && expanded.has(row.id) && stack.length < 512) {
      stack.push({ parent: row.id, level: top.level + 1, next: 0 });
    }
  }
  return out;
}

/**
 * Page indices that must be fetched to fill `flat[start..end)`.
 *
 * @returns `[parent, page]` pairs not yet requested.
 */
export function missingPages(
  flat: readonly FlatRow[],
  start: number,
  end: number,
  caches: ReadonlyMap<number, ChildCache>,
): [number, number][] {
  const out: [number, number][] = [];
  const seen = new Set<string>();
  for (let i = Math.max(0, start); i < Math.min(flat.length, end); i++) {
    const f = flat[i];
    if (!f || f.row) continue;
    const page = Math.floor(f.pos / PAGE_SIZE);
    const key = `${f.parent}:${page}`;
    if (seen.has(key) || caches.get(f.parent)?.pages.has(page)) continue;
    seen.add(key);
    out.push([f.parent, page]);
  }
  return out;
}

/**
 * Type-ahead: the next loaded row after `from` whose name starts with
 * `prefix` (case-insensitive), wrapping around.
 *
 * @returns Index into `flat`, or -1.
 */
export function typeAhead(flat: readonly FlatRow[], from: number, prefix: string): number {
  const p = prefix.toLocaleLowerCase();
  const n = flat.length;
  for (let k = 1; k <= n; k++) {
    const i = (from + k + n) % n;
    if (flat[i]?.row?.name.toLocaleLowerCase().startsWith(p)) return i;
  }
  return -1;
}

/** Column ids. */
export type ColumnId =
  | "name"
  | "allocated"
  | "logical"
  | "percent"
  | "items"
  | "modified"
  | "created"
  | "accessed"
  | "category"
  | "app"
  | "safety"
  | "attributes"
  | "path";

/** Formatting context for cells. */
export interface CellContext {
  units: SizeUnits;
  sizeMode: SizeMode;
  appName: (id: number) => string | null;
  /** Folder path of the root, for the path column. */
  pathOf: (row: Row) => string;
  now: number;
}

/** A column definition. */
export interface Column {
  id: ColumnId;
  label: string;
  /** Default width in CSS px. */
  width: number;
  /** Server sort key, or `null` when not sortable. */
  sort: SortKey | null;
  numeric: boolean;
  /** Plain-text cell value (also used for copy/export). */
  text(row: Row, ctx: CellContext): string;
}

const SAFETY_TEXT = { safe: "Safe", probably: "Probably", careful: "Careful", never: "Never" } as const;

/** All columns (SPEC §16.2), in default order. */
export const COLUMNS: readonly Column[] = [
  { id: "name", label: "Name", width: 280, sort: "name", numeric: false, text: (r) => r.name },
  { id: "allocated", label: "On disk", width: 96, sort: "size", numeric: true, text: (r, c) => formatBytes(r.allocated, { units: c.units }) },
  { id: "logical", label: "Logical", width: 96, sort: "size", numeric: true, text: (r, c) => formatBytes(r.logical, { units: c.units }) },
  { id: "percent", label: "% of parent", width: 120, sort: "size", numeric: true, text: () => "" },
  { id: "items", label: "Items", width: 80, sort: "items", numeric: true, text: (r) => (r.isDir ? formatCount(r.items) : "") },
  {
    id: "modified",
    label: "Modified",
    width: 150,
    sort: "modified",
    numeric: false,
    text: (r, c) => {
      const t = describeTimestamp(r.modifiedMs, c.now);
      return t.suspicious ? `${t.absolute} (suspicious)` : t.absolute;
    },
  },
  { id: "created", label: "Created", width: 150, sort: "created", numeric: false, text: (r) => formatDateTime(r.createdMs) },
  { id: "accessed", label: "Accessed", width: 150, sort: "accessed", numeric: false, text: (r) => formatDateTime(r.accessedMs) },
  { id: "category", label: "Category", width: 120, sort: "category", numeric: false, text: (r) => categoryInfo(r.category).label },
  { id: "app", label: "App", width: 130, sort: "app", numeric: false, text: (r, c) => (r.appId === 0 ? "" : (c.appName(r.appId) ?? `App #${r.appId}`)) },
  { id: "safety", label: "Safety", width: 90, sort: "safety", numeric: false, text: (r) => (r.safety ? SAFETY_TEXT[r.safety] : "") },
  {
    id: "attributes",
    label: "Attributes",
    width: 90,
    sort: null,
    numeric: false,
    text: (r) =>
      ATTRIBUTE_LABELS.filter(([bit]) => (r.flags & bit) !== 0)
        .map(([, letter]) => letter)
        .join(""),
  },
  { id: "path", label: "Path", width: 260, sort: null, numeric: false, text: (r, c) => c.pathOf(r) },
];

/** Columns shown by default. */
export const DEFAULT_COLUMNS: readonly ColumnId[] = ["name", "allocated", "logical", "percent", "items", "modified", "category", "safety"];

/** Size of a row in the active mode. */
export function rowSize(row: Row, mode: SizeMode): number {
  return mode === "allocated" ? row.allocated : row.logical;
}

/**
 * Serializes rows as tab-separated text (clipboard) or CSV (export).
 *
 * @param rows - Rows to write.
 * @param columns - Columns, in order.
 * @param ctx - Formatting context.
 * @param format - `tsv` or `csv`.
 * @returns Text with a header line, CRLF line endings (Windows apps expect them).
 */
export function serializeRows(rows: readonly Row[], columns: readonly Column[], ctx: CellContext, format: "tsv" | "csv"): string {
  const cell = (s: string) => {
    if (format === "tsv") return s.replace(/[\t\r\n]/g, " ");
    return /[",\r\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
  };
  const sep = format === "tsv" ? "\t" : ",";
  const cols = columns.filter((c) => c.id !== "percent");
  // Exports carry exact byte counts so spreadsheets can sum them.
  const value = (c: Column, r: Row) =>
    c.id === "allocated" ? String(r.allocated) : c.id === "logical" ? String(r.logical) : c.text(r, ctx);
  const lines = [cols.map((c) => cell(c.id === "allocated" || c.id === "logical" ? `${c.label} (bytes)` : c.label)).join(sep)];
  for (const r of rows) lines.push(cols.map((c) => cell(value(c, r))).join(sep));
  return lines.join("\r\n");
}
