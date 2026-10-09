/**
 * Detail-panel sections backed by the feature services: directory size
 * history (sparkline), recent writers from activity tracking, and the
 * classifier's "why" explanation for the selected path.
 */
import { useState } from "react";
import { Sparkline } from "../components/charts";
import { ExplanationView } from "../components/Explanation";
import { useCapability, useLoad } from "../components/feature";
import { useFeatures } from "../features";
import type { EntryDetail } from "../lib/detail";
import { formatBytes, formatDate } from "../lib/format";
import { useSettings } from "../store/settings";

const HISTORY_POINTS = 30;

/**
 * Size history of a directory: the series embedded in `entry_detail`, or
 * fetched with `history_dir_series` when the detail carries none.
 */
export function DirHistory({ detail }: { detail: EntryDetail }) {
  const features = useFeatures();
  const units = useSettings((s) => s.units);
  const canFetch = useCapability("history_dir_series");
  const embedded = detail.history?.map((p) => ({ atMs: p.atMs, bytes: p.allocated })) ?? null;
  const [load] = useLoad(
    () => features.history.fetchDirSeries(detail.volumeId, detail.id, HISTORY_POINTS),
    [features, detail.volumeId, detail.id],
    embedded === null && canFetch,
  );
  const points = embedded ?? (load.state === "ready" ? load.data.map((p) => ({ atMs: p.atMs, bytes: p.allocated })) : null);
  if (points === null) {
    return <p className="detail__muted">{embedded === null && canFetch && load.state === "loading" ? "Loading history…" : "No snapshots yet"}</p>;
  }
  return (
    <>
      <Sparkline points={points} label={`Size history of ${detail.name}`} units={units} />
      {points.length > 1 && <p className="detail__muted">Since {formatDate(points[0]?.atMs ?? null)}</p>}
    </>
  );
}

/** Programs that wrote under a directory in the last week (`activity_dir_writers`). */
export function DirWriters({ volumeId, id }: { volumeId: string; id: number }) {
  const features = useFeatures();
  const units = useSettings((s) => s.units);
  const has = useCapability("activity_dir_writers");
  const [load] = useLoad(() => features.activity.fetchDirWriters(volumeId, id, 7), [features, volumeId, id], has);
  if (!has || load.state !== "ready" || load.data.length === 0) return null;
  return (
    <>
      <p className="detail__muted">Wrote here in the last 7 days:</p>
      <ul className="paths">
        {load.data.slice(0, 5).map((w) => (
          <li key={w.image}>
            <code>{w.name}</code> — {formatBytes(w.bytesWritten, { units })}
          </li>
        ))}
      </ul>
    </>
  );
}

/** "Why is this classified as X?" toggle (`rules_explain`). */
export function WhyClassified({ path, label }: { path: string; label: string }) {
  const features = useFeatures();
  const has = useCapability("rules_explain");
  const [open, setOpen] = useState(false);
  const [load] = useLoad(() => features.settings.explainPath(path), [features, path], has && open);
  if (!has) return null;
  return (
    <>
      <button
        type="button"
        className="btn btn--small"
        aria-expanded={open}
        onClick={() => {
          setOpen(!open);
        }}
      >
        Why is this classified as {label}?
      </button>
      {open &&
        (load.state === "ready" ? (
          <ExplanationView e={load.data} />
        ) : load.state === "loading" ? (
          <p className="detail__muted">Explaining…</p>
        ) : (
          <p className="detail__muted">{load.message}</p>
        ))}
    </>
  );
}
