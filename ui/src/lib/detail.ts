/**
 * The detail panel's data contract (SPEC §16.3): `entry_detail` returns one
 * {@link EntryDetail} as JSON. Sections the backend cannot fill yet are
 * `null` (unknown) rather than empty, so the panel can tell "none" from
 * "not available".
 */
import { call } from "./backend";
import type { Safety } from "./types";

/** Reparse kind (`strata_core::ReparseKind`, serde names). */
export type ReparseKind = "symlink" | "mount_point" | "wof" | "cloud" | "dedup" | "app_exec_link" | "wsl" | "unknown";

/** Cloud placeholder state (`strata_core::CloudState`). */
export type CloudState = "online_only" | "locally_available" | "always_keep";

/** Attribution confidence (SPEC §12.3). */
export type Confidence = "exact" | "high" | "heuristic";

/** One timestamp with its provenance. Times are Unix ms UTC. */
export interface DetailTime {
  ms: number | null;
  /** Implausible (pre-1990 or future). */
  suspicious: boolean;
}

/** Full detail of one entry. */
export interface EntryDetail {
  id: number;
  volumeId: string;
  name: string;
  /** Full Win32 path without the `\\?\` prefix. */
  path: string;
  isDir: boolean;
  /** Shell icon as a PNG data URL (cached per extension by the backend), or `null`. */
  iconDataUrl: string | null;
  sizes: {
    logical: number;
    allocated: number;
    /** Named streams. */
    adsLogical: number;
    adsAllocated: number;
    /** Directory index overhead (directories only). */
    dirOverhead: number;
    /** Allocated/logical, or `null` when logical is 0. */
    compressionRatio: number | null;
    /** Allocated is an estimate (walker before its allocation pass). */
    estimated: boolean;
  };
  /** Subtree counts (directories only). */
  counts: { files: number; dirs: number } | null;
  times: {
    created: DetailTime;
    modified: DetailTime;
    accessed: DetailTime;
    mftChanged: DetailTime;
    /** `$FILE_NAME` created time (forensics, advanced). */
    fileNameCreated: DetailTime | null;
    /** Last-access updates are disabled or system-managed on this volume (SPEC §13). */
    accessUnreliable: boolean;
  };
  /** Raw `strata_core::EntryFlags` bits. */
  flags: number;
  reparse: { kind: ReparseKind; tag: number; target: string | null } | null;
  cloud: { state: CloudState; cloudLogical: number } | null;
  /** All paths of a hardlinked file (`null` when not hardlinked). */
  hardlinks: { paths: string[]; countedAt: string } | null;
  /** Alternate data streams. */
  streams: { name: string; logical: number; allocated: number }[];
  /** Content sniffing result (SPEC §12.4), or `null` when not sniffed. */
  detectedType: { label: string; claimedExtension: string | null; mismatch: boolean } | null;
  /** Classification, or `null` when unclassified. */
  classification: {
    category: number;
    ruleId: string;
    ruleName: string;
    explain: string;
  } | null;
  /** App attribution, or `null` when unattributed. */
  attribution: { app: string; confidence: Confidence; evidence: string[] } | null;
  safety: { tier: Safety; why: string; regenerable: boolean } | null;
  /** Last writer from ETW, or `null` when unknown or tracking is off. */
  lastWriter: { process: string; pid: number; atMs: number } | null;
  /** Directory history (allocated bytes per snapshot), oldest first; `null` when no history. */
  history: { atMs: number; allocated: number }[] | null;
  /** True when the entry's subtree is from a partial scan. */
  partial: boolean;
}

/**
 * Fetches the detail of one entry (`entry_detail`).
 *
 * @param volumeId - Volume id.
 * @param id - Entry id.
 * @returns The detail.
 */
export function fetchEntryDetail(volumeId: string, id: number): Promise<EntryDetail> {
  return call<EntryDetail>("entry_detail", { volumeId, id });
}
