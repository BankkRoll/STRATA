/**
 * Dependency injection for everything that talks to the backend.
 *
 * Components get their data sources from {@link ServicesContext}, so the app
 * uses the Tauri implementations, component tests use fakes, and the
 * dev-only fixture harness uses exported layout fixtures, all without the
 * components knowing which.
 */
import { createContext, useContext } from "react";
import { getCapabilities } from "./lib/backend";
import { CommandBus, type CommandBusDeps } from "./lib/commands";
import { fetchEntryDetail, type EntryDetail } from "./lib/detail";
import { BatchedEntryInfoProvider, tauriEntryInfoFetcher, type EntryInfoProvider } from "./lib/entries";
import type { ViewKind } from "./lib/layout/frame";
import { TauriLayoutStream, type LayoutStream } from "./lib/layout/stream";
import { fetchAppsBrief, tauriRowSource, type RowSource } from "./lib/rows";
import { TauriSearchStream, type SearchStream } from "./lib/search";
import {
  cancelScan,
  elevateHelper,
  fetchHelperStatus,
  fetchSinceLastScan,
  fetchVolumes,
  startScan,
  watchVolumes,
  type HelperStatus,
  type SinceLastScan,
  type VolumeInfo,
} from "./lib/volumes";
import { createRenderer } from "./render/registry";
import type { ViewRenderer } from "./render/renderer";
import { useApp } from "./store/app";

/** Volume operations used by the home screen. */
export interface VolumeService {
  list(): Promise<VolumeInfo[]>;
  watch(onChange: (volumes: VolumeInfo[]) => void): () => void;
  helperStatus(): Promise<HelperStatus>;
  sinceLastScan(volumeId: string): Promise<SinceLastScan | null>;
  startScan(volumeId: string, mode: "auto" | "fast" | "standard"): Promise<unknown>;
  cancelScan(volumeId: string): Promise<unknown>;
  elevate(): Promise<HelperStatus>;
}

/** Everything components need from the outside world. */
export interface Services {
  /** Opens a layout stream for one canvas. */
  createLayoutStream(): LayoutStream;
  /** Entry metadata for a volume (one cached provider per volume). */
  entryInfo(volumeId: string): EntryInfoProvider;
  /** Paged rows for the list pane. */
  rows: RowSource;
  /** Detail panel fetch. */
  fetchDetail(volumeId: string, id: number): Promise<EntryDetail>;
  volumes: VolumeService;
  /** Streaming filename search. */
  createSearchStream(): SearchStream;
  /** App catalog names by id (empty until loaded). */
  appName(id: number): string | null;
  /** Backend command names this build implements. */
  capabilities(): ReadonlySet<string>;
  /**
   * Why commands missing from {@link capabilities} are unavailable, when the
   * reason is not "this build lacks them" (the website demo runs without
   * the engine). Unset in the app.
   */
  unavailableReason?: string;
  /** Entry action dispatch. */
  bus: CommandBus;
  /** GPU renderer factory (`null` when WebGL2 is unavailable). */
  createRenderer(canvas: HTMLCanvasElement, kind: ViewKind): ViewRenderer | null;
}

/** Context carrying the active {@link Services}. */
export const ServicesContext = createContext<Services | null>(null);

/**
 * Reads the active services.
 *
 * @returns Services from the nearest provider.
 * @throws When rendered outside a provider (a programming error).
 */
export function useServices(): Services {
  const s = useContext(ServicesContext);
  if (!s) throw new Error("ServicesContext missing: wrap the tree in <ServicesContext value={...}>");
  return s;
}

/**
 * Builds a {@link CommandBus} whose UI effects drive the shared store.
 *
 * @param capabilities - Capability lookup.
 * @param writeClipboard - Clipboard writer.
 * @returns The bus.
 */
export function createStoreBus(
  capabilities: () => ReadonlySet<string>,
  writeClipboard: (text: string) => Promise<void> = (t) => navigator.clipboard.writeText(t),
): CommandBus {
  return new CommandBus(storeBusDeps(capabilities, writeClipboard));
}

/**
 * The dependencies {@link createStoreBus} wires, for buses that extend
 * {@link CommandBus}.
 *
 * @param capabilities - Capability lookup.
 * @param writeClipboard - Clipboard writer.
 * @returns Bus dependencies driving the shared store.
 */
export function storeBusDeps(
  capabilities: () => ReadonlySet<string>,
  writeClipboard: (text: string) => Promise<void> = (t) => navigator.clipboard.writeText(t),
): CommandBusDeps {
  return {
    capabilities,
    writeClipboard,
    ui: {
      showInList(target) {
        const s = useApp.getState();
        s.setPane("list", true);
        s.select(target.ids, target.ids[0] ?? null);
      },
      explain(target) {
        const s = useApp.getState();
        s.setPane("detail", true);
        s.select(target.ids, target.ids[0] ?? null);
      },
      exclude(target) {
        useApp.getState().exclude(target.ids);
        useApp.getState().notify(`${target.ids.length === 1 ? "1 item" : `${target.ids.length} items`} hidden from view.`);
      },
      notify(message) {
        useApp.getState().notify(message);
      },
    },
  };
}

/**
 * The production services backed by Tauri commands and channels.
 *
 * @returns Services plus a promise that resolves once capabilities and the
 *   app catalog are loaded.
 */
export function createTauriServices(): { services: Services; ready: Promise<void> } {
  let caps: ReadonlySet<string> = new Set();
  const apps = new Map<number, string>();
  const providers = new Map<string, EntryInfoProvider>();
  const ready = getCapabilities()
    .then(async (c) => {
      caps = c;
      if (c.has("apps_brief")) for (const a of await fetchAppsBrief()) apps.set(a.id, a.name);
    })
    .catch((err: unknown) => {
      console.error("capability discovery failed", err);
    });
  const services: Services = {
    createLayoutStream: () => new TauriLayoutStream(),
    entryInfo(volumeId) {
      let p = providers.get(volumeId);
      if (!p) {
        p = new BatchedEntryInfoProvider(volumeId, tauriEntryInfoFetcher);
        providers.set(volumeId, p);
      }
      return p;
    },
    rows: tauriRowSource,
    fetchDetail: fetchEntryDetail,
    volumes: {
      list: fetchVolumes,
      watch: watchVolumes,
      helperStatus: fetchHelperStatus,
      sinceLastScan: fetchSinceLastScan,
      startScan,
      cancelScan,
      elevate: elevateHelper,
    },
    createSearchStream: () => new TauriSearchStream(),
    appName: (id) => apps.get(id) ?? null,
    capabilities: () => caps,
    bus: createStoreBus(() => caps),
    createRenderer,
  };
  return { services, ready };
}
