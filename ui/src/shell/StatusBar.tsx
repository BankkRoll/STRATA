/**
 * The status bar: scan state with progress and ETA, scan mode (standard or
 * fast, with elevation on click), live updates, the latest message (polite
 * live region), the selection summary and the current volume's free space.
 */
import { useState, useSyncExternalStore } from "react";
import { Icon } from "../components/icons";
import { errorMessage } from "../lib/backend";
import { formatBytes, formatCount, type SizeUnits } from "../lib/format";
import { volumeName, type VolumeInfo } from "../lib/volumes";
import { useServices } from "../services";
import { useApp } from "../store/app";
import { useSettings } from "../store/settings";
import { useVolumes } from "../store/volumes";
import { etaText, scanPercent } from "./scan";

const SUMMARY_CAP = 2000;

/**
 * Total size of the selection, summed as entry infos arrive.
 *
 * @returns Bytes and whether every selected entry is loaded.
 */
function useSelectionBytes(): { bytes: number; complete: boolean } {
  const services = useServices();
  const volumeId = useApp((s) => s.volumeId);
  const selection = useApp((s) => s.selection);
  const sizeMode = useApp((s) => s.sizeMode);
  const provider = volumeId ? services.entryInfo(volumeId) : null;
  const key = useSyncExternalStore(
    (cb) => provider?.subscribe(cb) ?? (() => undefined),
    () => {
      if (!provider || selection.length === 0 || selection.length > SUMMARY_CAP) return "0|0";
      let bytes = 0;
      let complete = 1;
      for (const id of selection) {
        const info = provider.get(id);
        if (!info) complete = 0;
        else bytes += sizeMode === "allocated" ? info.allocated : info.logical;
      }
      return `${bytes}|${complete}`;
    },
  );
  const [bytes, complete] = key.split("|");
  return { bytes: Number(bytes), complete: complete === "1" };
}

/** Scan progress / state of the volume shown, else of any scan running. */
function ScanItem({ volume, units }: { volume: VolumeInfo | null; units: SizeUnits }) {
  const scanning = useVolumes((s) => s.volumes?.find((v) => v.scan.state === "scanning") ?? null);
  const v = volume?.scan.state === "scanning" ? volume : (scanning ?? volume);
  if (!v) return <span className="sb-cell sb-cell--muted">No volume open</span>;
  const name = volumeName(v);
  const p = v.scan.progress;
  if (v.scan.state === "scanning") {
    const parts = [`Scanning ${name}`];
    if (p?.fraction != null) parts.push(scanPercent(p.fraction));
    if (p) parts.push(`${formatCount(p.entries)} items`);
    if (p?.etaSecs != null) parts.push(etaText(p.etaSecs));
    return (
      <span className="sb-cell sb-cell--scan" title={p ? `${formatBytes(p.bytes, { units })} found so far` : undefined}>
        <span className="sb-meter" role="progressbar" aria-label={`Scanning ${name}`} aria-valuemin={0} aria-valuemax={100} aria-valuenow={p?.fraction != null ? Math.round(p.fraction * 100) : undefined}>
          <span className={p?.fraction != null ? "sb-meter__fill" : "sb-meter__fill is-indeterminate"} style={p?.fraction != null ? { width: `${p.fraction * 100}%` } : undefined} />
        </span>
        {parts.join(" · ")}
      </span>
    );
  }
  const label = { never: "Not scanned", live: "Indexed", stale: "Index is stale", partial: "Partial scan" }[v.scan.state];
  return <span className={`sb-cell${v.scan.state === "stale" || v.scan.state === "partial" ? " sb-cell--warn" : ""}`}>{`${name} · ${label}`}</span>;
}

/** Standard vs fast scan; clicking "Standard scan" asks for elevation. */
function HelperItem() {
  const services = useServices();
  const helper = useVolumes((s) => s.helper);
  const notify = useApp((s) => s.notify);
  const [busy, setBusy] = useState(false);
  const [declined, setDeclined] = useState(false);
  if (!helper) return null;
  if (helper.elevated) {
    return (
      <span className="sb-cell" data-tip={helper.mode === "service" ? "The helper service reads the drive directly" : "The elevated helper reads the drive directly"}>
        <Icon name="shield" size={12} />
        Fast scan
      </span>
    );
  }
  return (
    <button
      type="button"
      className={declined ? "sb-cell sb-btn sb-cell--warn" : "sb-cell sb-btn"}
      disabled={busy}
      data-tip={
        declined
          ? "Administrator access wasn’t granted, so some system folders stay hidden. Click to try again."
          : "Some system folders are hidden. Click to enable fast scan (administrator)."
      }
      onClick={() => {
        setBusy(true);
        services.volumes
          .elevate()
          .then(
            (h) => {
              setDeclined(false);
              useVolumes.getState().setHelper(h);
            },
            (err: unknown) => {
              setDeclined(true);
              notify(`Fast scan not enabled: ${errorMessage(err)}`);
            },
          )
          .finally(() => {
            setBusy(false);
          });
      }}
    >
      <Icon name={declined ? "warning" : "shield"} size={12} />
      {declined ? "Standard scan · fast scan declined" : "Standard scan"}
    </button>
  );
}

/** The bottom status bar. */
export function StatusBar() {
  const status = useApp((s) => s.status);
  const count = useApp((s) => s.selection.length);
  const volumeId = useApp((s) => s.volumeId);
  const units = useSettings((s) => s.units);
  const volume = useVolumes((s) => s.volumes?.find((v) => v.id === volumeId) ?? null);
  const sel = useSelectionBytes();
  const live = volume?.scan.state === "live";
  return (
    <footer className="statusbar" aria-label="Status" data-region="statusbar">
      <ScanItem volume={volume} units={units} />
      <HelperItem />
      {volume && (
        <span className={live ? "sb-cell" : "sb-cell sb-cell--muted"} data-tip={live ? "Changes on disk appear as they happen" : "Live updates resume after the next scan"}>
          <span className={live ? "sb-dot is-on" : "sb-dot"} aria-hidden="true" />
          {live ? "Live" : "Not live"}
        </span>
      )}
      <span className="sb-msg" role="status" aria-live="polite">
        {status}
      </span>
      {count > 0 && (
        <span className="sb-cell">
          {count === 1 ? "1 selected" : `${formatCount(count)} selected`}
          {sel.bytes > 0 && ` · ${sel.complete ? "" : "≥ "}${formatBytes(sel.bytes, { units })}`}
        </span>
      )}
      {volume && (
        <span className="sb-cell" data-tip={`${formatBytes(volume.totalBytes - volume.freeBytes, { units })} used of ${formatBytes(volume.totalBytes, { units })}`}>
          <Icon name="drive" size={12} />
          {`${formatBytes(volume.freeBytes, { units })} free`}
        </span>
      )}
    </footer>
  );
}
