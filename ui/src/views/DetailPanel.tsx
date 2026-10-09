/**
 * Detail panel for the selection (SPEC §16.3). Each section has designed
 * empty ("none"), unknown ("not available") and loading states; the whole
 * panel has empty, loading, error, multi-selection and partial-scan states.
 */
import { useEffect, useState, type ReactNode } from "react";
import { errorMessage } from "../lib/backend";
import { ENTRY_ACTIONS } from "../lib/commands";
import type { DetailTime, EntryDetail } from "../lib/detail";
import { describeTimestamp, formatBytes, formatBytesExact, formatCount, formatDate } from "../lib/format";
import { categoryInfo } from "../lib/palette";
import { ATTRIBUTE_LABELS, EntryFlag } from "../lib/rows";
import { useServices } from "../services";
import { useApp } from "../store/app";
import { useSettings } from "../store/settings";
import { Icon } from "../components/icons";

/** Result of the latest fetch, tagged with the entry it belongs to. */
type Load = { id: number; volumeId: string } & ({ state: "error"; message: string } | { state: "ready"; detail: EntryDetail });

const REPARSE_LABEL: Record<string, string> = {
  symlink: "Symbolic link",
  mount_point: "Junction / mount point",
  wof: "Compressed (WOF)",
  cloud: "Cloud placeholder",
  dedup: "Deduplicated",
  app_exec_link: "App execution alias",
  wsl: "WSL special file",
  unknown: "Reparse point (unknown)",
};

const CLOUD_LABEL = { online_only: "Online-only", locally_available: "Locally available", always_keep: "Always keep on this device" } as const;
const SAFETY_LABEL = { safe: "Safe", probably: "Probably safe", careful: "Careful", never: "Never" } as const;
const CONFIDENCE_LABEL = { exact: "Exact", high: "High", heuristic: "Heuristic" } as const;

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="detail__section">
      <h3>{title}</h3>
      {children}
    </section>
  );
}

function Unknown({ text = "Not available yet" }: { text?: string }) {
  return <p className="detail__muted">{text}</p>;
}

function Time({ label, t, note }: { label: string; t: DetailTime; note?: string }) {
  const d = describeTimestamp(t.ms);
  return (
    <>
      <dt>{label}</dt>
      <dd>
        {t.ms === null ? (
          "Unknown"
        ) : (
          <>
            <time dateTime={new Date(t.ms).toISOString()}>{d.absolute}</time>
            <span className="detail__muted"> · {d.relative}</span>
          </>
        )}
        {(t.suspicious || d.suspicious) && (
          <span className="badge badge--warn" title="Before 1990 or in the future">
            Suspicious timestamp
          </span>
        )}
        {note && <span className="detail__muted detail__note">{note}</span>}
      </dd>
    </>
  );
}

/** Tiny inline SVG sparkline of a directory's size history. */
export function Sparkline({ points, label }: { points: { atMs: number; allocated: number }[]; label: string }) {
  if (points.length < 2) return <Unknown text="Not enough history yet" />;
  const w = 220;
  const h = 40;
  const xs = points.map((p) => p.atMs);
  const ys = points.map((p) => p.allocated);
  const x0 = Math.min(...xs);
  const x1 = Math.max(...xs);
  const y0 = Math.min(...ys);
  const y1 = Math.max(...ys);
  const px = (x: number) => (x1 === x0 ? 0 : ((x - x0) / (x1 - x0)) * w);
  const py = (y: number) => (y1 === y0 ? h / 2 : h - ((y - y0) / (y1 - y0)) * (h - 4) - 2);
  const d = points.map((p, i) => `${i === 0 ? "M" : "L"}${px(p.atMs).toFixed(1)},${py(p.allocated).toFixed(1)}`).join(" ");
  return (
    <svg className="sparkline" viewBox={`0 0 ${w} ${h}`} role="img" aria-label={label}>
      <path d={d} fill="none" stroke="currentColor" strokeWidth="1.5" />
    </svg>
  );
}

function DetailBody({ d }: { d: EntryDetail }) {
  const units = useSettings((s) => s.units);
  const services = useServices();
  const caps = services.capabilities();
  const fmt = (n: number) => formatBytes(n, { units });
  const attrs = ATTRIBUTE_LABELS.filter(([bit]) => (d.flags & bit) !== 0);
  const flagNotes = [
    (d.flags & EntryFlag.ACCESS_DENIED) !== 0 && "Access denied: contents unknown",
    (d.flags & EntryFlag.ORPHAN) !== 0 && "Orphaned entry",
    (d.flags & EntryFlag.VIRTUAL) !== 0 && "Virtual node (not on disk)",
    (d.flags & EntryFlag.NTFS_METADATA) !== 0 && "NTFS metadata",
  ].filter((x): x is string => typeof x === "string");

  return (
    <>
      <header className="detail__head">
        {d.iconDataUrl ? <img src={d.iconDataUrl} alt="" width={32} height={32} /> : <Icon name={d.isDir ? "folder" : "file"} size={32} />}
        <div>
          <h2 className="detail__name">{d.name}</h2>
          <p className="detail__path">{d.path}</p>
          <button
            type="button"
            className="btn btn--small"
            onClick={() => {
              void navigator.clipboard.writeText(d.path).then(() => {
                useApp.getState().notify("Path copied.");
              });
            }}
          >
            Copy path
          </button>
        </div>
      </header>
      {d.partial && (
        <p className="banner banner--warn" role="note">
          From a partial scan: totals below may be incomplete.
        </p>
      )}
      <Section title="Size">
        <dl className="kv">
          <dt>On disk</dt>
          <dd title={formatBytesExact(d.sizes.allocated)}>
            {fmt(d.sizes.allocated)}
            {d.sizes.estimated && <span className="badge">estimated</span>}
          </dd>
          <dt>Logical</dt>
          <dd title={formatBytesExact(d.sizes.logical)}>{fmt(d.sizes.logical)}</dd>
          <dt>Streams (ADS)</dt>
          <dd>{d.sizes.adsLogical > 0 ? `${fmt(d.sizes.adsLogical)} (${fmt(d.sizes.adsAllocated)} on disk)` : "None"}</dd>
          {d.isDir && (
            <>
              <dt>Index overhead</dt>
              <dd>{fmt(d.sizes.dirOverhead)}</dd>
            </>
          )}
          <dt>Compression</dt>
          <dd>{d.sizes.compressionRatio === null ? "—" : `${Math.round(d.sizes.compressionRatio * 100)}% of logical size`}</dd>
          {d.counts && (
            <>
              <dt>Contains</dt>
              <dd>
                {formatCount(d.counts.files)} files, {formatCount(d.counts.dirs)} folders
              </dd>
            </>
          )}
        </dl>
      </Section>
      <Section title="Dates">
        <dl className="kv">
          <Time label="Modified" t={d.times.modified} />
          <Time label="Created" t={d.times.created} />
          <Time
            label="Accessed"
            t={d.times.accessed}
            {...(d.times.accessUnreliable ? { note: "Last-access updates are off or system-managed on this volume; treat as approximate." } : {})}
          />
          <Time label="MFT changed" t={d.times.mftChanged} />
          {d.times.fileNameCreated && <Time label="Created ($FILE_NAME)" t={d.times.fileNameCreated} />}
        </dl>
      </Section>
      <Section title="Attributes">
        {attrs.length === 0 && flagNotes.length === 0 ? (
          <Unknown text="None" />
        ) : (
          <ul className="chips">
            {attrs.map(([bit, , label]) => (
              <li key={bit} className="chip">
                {label}
              </li>
            ))}
            {flagNotes.map((n) => (
              <li key={n} className="chip chip--warn">
                {n}
              </li>
            ))}
          </ul>
        )}
        {d.reparse && (
          <p>
            {REPARSE_LABEL[d.reparse.kind] ?? "Reparse point"}
            {d.reparse.target && (
              <>
                {" → "}
                <span className="detail__path">{d.reparse.target}</span>
              </>
            )}
            <span className="detail__muted"> (tag 0x{d.reparse.tag.toString(16).toUpperCase()})</span>
          </p>
        )}
        {d.cloud && (
          <p>
            {CLOUD_LABEL[d.cloud.state]} · cloud size {fmt(d.cloud.cloudLogical)}
          </p>
        )}
      </Section>
      <Section title="Hardlinks">
        {d.hardlinks ? (
          <>
            <p className="detail__muted">Bytes are counted at {d.hardlinks.countedAt}</p>
            <ul className="paths">
              {d.hardlinks.paths.map((p) => (
                <li key={p}>{p}</li>
              ))}
            </ul>
          </>
        ) : (
          <Unknown text="None" />
        )}
      </Section>
      <Section title="Alternate data streams">
        {d.streams.length === 0 ? (
          <Unknown text="None" />
        ) : (
          <ul className="paths">
            {d.streams.map((s) => (
              <li key={s.name}>
                :{s.name} — {fmt(s.logical)}
              </li>
            ))}
          </ul>
        )}
      </Section>
      <Section title="What it is">
        {d.detectedType && (
          <p>
            Detected: {d.detectedType.label}
            {d.detectedType.mismatch && d.detectedType.claimedExtension && (
              <span className="badge badge--warn">claims .{d.detectedType.claimedExtension}</span>
            )}
          </p>
        )}
        {d.classification ? (
          <>
            <p>
              <span className="cat-dot" style={{ background: categoryInfo(d.classification.category).light }} aria-hidden="true" />
              {categoryInfo(d.classification.category).label} · {d.classification.ruleName}
            </p>
            <p className="detail__muted">{d.classification.explain}</p>
            <p className="detail__muted">Rule {d.classification.ruleId}</p>
          </>
        ) : (
          <Unknown text="Not classified by any rule" />
        )}
      </Section>
      <Section title="Owner">
        {d.attribution ? (
          <>
            <p>
              {d.attribution.app} <span className="badge">{CONFIDENCE_LABEL[d.attribution.confidence]} confidence</span>
            </p>
            <ul className="paths">
              {d.attribution.evidence.map((e) => (
                <li key={e}>{e}</li>
              ))}
            </ul>
          </>
        ) : (
          <Unknown text="No app attribution" />
        )}
        {d.lastWriter ? (
          <p>
            Last writer: <code>{d.lastWriter.process}</code> (PID {d.lastWriter.pid}) · {describeTimestamp(d.lastWriter.atMs).relative}
          </p>
        ) : (
          <p className="detail__muted">Last writer unknown (activity tracking off or no events).</p>
        )}
      </Section>
      <Section title="Safety">
        {d.safety ? (
          <>
            <p className={`tier tier--${d.safety.tier}`}>
              <Icon name="shield" /> {SAFETY_LABEL[d.safety.tier]}
              {d.safety.regenerable && <span className="badge">regenerable</span>}
            </p>
            <p className="detail__muted">{d.safety.why}</p>
          </>
        ) : (
          <Unknown text="Not classified: treat as user data" />
        )}
      </Section>
      {d.isDir && (
        <Section title="History">
          {d.history ? (
            <>
              <Sparkline points={d.history} label={`Size history of ${d.name}`} />
              {d.history.length > 1 && (
                <p className="detail__muted">
                  Since {formatDate(d.history[0]?.atMs ?? null)}
                </p>
              )}
            </>
          ) : (
            <Unknown text="No snapshots yet" />
          )}
        </Section>
      )}
      <Section title="Actions">
        <div className="detail__actions">
          {ENTRY_ACTIONS.filter((a) => a.type !== "explain" && a.type !== "copyPath").map((a) => {
            const av = services.bus.availability({ type: a.type, target: { volumeId: d.volumeId, ids: [d.id] } });
            return (
              <button
                key={a.type}
                type="button"
                className="btn btn--small"
                aria-disabled={!av.enabled}
                title={av.enabled ? a.label : av.reason}
                onClick={() => {
                  if (av.enabled) void services.bus.dispatch({ type: a.type, target: { volumeId: d.volumeId, ids: [d.id] } });
                }}
              >
                {a.label}
              </button>
            );
          })}
        </div>
        {caps.size === 0 && <p className="detail__muted">File actions arrive with the shell and cleanup integration.</p>}
      </Section>
    </>
  );
}

/** Props for {@link DetailPanel}. */
export interface DetailPanelProps {
  /** Close button handler (narrow layouts show the panel as a drawer). */
  onClose?: () => void;
}

/** The right-hand detail panel. */
export function DetailPanel({ onClose }: DetailPanelProps) {
  const services = useServices();
  const volumeId = useApp((s) => s.volumeId);
  const primary = useApp((s) => s.primary);
  const count = useApp((s) => s.selection.length);
  const [load, setLoad] = useState<Load | null>(null);

  useEffect(() => {
    if (volumeId === null || primary === null) return;
    let cancelled = false;
    services
      .fetchDetail(volumeId, primary)
      .then((detail) => {
        if (!cancelled) setLoad({ id: primary, volumeId, state: "ready", detail });
      })
      .catch((err: unknown) => {
        if (!cancelled) setLoad({ id: primary, volumeId, state: "error", message: errorMessage(err) });
      });
    return () => {
      cancelled = true;
    };
  }, [services, volumeId, primary]);

  // A result for another entry means the current one is still loading.
  const current = load && load.id === primary && load.volumeId === volumeId ? load : null;
  let body;
  if (volumeId === null || primary === null) {
    body = (
      <div className="state state--quiet">
        <h2>Nothing selected</h2>
        <p>Select a block or a row to see what it is, who made it, and whether it is safe to remove.</p>
      </div>
    );
  } else if (!current) {
    body = (
      <div className="detail__loading" role="status" aria-label="Loading details">
        <span className="skeleton skeleton--title" />
        <span className="skeleton" />
        <span className="skeleton" />
        <span className="skeleton skeleton--short" />
      </div>
    );
  } else if (current.state === "error") {
    body = (
      <div className="state state--error" role="alert">
        <h2>Details unavailable</h2>
        <p>{current.message}</p>
      </div>
    );
  } else {
    body = <DetailBody d={current.detail} />;
  }

  return (
    <aside className="detail" aria-label="Details">
      <div className="detail__bar">
        <span className="detail__title">{count > 1 ? `${count} items selected · showing the last` : "Details"}</span>
        {onClose && (
          <button type="button" className="icon-btn" aria-label="Close details" onClick={onClose}>
            <Icon name="close" />
          </button>
        )}
      </div>
      <div className="detail__scroll">{body}</div>
    </aside>
  );
}
