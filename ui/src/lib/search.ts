/**
 * Search plumbing for the command palette (SPEC §17).
 *
 * - {@link fuzzyMatch}: scores palette commands against the typed text.
 * - {@link SearchStream}: streaming filename results from the backend search
 *   engine (`search_open` / `search_query` / `search_close`, results on a
 *   Channel), implemented by {@link TauriSearchStream}.
 */
import { Channel } from "@tauri-apps/api/core";
import { call } from "./backend";

/** A fuzzy match: score (higher is better) and matched character positions. */
export interface FuzzyMatch {
  score: number;
  positions: number[];
}

/**
 * Subsequence fuzzy match, case-insensitive. Rewards consecutive runs,
 * word starts and an early first match; rejects when any query character is
 * missing.
 *
 * @param query - What the user typed.
 * @param text - Candidate label.
 * @returns The match, or `null` when `query` is not a subsequence of `text`.
 * @example
 * fuzzyMatch("scd", "Scan D:") // matches S, c, D
 */
export function fuzzyMatch(query: string, text: string): FuzzyMatch | null {
  const q = query.trim().toLowerCase();
  if (q.length === 0) return { score: 0, positions: [] };
  const t = text.toLowerCase();
  const positions: number[] = [];
  let score = 0;
  let ti = 0;
  let prev = -2;
  for (const ch of q) {
    if (ch === " ") continue;
    const found = t.indexOf(ch, ti);
    if (found < 0) return null;
    const atWordStart = found === 0 || /[\s\-_:\\/.(]/.test(t[found - 1] ?? "");
    score += 1;
    if (found === prev + 1) score += 3;
    if (atWordStart) score += 2;
    positions.push(found);
    prev = found;
    ti = found + 1;
  }
  score -= (positions[0] ?? 0) * 0.1;
  score -= (text.length - q.length) * 0.01;
  return { score, positions };
}

/** One filename search hit. */
export interface SearchResult {
  volumeId: string;
  id: number;
  name: string;
  /** Parent folder path. */
  parentPath: string;
  isDir: boolean;
  allocated: number;
  logical: number;
}

/** A batch of results for one query. */
export interface SearchBatch {
  /** Query sequence number this batch answers. */
  seq: number;
  results: SearchResult[];
  /** No more batches will follow for this query. */
  done: boolean;
  /** Total matches when known. */
  total: number | null;
}

/** Query options (the filter syntax is parsed by the backend). */
export interface SearchQuery {
  text: string;
  regex: boolean;
  caseSensitive: boolean;
  /** Limit to one volume, or all indexed volumes when `null`. */
  volumeId: string | null;
}

/** Streaming search. */
export interface SearchStream {
  /** Starts a query (superseding the previous one); returns its seq. */
  query(q: SearchQuery): number;
  /** Receives batches for the latest query only. Returns unsubscribe. */
  subscribe(listener: (batch: SearchBatch) => void): () => void;
  /** Receives failures. Returns unsubscribe. */
  onError(listener: (err: unknown) => void): () => void;
  close(): void;
}

/** {@link SearchStream} over the backend search engine. */
export class TauriSearchStream implements SearchStream {
  private readonly channel = new Channel<SearchBatch>();
  private readonly streamId: Promise<number>;
  private readonly listeners = new Set<(batch: SearchBatch) => void>();
  private readonly errorListeners = new Set<(err: unknown) => void>();
  private seq = 0;

  constructor() {
    this.channel.onmessage = (batch) => {
      if (batch.seq !== this.seq) return;
      for (const l of this.listeners) l(batch);
    };
    this.streamId = call<{ streamId: number }>("search_open", { onResults: this.channel }).then((r) => r.streamId);
    this.streamId.catch((err: unknown) => {
      this.emitError(err);
    });
  }

  private emitError(err: unknown): void {
    for (const l of this.errorListeners) l(err);
  }

  query(q: SearchQuery): number {
    const seq = ++this.seq;
    this.streamId
      .then((streamId) => call<null>("search_query", { streamId, seq, query: q }))
      .catch((err: unknown) => {
        this.emitError(err);
      });
    return seq;
  }

  subscribe(listener: (batch: SearchBatch) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  onError(listener: (err: unknown) => void): () => void {
    this.errorListeners.add(listener);
    return () => this.errorListeners.delete(listener);
  }

  close(): void {
    this.streamId
      .then((streamId) => call<null>("search_close", { streamId }))
      .catch(() => {
        // Nothing to release if the stream never opened.
      });
  }
}
