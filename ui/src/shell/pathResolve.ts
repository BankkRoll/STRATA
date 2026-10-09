/**
 * Typed-path navigation for the path bar: resolves `C:\Users\me` to the
 * chain of entry ids, and suggests completions, by walking the index one
 * folder at a time through the paged children source (`list_children`).
 *
 * Paths may start with a drive (`C:\…`), a volume label, or be relative to
 * the current folder (`..\Downloads`).
 */
import type { RowSource } from "../lib/rows";
import { NO_FILTERS } from "../lib/types";
import { volumeName, type VolumeInfo } from "../lib/volumes";

/** Where a path resolves to. */
export interface ResolvedPath {
  volumeId: string;
  /** Root-to-folder entry ids. */
  path: number[];
}

/** Context for resolution. */
export interface ResolveContext {
  volumes: readonly VolumeInfo[];
  rows: RowSource;
  /** Current location, for relative paths. */
  current: ResolvedPath | null;
}

const PAGE = 500;
const MAX_PAGES = 40;

/** Normalizes separators, quotes and trailing slashes. */
export function normalizePath(text: string): string {
  return text.trim().replace(/^"(.*)"$/, "$1").replace(/\//g, "\\").replace(/\\{2,}/g, "\\").replace(/\\$/, "");
}

/**
 * The text a volume root is typed as: the drive (`C:`), else the label.
 *
 * @param v - Volume.
 */
export function rootText(v: VolumeInfo): string {
  const mount = v.mountPoints[0];
  return mount ? mount.replace(/\\$/, "") : v.label || v.id;
}

function rootCandidates(v: VolumeInfo): string[] {
  return [...v.mountPoints.map((m) => m.replace(/\\$/, "")), v.label, volumeName(v)].filter((s) => s !== "");
}

/**
 * Splits a typed path into its volume and the segments below the root.
 *
 * @returns `null` when no indexed volume matches and there is no current folder.
 */
export function splitPath(text: string, ctx: Pick<ResolveContext, "volumes" | "current">): { volume: VolumeInfo | null; base: number[]; segments: string[] } | null {
  const t = normalizePath(text);
  const lower = t.toLowerCase();
  let best: { v: VolumeInfo; len: number } | null = null;
  for (const v of ctx.volumes) {
    if (v.scan.rootId === null) continue;
    for (const c of rootCandidates(v)) {
      const l = c.toLowerCase();
      if ((lower === l || lower.startsWith(`${l}\\`)) && (!best || l.length > best.len)) best = { v, len: l.length };
    }
  }
  if (best && best.v.scan.rootId !== null) {
    const rest = t.slice(best.len).replace(/^\\/, "");
    return { volume: best.v, base: [best.v.scan.rootId], segments: rest === "" ? [] : rest.split("\\") };
  }
  if (!ctx.current || /^[a-z]:/i.test(t) || t.startsWith("\\")) return null;
  const volume = ctx.volumes.find((v) => v.id === ctx.current?.volumeId) ?? null;
  return { volume, base: [...ctx.current.path], segments: t === "" ? [] : t.split("\\") };
}

async function findChild(rows: RowSource, volumeId: string, parent: number, name: string): Promise<{ id: number; isDir: boolean } | null> {
  const want = name.toLowerCase();
  for (let page = 0, offset = 0; page < MAX_PAGES; page++) {
    const r = await rows.fetchChildren({ volumeId, parent, sort: { key: "name", desc: false }, sizeMode: "allocated", offset, limit: PAGE, filters: NO_FILTERS });
    const hit = r.rows.find((row) => row.name.toLowerCase() === want);
    if (hit) return { id: hit.id, isDir: hit.isDir };
    offset += r.rows.length;
    if (r.rows.length === 0 || offset >= r.total) return null;
  }
  return null;
}

/**
 * Resolves a typed path to a folder.
 *
 * @param text - What the user typed.
 * @param ctx - Volumes, row source and current location.
 * @returns The location, or a user-facing error.
 */
export async function resolvePath(text: string, ctx: ResolveContext): Promise<ResolvedPath | { error: string }> {
  const split = splitPath(text, ctx);
  if (!split) return { error: `No scanned volume matches “${normalizePath(text)}”.` };
  const volumeId = split.volume?.id ?? ctx.current?.volumeId;
  if (!volumeId) return { error: "Open a scanned volume first." };
  const path = [...split.base];
  let parentName = split.volume ? rootText(split.volume) : "this folder";
  for (const seg of split.segments) {
    if (seg === "" || seg === ".") continue;
    if (seg === "..") {
      if (path.length > 1) path.pop();
      continue;
    }
    const child = await findChild(ctx.rows, volumeId, path[path.length - 1] as number, seg);
    if (!child) return { error: `No folder named “${seg}” in “${parentName}”.` };
    if (!child.isDir) return { error: `“${seg}” is a file, not a folder.` };
    path.push(child.id);
    parentName = seg;
  }
  return { volumeId, path };
}

/**
 * Suggests completions for the last segment of a typed path: the parent's
 * child folders whose names start with it, largest first.
 *
 * @param text - What the user typed.
 * @param ctx - Volumes, row source and current location.
 * @param limit - Maximum suggestions.
 * @returns Full path texts to offer.
 */
export async function suggestPaths(text: string, ctx: ResolveContext, limit = 8): Promise<string[]> {
  const raw = text.trimStart().replace(/\//g, "\\");
  const cut = raw.lastIndexOf("\\");
  if (cut < 0) {
    const q = normalizePath(raw).toLowerCase();
    return ctx.volumes
      .filter((v) => v.scan.rootId !== null)
      .map(rootText)
      .filter((r) => r.toLowerCase().startsWith(q))
      .slice(0, limit)
      .map((r) => `${r}\\`);
  }
  const parentText = raw.slice(0, cut);
  const partial = raw.slice(cut + 1).toLowerCase();
  if (parentText === "") return [];
  const parent = await resolvePath(parentText, ctx);
  if ("error" in parent) return [];
  const r = await ctx.rows.fetchChildren({ volumeId: parent.volumeId, parent: parent.path[parent.path.length - 1] as number, sort: { key: "size", desc: true }, sizeMode: "allocated", offset: 0, limit: 200, filters: NO_FILTERS });
  return r.rows
    .filter((row) => row.isDir && row.name.toLowerCase().startsWith(partial) && row.name.toLowerCase() !== partial)
    .slice(0, limit)
    .map((row) => `${parentText}\\${row.name}`);
}
