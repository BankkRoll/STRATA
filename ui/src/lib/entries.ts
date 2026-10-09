/**
 * Name and metadata lookups for entry ids seen in layout buffers.
 *
 * Layout records carry only ids; tooltips, labels and breadcrumbs need names
 * and a few facts. {@link EntryInfoProvider} answers synchronously from an LRU
 * cache and batches misses into one `entry_info` call per tick, so hovering
 * across a treemap never issues one IPC call per pointer move.
 */
import { call } from "./backend";
import type { Safety } from "./types";

/** Compact facts about one entry (`entry_info` response element). */
export interface EntryInfo {
  /** Entry id. */
  id: number;
  /** File or folder name (unpaired surrogates already replaced with U+FFFD). */
  name: string;
  /** Directory. */
  isDir: boolean;
  /** Allocated bytes (subtree total for directories). */
  allocated: number;
  /** Logical bytes (subtree total for directories). */
  logical: number;
  /** Files and folders below a directory; 0 for files. */
  items: number;
  /** Category id (`strata_core::Category` discriminant). */
  category: number;
  /** Owning app display name, or `null` when unattributed. */
  app: string | null;
  /** Safety tier, or `null` when unclassified. */
  safety: Safety | null;
  /** Last modified (directories: newest in subtree), Unix ms UTC, or `null`. */
  modifiedMs: number | null;
  /** Some timestamp is implausible (`EntryFlags::SUSPICIOUS_TIME`). */
  suspiciousTime: boolean;
}

/** Fetches entry infos for a batch of ids (≤ {@link MAX_BATCH}). */
export type EntryInfoFetcher = (volumeId: string, ids: number[]) => Promise<EntryInfo[]>;

/** Largest batch sent in one `entry_info` call. */
export const MAX_BATCH = 512;

/**
 * Least-recently-used map. `Map` iteration order is insertion order, so
 * re-inserting on access keeps the oldest entry first.
 */
export class LruCache<K, V> {
  private readonly map = new Map<K, V>();

  /** @param capacity - Maximum number of entries kept. */
  constructor(readonly capacity: number) {}

  /** Number of cached entries. */
  get size(): number {
    return this.map.size;
  }

  /** Returns and refreshes an entry. */
  get(key: K): V | undefined {
    const v = this.map.get(key);
    if (v !== undefined) {
      this.map.delete(key);
      this.map.set(key, v);
    }
    return v;
  }

  /** Whether `key` is cached (does not refresh it). */
  has(key: K): boolean {
    return this.map.has(key);
  }

  /** Inserts or replaces an entry, evicting the least recently used. */
  set(key: K, value: V): void {
    this.map.delete(key);
    this.map.set(key, value);
    while (this.map.size > this.capacity) {
      const oldest = this.map.keys().next();
      if (oldest.done) break;
      this.map.delete(oldest.value);
    }
  }

  /** Removes an entry. */
  delete(key: K): void {
    this.map.delete(key);
  }

  /** Removes everything. */
  clear(): void {
    this.map.clear();
  }
}

/** Synchronous-read, batched-fetch entry metadata. */
export interface EntryInfoProvider {
  /** Cached info, or `undefined` (and a fetch is scheduled). */
  get(id: number): EntryInfo | undefined;
  /** Ensures `ids` are cached; resolves when fetched (or failed). */
  load(ids: Iterable<number>): Promise<void>;
  /** Called after new infos land in the cache. Returns unsubscribe. */
  subscribe(listener: () => void): () => void;
  /** Drops cached infos (all when `ids` is omitted), e.g. after live updates. */
  invalidate(ids?: Iterable<number>): void;
}

/**
 * LRU-cached provider for one volume that coalesces misses from the same
 * tick into batched fetches.
 */
export class BatchedEntryInfoProvider implements EntryInfoProvider {
  private readonly cache: LruCache<number, EntryInfo>;
  private readonly queued = new Set<number>();
  private readonly inFlight = new Set<number>();
  private readonly listeners = new Set<() => void>();
  private waiters: (() => void)[] = [];
  private scheduled = false;

  /**
   * @param volumeId - Volume the ids belong to.
   * @param fetcher - Batch fetcher (the Tauri one in the app).
   * @param capacity - LRU capacity.
   */
  constructor(
    readonly volumeId: string,
    private readonly fetcher: EntryInfoFetcher,
    capacity = 50_000,
  ) {
    this.cache = new LruCache(capacity);
  }

  get(id: number): EntryInfo | undefined {
    const v = this.cache.get(id);
    if (v === undefined) this.enqueue(id);
    return v;
  }

  load(ids: Iterable<number>): Promise<void> {
    let any = false;
    for (const id of ids) {
      if (!this.cache.has(id)) {
        this.enqueue(id);
        any = true;
      }
    }
    if (!any) return Promise.resolve();
    return new Promise((resolve) => this.waiters.push(resolve));
  }

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  invalidate(ids?: Iterable<number>): void {
    if (!ids) {
      this.cache.clear();
      return;
    }
    for (const id of ids) this.cache.delete(id);
  }

  private enqueue(id: number): void {
    if (!this.inFlight.has(id)) this.queued.add(id);
    if (this.scheduled) return;
    this.scheduled = true;
    queueMicrotask(() => {
      this.scheduled = false;
      void this.flush();
    });
  }

  private async flush(): Promise<void> {
    const ids = [...this.queued];
    this.queued.clear();
    const waiters = this.waiters;
    this.waiters = [];
    if (ids.length === 0) {
      for (const w of waiters) w();
      return;
    }
    for (const id of ids) this.inFlight.add(id);
    const batches: number[][] = [];
    for (let i = 0; i < ids.length; i += MAX_BATCH) batches.push(ids.slice(i, i + MAX_BATCH));
    const results = await Promise.allSettled(batches.map((b) => this.fetcher(this.volumeId, b)));
    let added = false;
    for (const r of results) {
      if (r.status !== "fulfilled") continue;
      for (const info of r.value) {
        this.cache.set(info.id, info);
        added = true;
      }
    }
    for (const id of ids) this.inFlight.delete(id);
    if (added) for (const l of this.listeners) l();
    for (const w of waiters) w();
  }
}

/** The `entry_info` Tauri command as an {@link EntryInfoFetcher}. */
export const tauriEntryInfoFetcher: EntryInfoFetcher = (volumeId, ids) =>
  call<EntryInfo[]>("entry_info", { volumeId, ids });

/**
 * Fetches an entry's full path (`entry_path`), e.g. for "Copy path".
 *
 * @param volumeId - Volume id.
 * @param id - Entry id.
 * @returns The Win32 path (without the `\\?\` prefix).
 */
export function fetchEntryPath(volumeId: string, id: number): Promise<string> {
  return call<string>("entry_path", { volumeId, id });
}
