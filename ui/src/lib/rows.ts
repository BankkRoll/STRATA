/**
 * Paged rows for the list / tree-table pane.
 *
 * `list_children` returns one binary row page (bulk data travels as binary,
 * not JSON): a 32-byte header, fixed 64-byte row records, then a UTF-16LE
 * name blob. {@link decodeRowPage} reads it; {@link RowSource} is what the
 * table depends on.
 */
import { call } from "./backend";
import { epoch2000ToMs } from "./format";
import type { Safety, ViewFilters } from "./types";

/** `"STRP"` read as a little-endian u32. */
export const ROW_PAGE_MAGIC = 0x50525453;
/** Row page format version. */
export const ROW_PAGE_VERSION = 1;
/** Row page header size. */
export const ROW_PAGE_HEADER_BYTES = 32;
/** Bytes per row record. */
export const ROW_STRIDE = 64;

/** `strata_core::EntryFlags` bits the UI displays. */
export const EntryFlag = {
  DIR: 1 << 0,
  HIDDEN: 1 << 1,
  SYSTEM: 1 << 2,
  READONLY: 1 << 3,
  COMPRESSED: 1 << 4,
  SPARSE: 1 << 5,
  ENCRYPTED: 1 << 6,
  HAS_ADS: 1 << 7,
  HARDLINK_SECONDARY: 1 << 8,
  ORPHAN: 1 << 9,
  ACCESS_DENIED: 1 << 10,
  DELETE_PENDING: 1 << 11,
  NTFS_METADATA: 1 << 12,
  VIRTUAL: 1 << 13,
  SUSPICIOUS_TIME: 1 << 14,
  TEMPORARY: 1 << 15,
  OFFLINE: 1 << 16,
  PARTIAL: 1 << 17,
  CYCLE_BROKEN: 1 << 18,
  ALLOC_ESTIMATED: 1 << 19,
} as const;

/** Short attribute letters/labels for the attributes column, in display order. */
export const ATTRIBUTE_LABELS: readonly [number, string, string][] = [
  [EntryFlag.READONLY, "R", "Read-only"],
  [EntryFlag.HIDDEN, "H", "Hidden"],
  [EntryFlag.SYSTEM, "S", "System"],
  [EntryFlag.COMPRESSED, "C", "Compressed"],
  [EntryFlag.SPARSE, "P", "Sparse"],
  [EntryFlag.ENCRYPTED, "E", "Encrypted"],
  [EntryFlag.TEMPORARY, "T", "Temporary"],
  [EntryFlag.OFFLINE, "O", "Offline"],
  [EntryFlag.HAS_ADS, "A", "Alternate data streams"],
  [EntryFlag.HARDLINK_SECONDARY, "L", "Hardlink (counted elsewhere)"],
];

const SAFETY_BY_CODE: readonly (Safety | null)[] = [null, "safe", "probably", "careful", "never"];

/** One table row. */
export interface Row {
  id: number;
  parent: number;
  name: string;
  /** `strata_core::EntryFlags` bits. */
  flags: number;
  isDir: boolean;
  category: number;
  safety: Safety | null;
  allocated: number;
  logical: number;
  /** Files and folders below (directories); 0 for files. */
  items: number;
  /** Direct children (directories); 0 for files. */
  childCount: number;
  modifiedMs: number | null;
  createdMs: number | null;
  accessedMs: number | null;
  /** App catalog id (0 = unattributed). */
  appId: number;
}

/** One page of children of `parent`. */
export interface RowPage {
  parent: number;
  /** Total children after filters. */
  total: number;
  /** Index of the first row in this page. */
  offset: number;
  rows: Row[];
}

/** Sortable columns understood by `list_children`. */
export type SortKey = "name" | "size" | "items" | "modified" | "created" | "accessed" | "category" | "safety" | "app";

/** A page request. */
export interface RowQuery {
  volumeId: string;
  parent: number;
  sort: { key: SortKey; desc: boolean };
  /** Size mode decides what "size" sorts by. */
  sizeMode: "allocated" | "logical";
  offset: number;
  limit: number;
  filters: ViewFilters;
}

/** Paged children source for the tree-table. */
export interface RowSource {
  /** Fetches one page of `query.parent`'s children. */
  fetchChildren(query: RowQuery): Promise<RowPage>;
}

/** Raised for malformed row pages. */
export class RowPageError extends Error {
  override name = "RowPageError";
}

function u64(v: DataView, o: number): number {
  return v.getUint32(o, true) + v.getUint32(o + 4, true) * 0x1_0000_0000;
}

const utf16 = new TextDecoder("utf-16le");

/**
 * Decodes a binary row page.
 *
 * @param input - Response bytes of `list_children`.
 * @returns The page.
 * @throws {RowPageError} On a bad header or out-of-bounds names.
 */
export function decodeRowPage(input: ArrayBuffer | Uint8Array): RowPage {
  const bytes = input instanceof Uint8Array ? input : new Uint8Array(input);
  if (bytes.byteLength < ROW_PAGE_HEADER_BYTES) throw new RowPageError("row page too short");
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (v.getUint32(0, true) !== ROW_PAGE_MAGIC) throw new RowPageError("bad row page magic");
  if (v.getUint16(4, true) !== ROW_PAGE_VERSION) throw new RowPageError("unsupported row page version");
  const parent = v.getUint32(8, true);
  const total = v.getUint32(12, true);
  const offset = v.getUint32(16, true);
  const count = v.getUint32(20, true);
  const namesOff = v.getUint32(24, true);
  const namesLen = v.getUint32(28, true);
  if (ROW_PAGE_HEADER_BYTES + count * ROW_STRIDE > namesOff || namesOff + namesLen > bytes.byteLength || namesLen % 2 !== 0) {
    throw new RowPageError("row page sections out of bounds");
  }
  const names = bytes.subarray(namesOff, namesOff + namesLen);
  const rows: Row[] = [];
  for (let i = 0; i < count; i++) {
    const o = ROW_PAGE_HEADER_BYTES + i * ROW_STRIDE;
    const nameOff = v.getUint32(o + 56, true) * 2;
    const nameLen = v.getUint32(o + 60, true) * 2;
    if (nameOff + nameLen > names.byteLength) throw new RowPageError(`row ${i} name out of bounds`);
    const flags = v.getUint32(o + 8, true);
    rows.push({
      id: v.getUint32(o, true),
      parent: v.getUint32(o + 4, true),
      flags,
      isDir: (flags & EntryFlag.DIR) !== 0,
      category: v.getUint16(o + 12, true),
      safety: SAFETY_BY_CODE[v.getUint8(o + 14)] ?? null,
      allocated: u64(v, o + 16),
      logical: u64(v, o + 24),
      items: v.getUint32(o + 32, true),
      childCount: v.getUint32(o + 36, true),
      modifiedMs: epoch2000ToMs(v.getUint32(o + 40, true)),
      createdMs: epoch2000ToMs(v.getUint32(o + 44, true)),
      accessedMs: epoch2000ToMs(v.getUint32(o + 48, true)),
      appId: v.getUint32(o + 52, true),
      name: utf16.decode(names.subarray(nameOff, nameOff + nameLen)),
    });
  }
  return { parent, total, offset, rows };
}

/** {@link RowSource} backed by the `list_children` command. */
export const tauriRowSource: RowSource = {
  async fetchChildren(query) {
    const bytes = await call<ArrayBuffer>("list_children", { query });
    return decodeRowPage(bytes);
  },
};

/** One entry of the app catalog summary (`apps_brief`). */
export interface AppBrief {
  id: number;
  name: string;
}

/**
 * Fetches app id → display name for the app column (`apps_brief`).
 *
 * @returns Catalog entries.
 */
export function fetchAppsBrief(): Promise<AppBrief[]> {
  return call<AppBrief[]>("apps_brief");
}
