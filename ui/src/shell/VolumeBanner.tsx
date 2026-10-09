/**
 * A one-line notice above the workspace when the open volume's index is not
 * a complete, current picture: a scan still running, a cancelled (partial)
 * scan, a stale index, or a disconnected drive.
 */
import { useState } from "react";
import { Icon } from "../components/icons";
import { errorMessage } from "../lib/backend";
import { formatCount, formatRelative } from "../lib/format";
import { volumeName } from "../lib/volumes";
import { useServices } from "../services";
import { useApp } from "../store/app";
import { useVolumes } from "../store/volumes";
import { scanPercent } from "./scan";

/** The notice for the open volume, or nothing when its index is complete. */
export function VolumeBanner() {
  const services = useServices();
  const volumeId = useApp((s) => s.volumeId);
  const notify = useApp((s) => s.notify);
  const volume = useVolumes((s) => s.volumes?.find((v) => v.id === volumeId) ?? null);
  const [busy, setBusy] = useState(false);
  if (!volume) return null;
  const name = volumeName(volume);
  const rescan = (
    <button
      type="button"
      className="btn btn--sm"
      disabled={busy}
      onClick={() => {
        setBusy(true);
        services.volumes
          .startScan(volume.id, "auto")
          .catch((err: unknown) => {
            notify(errorMessage(err));
          })
          .finally(() => {
            setBusy(false);
          });
      }}
    >
      Rescan
    </button>
  );
  let text: string;
  let action = rescan;
  let tone = "info";
  if (!volume.present) {
    text = `${name} is disconnected. You are browsing its last index${volume.scan.lastScanMs ? ` from ${formatRelative(volume.scan.lastScanMs)}` : ""}.`;
    action = <></>;
    tone = "warn";
  } else if (volume.scan.state === "scanning") {
    const p = volume.scan.progress;
    text = `Scanning ${name}${p?.fraction != null ? ` (${scanPercent(p.fraction)})` : ""}. Sizes grow as ${p ? `${formatCount(p.entries)} items so far are` : "items are"} counted.`;
    action = <></>;
  } else if (volume.scan.state === "partial") {
    text = `The last scan of ${name} was stopped early, so some folders are missing or smaller than they are.`;
    tone = "warn";
  } else if (volume.scan.state === "stale") {
    text = `Changes on ${name} were missed while Strata wasn’t watching. Sizes may be out of date.`;
    tone = "warn";
  } else {
    return null;
  }
  return (
    <div className={`ws-banner ws-banner--${tone}`} role="status">
      <Icon name={tone === "warn" ? "warning" : "info"} size={14} />
      <span className="ws-banner__text">{text}</span>
      {action}
    </div>
  );
}
