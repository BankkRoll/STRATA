/**
 * Volume list, scan state and elevation for the home screen (SPEC §5, §16.2).
 *
 * `list_volumes` returns {@link VolumeInfo}[]; the backend emits the
 * `volumes://changed` event (payload: the full list) on hot-plug and on every
 * scan state change, so the home screen never polls.
 *
 * NOTE: the platform track owns volume discovery; this type is the UI's
 * proposal until its contract merges (see `docs/tracks/ui.md`).
 */
import { listen } from "@tauri-apps/api/event";
import { call, inTauri } from "./backend";

/** Filesystem as reported by `GetVolumeInformationW` (Dev Drive is ReFS with a flag). */
export type Filesystem = "NTFS" | "ReFS" | "FAT32" | "exFAT" | "FAT" | "network" | "other";

/** `GetDriveTypeW` class. */
export type DriveKind = "fixed" | "removable" | "network" | "cdrom" | "ramdisk" | "unknown";

/** Scan state chip values. */
export type ScanState = "never" | "scanning" | "live" | "stale" | "partial";

/** Progress of a running scan. */
export interface ScanProgress {
  /** Entries processed so far. */
  entries: number;
  /** Bytes accounted so far. */
  bytes: number;
  /** 0–1 when the total is known (MFT scans), else `null`. */
  fraction: number | null;
  /** Estimated seconds remaining, or `null`. */
  etaSecs: number | null;
}

/** One volume. */
export interface VolumeInfo {
  /** Stable id: the volume GUID path (`\\?\Volume{…}\`). */
  id: string;
  /** Drive letters and folder mount points (`C:\`, `D:\Mounts\Data\`); may be empty. */
  mountPoints: string[];
  label: string;
  filesystem: Filesystem;
  /** Dev Drive (ReFS with the Dev Drive flag). */
  devDrive: boolean;
  kind: DriveKind;
  isSystem: boolean;
  totalBytes: number;
  freeBytes: number;
  clusterSize: number;
  serial: string;
  /** BitLocker: locked volumes cannot be scanned. */
  bitlocker: "none" | "unlocked" | "locked";
  /** Volume is present (removed volumes stay listed, greyed, with a stale index). */
  present: boolean;
  scan: {
    state: ScanState;
    progress: ScanProgress | null;
    /** Unix ms of the last completed scan, or `null`. */
    lastScanMs: number | null;
    /** Which scanner produced the index. */
    scanner: "mft" | "walker" | null;
    /** Root entry id once an index exists. */
    rootId: number | null;
    /** Unix ms of the last change applied after the scan (live updates, cleanup), or `null`. */
    changedMs?: number | null;
    /** Why live updates stopped (journal lost or turned off), until the next scan. */
    notice?: string | null;
    /** Changes left to replay while catching up with the change journal. */
    catchingUp?: number | null;
  };
  /** Allocated bytes per category id once scanned, else `null`. */
  categoryBytes: Record<string, number> | null;
}

/** Elevation and helper state. */
export interface HelperStatus {
  /** An elevated helper is connected (fast MFT scans available). */
  elevated: boolean;
  mode: "none" | "on_demand" | "service";
  /** Connection state; `disconnected` after a crash (offer to reconnect). */
  state?: "none" | "connected" | "declined" | "disconnected";
  /** The helper ships with this build; when false, hide "Enable fast scan". */
  available?: boolean;
  /** Last problem, for the banner. */
  message?: string | null;
}

/** "Since last scan" summary (SPEC §18), or `null` when there is no history. */
export interface SinceLastScan {
  deltaBytes: number;
  sinceMs: number;
  biggest: { path: string; deltaBytes: number } | null;
}

/** Lists volumes (`list_volumes`). */
export function fetchVolumes(): Promise<VolumeInfo[]> {
  return call<VolumeInfo[]>("list_volumes");
}

/** Reads elevation state (`helper_status`). */
export function fetchHelperStatus(): Promise<HelperStatus> {
  return call<HelperStatus>("helper_status");
}

/** Reads the since-last-scan summary (`history_since_last_scan`). */
export function fetchSinceLastScan(volumeId: string): Promise<SinceLastScan | null> {
  return call<SinceLastScan | null>("history_since_last_scan", { volumeId });
}

/**
 * Starts a scan (`scan_start`). `fast` asks for the elevated MFT scanner
 * (UAC prompt when no helper runs); `auto` uses it only if already elevated.
 */
export function startScan(volumeId: string, mode: "auto" | "fast" | "standard"): Promise<null> {
  return call<null>("scan_start", { volumeId, mode });
}

/** Cancels a running scan (`scan_cancel`); partial results stay browsable. */
export function cancelScan(volumeId: string): Promise<null> {
  return call<null>("scan_cancel", { volumeId });
}

/** Launches the elevated helper (`helper_elevate`). */
export function elevateHelper(): Promise<HelperStatus> {
  return call<HelperStatus>("helper_elevate");
}

/**
 * Subscribes to `volumes://changed`.
 *
 * @param onChange - Receives the full volume list.
 * @returns Unsubscribe function (no-op outside Tauri).
 */
export function watchVolumes(onChange: (volumes: VolumeInfo[]) => void): () => void {
  if (!inTauri()) return () => undefined;
  const un = listen<VolumeInfo[]>("volumes://changed", (e) => {
    onChange(e.payload);
  });
  return () => {
    void un.then((f) => {
      f();
    });
  };
}

/**
 * Subscribes to `helper://changed` (connect, disconnect, crash).
 *
 * @param onChange - Receives the new status.
 * @returns Unsubscribe function (no-op outside Tauri).
 */
export function watchHelper(onChange: (status: HelperStatus) => void): () => void {
  if (!inTauri()) return () => undefined;
  const un = listen<HelperStatus>("helper://changed", (e) => {
    onChange(e.payload);
  });
  return () => {
    void un.then((f) => {
      f();
    });
  };
}

/** Display name: first mount point, else the label, else the GUID. */
export function volumeName(v: VolumeInfo): string {
  const mount = v.mountPoints[0];
  if (mount) return v.label ? `${v.label} (${mount.replace(/\\$/, "")})` : mount;
  return v.label || v.id;
}
