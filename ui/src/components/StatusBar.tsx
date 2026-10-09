/**
 * Status bar: polite live region for action results, selection summary,
 * active size mode, scan state of the open volume, and the app version.
 */
import type { AppInfo } from "../lib/backend";
import { volumeName } from "../lib/volumes";
import { useApp } from "../store/app";
import { useVolumes } from "../store/volumes";

/** Props for {@link StatusBar}. */
export interface StatusBarProps {
  info: AppInfo | null;
}

/** The bottom status bar. */
export function StatusBar({ info }: StatusBarProps) {
  const status = useApp((s) => s.status);
  const count = useApp((s) => s.selection.length);
  const sizeMode = useApp((s) => s.sizeMode);
  const volumeId = useApp((s) => s.volumeId);
  const excluded = useApp((s) => s.filters.excluded.length);
  const volume = useVolumes((s) => s.volumes?.find((v) => v.id === volumeId) ?? null);
  return (
    <footer className="status-bar" aria-label="App status">
      <span className="status-bar__msg" role="status" aria-live="polite">
        {status}
      </span>
      {count > 0 && <span>{count === 1 ? "1 item selected" : `${count.toLocaleString()} items selected`}</span>}
      {excluded > 0 && <span>{excluded} excluded from view</span>}
      {volume && (
        <span>
          {volumeName(volume)} · {volume.scan.state === "partial" ? "Partial scan" : volume.scan.state === "live" ? "Live" : volume.scan.state}
        </span>
      )}
      <span>{sizeMode === "allocated" ? "Sizes: on disk" : "Sizes: logical"}</span>
      <span>Strata {info?.version ?? ""}</span>
      {info && info.windowsBuild > 0 && <span>Windows build {info.windowsBuild}</span>}
    </footer>
  );
}
