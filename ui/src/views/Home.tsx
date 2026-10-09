/**
 * Home screen: the volumes overview (SPEC §16.2). Every volume shows a
 * capacity bar (category-colored once scanned), filesystem badge, scan state
 * chip and its scan action, plus the elevation banner (SPEC §4) and the
 * "since last scan" banner (SPEC §18).
 */
import { useEffect, useState } from "react";
import { BackendUnavailableError, errorMessage } from "../lib/backend";
import { formatBytes, formatPercent, formatRelative } from "../lib/format";
import { useDark } from "../lib/hooks";
import { CATEGORIES } from "../lib/palette";
import { volumeName, watchHelper, type ScanState, type SinceLastScan, type VolumeInfo } from "../lib/volumes";
import { useServices } from "../services";
import { useApp } from "../store/app";
import { useSettings } from "../store/settings";
import { useVolumes } from "../store/volumes";
import { Icon } from "../components/icons";

const STATE_LABEL: Record<ScanState, string> = {
  never: "Not scanned",
  scanning: "Scanning",
  live: "Live",
  stale: "Stale",
  partial: "Partial",
};

/** Capacity bar: per-category segments when scanned, else used vs free. */
export function CapacityBar({ v }: { v: VolumeInfo }) {
  const dark = useDark();
  const units = useSettings((s) => s.units);
  const used = Math.max(0, v.totalBytes - v.freeBytes);
  const total = Math.max(1, v.totalBytes);
  const segments: { label: string; bytes: number; color: string }[] = [];
  if (v.categoryBytes) {
    let accounted = 0;
    for (const c of CATEGORIES) {
      const b = v.categoryBytes[String(c.id)] ?? 0;
      if (b <= 0) continue;
      accounted += b;
      segments.push({ label: c.label, bytes: b, color: dark ? c.dark : c.light });
    }
    // The gap between used space and what the scan found is shown, never hidden (SPEC §5).
    if (used > accounted) segments.push({ label: "Unaccounted / system reserved", bytes: used - accounted, color: "var(--unaccounted)" });
  } else {
    segments.push({ label: "Used", bytes: used, color: "var(--used)" });
  }
  const text = `${formatBytes(used, { units })} used of ${formatBytes(v.totalBytes, { units })}, ${formatBytes(v.freeBytes, { units })} free`;
  return (
    <div className="capacity">
      <div className="capacity__bar" role="img" aria-label={`${text}. ${segments.map((s) => `${s.label} ${formatBytes(s.bytes, { units })}`).join(", ")}`}>
        {segments.map((s) => (
          <span key={s.label} style={{ width: `${(s.bytes / total) * 100}%`, background: s.color }} title={`${s.label}: ${formatBytes(s.bytes, { units })}`} />
        ))}
      </div>
      <p className="capacity__text">
        {text} <span className="detail__muted">({formatPercent(used / total)} full)</span>
      </p>
    </div>
  );
}

function VolumeCard({ v }: { v: VolumeInfo }) {
  const services = useServices();
  const openVolume = useApp((s) => s.openVolume);
  const notify = useApp((s) => s.notify);
  const [busy, setBusy] = useState(false);
  const state = v.scan.state;
  const canOpen = v.scan.rootId !== null && v.present;
  const locked = v.bitlocker === "locked";
  const run = (p: Promise<unknown>) => {
    setBusy(true);
    p.catch((err: unknown) => {
      notify(errorMessage(err));
    }).finally(() => {
      setBusy(false);
    });
  };
  const progress = v.scan.progress;
  return (
    <li className={v.present ? "volume" : "volume volume--absent"}>
      <div className="volume__head">
        <h2 className="volume__name">{volumeName(v)}</h2>
        <span className="badge">{v.devDrive ? "Dev Drive" : v.filesystem}</span>
        {v.isSystem && <span className="badge">System</span>}
        {v.kind !== "fixed" && <span className="badge">{v.kind}</span>}
        {locked && (
          <span className="badge badge--warn">
            <Icon name="lock" size={12} /> BitLocker locked
          </span>
        )}
        <span className={`chip-state chip-state--${state}`}>{STATE_LABEL[state]}</span>
      </div>
      <CapacityBar v={v} />
      {state === "scanning" && progress && (
        <div className="volume__progress">
          <progress max={1} value={progress.fraction ?? undefined} aria-label={`Scanning ${volumeName(v)}`} />
          <span>
            {progress.entries.toLocaleString()} items
            {progress.etaSecs !== null ? ` · about ${Math.ceil(progress.etaSecs)} s left` : ""}
          </span>
        </div>
      )}
      <div className="volume__actions">
        {state === "scanning" ? (
          <button
            type="button"
            className="btn btn--sm"
            disabled={busy}
            onClick={() => {
              run(services.volumes.cancelScan(v.id));
            }}
          >
            Cancel scan
          </button>
        ) : (
          <button
            type="button"
            className="btn btn--sm"
            disabled={busy || locked || !v.present}
            onClick={() => {
              run(services.volumes.startScan(v.id, "auto"));
            }}
          >
            {state === "never" ? "Scan" : "Rescan"}
          </button>
        )}
        {canOpen && (
          <button
            type="button"
            className="btn btn--sm btn--primary"
            onClick={() => {
              if (v.scan.rootId !== null) openVolume(v.id, v.scan.rootId);
            }}
          >
            Open map
          </button>
        )}
        {v.scan.lastScanMs !== null && <span className="detail__muted">Scanned {formatRelative(v.scan.lastScanMs)}</span>}
      </div>
    </li>
  );
}

function SinceLastScanBanner({ volume }: { volume: VolumeInfo }) {
  const services = useServices();
  const units = useSettings((s) => s.units);
  const [data, setData] = useState<SinceLastScan | null>(null);
  useEffect(() => {
    let cancelled = false;
    services.volumes
      .sinceLastScan(volume.id)
      .then((d) => {
        if (!cancelled) setData(d);
      })
      .catch(() => {
        // History is optional; without it the banner simply stays hidden.
      });
    return () => {
      cancelled = true;
    };
  }, [services, volume.id]);
  if (!data || data.deltaBytes === 0) return null;
  const sign = data.deltaBytes > 0 ? "+" : "";
  return (
    <p className="banner" role="note">
      <strong>
        {sign}
        {formatBytes(data.deltaBytes, { units })}
      </strong>{" "}
      on {volumeName(volume)} since {formatRelative(data.sinceMs)}
      {data.biggest && (
        <>
          {" "}
          — biggest: <code>{data.biggest.path}</code> {data.biggest.deltaBytes > 0 ? "+" : ""}
          {formatBytes(data.biggest.deltaBytes, { units })}
        </>
      )}{" "}
      {volume.scan.rootId !== null && (
        <button
          type="button"
          className="linkish"
          onClick={() => {
            const s = useApp.getState();
            if (volume.scan.rootId !== null && s.volumeId !== volume.id) s.openVolume(volume.id, volume.scan.rootId);
            s.setView("history");
          }}
        >
          See what changed
        </button>
      )}
    </p>
  );
}

/**
 * Loads the volume list and helper state and keeps them current via
 * `volumes://changed`. Mounted once by the shell so breadcrumbs and the
 * status bar have volume names on every view.
 */
export function useVolumeSync(): void {
  const services = useServices();
  useEffect(() => {
    const store = useVolumes.getState();
    let cancelled = false;
    services.volumes
      .list()
      .then((v) => {
        if (!cancelled) store.setVolumes(v);
      })
      .catch((err: unknown) => {
        if (!cancelled) store.setError(errorMessage(err), err instanceof BackendUnavailableError);
      });
    services.volumes
      .helperStatus()
      .then((h) => {
        if (!cancelled) store.setHelper(h);
      })
      .catch(() => {
        if (!cancelled) store.setHelper(null);
      });
    const unwatch = services.volumes.watch((v) => {
      store.setVolumes(v);
    });
    const unwatchHelper = watchHelper((h) => {
      store.setHelper(h);
    });
    return () => {
      cancelled = true;
      unwatch();
      unwatchHelper();
    };
  }, [services]);
}

/** First-run introduction: nothing has been scanned yet. */
function Welcome({ volumes }: { volumes: readonly VolumeInfo[] }) {
  const services = useServices();
  const notify = useApp((s) => s.notify);
  const [busy, setBusy] = useState(false);
  const target = volumes.find((v) => v.isSystem && v.present && v.bitlocker !== "locked") ?? volumes.find((v) => v.present && v.bitlocker !== "locked");
  return (
    <section className="welcome" aria-labelledby="welcome-title">
      <svg className="welcome__mark" viewBox="0 0 64 64" aria-hidden="true">
        <rect x="6" y="10" width="52" height="10" rx="3" />
        <rect x="6" y="27" width="34" height="10" rx="3" />
        <rect x="6" y="44" width="20" height="10" rx="3" />
      </svg>
      <div className="welcome__body">
        <h2 id="welcome-title" className="welcome__title">
          Start with a scan
        </h2>
        <p>
          Strata maps what is using each drive, explains what every item is and which app put it there, and helps you clean up safely. Scanning only reads; nothing on
          disk changes.
        </p>
        {target && (
          <div className="welcome__actions">
            <button
              type="button"
              className="btn btn--primary"
              disabled={busy}
              onClick={() => {
                setBusy(true);
                services.volumes
                  .startScan(target.id, "auto")
                  .catch((err: unknown) => {
                    notify(errorMessage(err));
                  })
                  .finally(() => {
                    setBusy(false);
                  });
              }}
            >
              Scan {volumeName(target)}
            </button>
            <span className="detail__muted">Or pick any drive below.</span>
          </div>
        )}
      </div>
    </section>
  );
}

/** Volumes overview. */
export function Home() {
  const services = useServices();
  const volumes = useVolumes((s) => s.volumes);
  const helper = useVolumes((s) => s.helper);
  const error = useVolumes((s) => s.error);
  const notify = useApp((s) => s.notify);
  const unavailable = useVolumes((s) => s.unavailable);

  const units = useSettings((s) => s.units);
  const scanned = volumes?.find((v) => v.scan.lastScanMs !== null && v.isSystem) ?? volumes?.find((v) => v.scan.lastScanMs !== null);
  const firstRun = volumes !== null && volumes.length > 0 && volumes.every((v) => v.scan.rootId === null && v.scan.state !== "scanning");
  const total = volumes?.reduce((a, v) => a + v.totalBytes, 0) ?? 0;
  const free = volumes?.reduce((a, v) => a + v.freeBytes, 0) ?? 0;

  return (
    <section className="home" aria-labelledby="home-title">
      <div className="home__inner">
        <header className={volumes && volumes.length > 0 ? "home__head" : "visually-hidden"}>
          <h1 id="home-title" className="home__title">
            Volumes
          </h1>
          {volumes && volumes.length > 0 && (
            <p className="home__summary">
              {volumes.length === 1 ? "1 drive" : `${volumes.length} drives`} · {formatBytes(free, { units })} free of {formatBytes(total, { units })}
            </p>
          )}
        </header>
        {firstRun && <Welcome volumes={volumes} />}
        {helper && !helper.elevated && volumes && volumes.length > 0 && (
          <p className="banner banner--info" role="note">
            <Icon name="shield" size={14} />
            Standard scan — some system folders hidden.{" "}
            <button
              type="button"
              className="linkish"
              onClick={() => {
                services.volumes.elevate().then(useVolumes.getState().setHelper, (err: unknown) => {
                  notify(errorMessage(err));
                });
              }}
            >
              Enable fast scan
            </button>
          </p>
        )}
        {scanned && <SinceLastScanBanner volume={scanned} />}
        {volumes === null && !error && (
          <div className="state state--quiet" role="status">
            <span className="spinner" aria-hidden="true" />
            <p>Finding volumes…</p>
          </div>
        )}
        {error && (
          <section className="empty-state">
            <svg className="empty-state__mark" viewBox="0 0 64 64" aria-hidden="true">
              <rect x="6" y="10" width="52" height="10" rx="3" />
              <rect x="6" y="27" width="34" height="10" rx="3" />
              <rect x="6" y="44" width="20" height="10" rx="3" />
            </svg>
            <h2 className="empty-state__title">See everything on your drives</h2>
            <p>
              Strata maps what is using your disk, explains what each item is and which app put it there, and helps you
              clean up safely.
            </p>
            <p className="empty-state__hint" role={unavailable ? undefined : "alert"}>
              {unavailable ? "Drive scanning is not connected in this build yet." : `Couldn’t list volumes: ${error}`}
            </p>
          </section>
        )}
        {volumes && volumes.length === 0 && (
          <div className="state state--quiet">
            <h2>No volumes found</h2>
            <p>Strata lists fixed and removable drives. Network drives can be added in Settings.</p>
          </div>
        )}
        {volumes && volumes.length > 0 && (
          <ul className="volumes">
            {volumes.map((v) => (
              <VolumeCard key={v.id} v={v} />
            ))}
          </ul>
        )}
      </div>
    </section>
  );
}
