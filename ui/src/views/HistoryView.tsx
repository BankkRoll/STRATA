/**
 * History / timeline (SPEC §18): volume usage over time and a "what changed"
 * diff between any two snapshots (grown, shrunk, new and deleted large
 * folders). Folders that still exist link to the detail panel.
 */
import { useState } from "react";
import { TimeChart } from "../components/charts";
import { LoadState, Unavailable, ViewFrame, useCapability, useLoad } from "../components/feature";
import { useFeatures } from "../features";
import { formatDelta } from "../lib/chart";
import { formatBytes, formatDateTime } from "../lib/format";
import { orderPick, type DirChange, type SnapshotDiff, type SnapshotInfo } from "../lib/history";
import { useApp } from "../store/app";
import { useSettings } from "../store/settings";

/** History view. */
export function HistoryView() {
  const features = useFeatures();
  const has = useCapability("history_snapshots", "history_usage");
  const volumeId = useApp((s) => s.volumeId) as string;
  const units = useSettings((s) => s.units);
  const [snaps, reloadSnaps] = useLoad(() => features.history.fetchSnapshots(volumeId), [features, volumeId], has);
  const [usage, reloadUsage] = useLoad(() => features.history.fetchUsage(volumeId, null, null), [features, volumeId], has);
  if (!has) {
    return (
      <ViewFrame title="History">
        <Unavailable feature="History" command="history_snapshots">
          Volume usage over time and what changed between any two scans.
        </Unavailable>
      </ViewFrame>
    );
  }
  return (
    <ViewFrame title="History" lead="Snapshots are taken after each scan and daily while live.">
      <LoadState load={usage} feature="Usage history" command="history_usage" onRetry={reloadUsage}>
        {(points) => (
          <TimeChart
            title="Used space over time"
            units={units}
            yMax={Math.max(0, ...points.map((p) => p.totalBytes))}
            series={[
              { name: "Used", color: "var(--mark-1)", points: points.map((p) => ({ x: p.atMs, y: p.usedBytes })) },
              { name: "Capacity", color: "var(--mark-3)", dash: "5 4", points: points.map((p) => ({ x: p.atMs, y: p.totalBytes })) },
            ]}
          />
        )}
      </LoadState>
      <LoadState load={snaps} feature="Snapshots" command="history_snapshots" onRetry={reloadSnaps}>
        {(list) =>
          list.length < 2 ? (
            <div className="state state--quiet">
              <h2>Not enough history yet</h2>
              <p>“What changed” needs at least two snapshots. One is taken after every scan.</p>
            </div>
          ) : (
            <DiffPicker snapshots={list} />
          )
        }
      </LoadState>
    </ViewFrame>
  );
}

/** Props for {@link DiffPicker}. */
export interface DiffPickerProps {
  snapshots: readonly SnapshotInfo[];
}

/** Two snapshot pickers (default: the last two) and the resulting diff. */
export function DiffPicker({ snapshots }: DiffPickerProps) {
  const features = useFeatures();
  const canDiff = useCapability("history_diff");
  const sizeMode = useApp((s) => s.sizeMode);
  const sorted = [...snapshots].sort((a, b) => a.takenMs - b.takenMs);
  const [a, setA] = useState(sorted[sorted.length - 2]?.id ?? 0);
  const [b, setB] = useState(sorted[sorted.length - 1]?.id ?? 0);
  const pick = orderPick(sorted, a, b);
  const [diff, reload] = useLoad(
    () => (pick ? features.history.fetchDiff(pick[0], pick[1], sizeMode, 25) : Promise.reject(new Error("Pick two different snapshots."))),
    [features, pick?.[0], pick?.[1], sizeMode],
    canDiff && pick !== null,
  );
  const option = (s: SnapshotInfo) => (
    <option key={s.id} value={s.id}>
      {formatDateTime(s.takenMs)}
    </option>
  );
  return (
    <section aria-labelledby="diff-h">
      <h2 id="diff-h" className="section-title">
        What changed
      </h2>
      <div className="toolbar">
        <label>
          From{" "}
          <select
            className="select"
            value={a}
            onChange={(e) => {
              setA(Number(e.target.value));
            }}
          >
            {sorted.map(option)}
          </select>
        </label>
        <label>
          To{" "}
          <select
            className="select"
            value={b}
            onChange={(e) => {
              setB(Number(e.target.value));
            }}
          >
            {sorted.map(option)}
          </select>
        </label>
        {pick === null && <span className="warn-text">Pick two different snapshots.</span>}
      </div>
      {!canDiff ? (
        <Unavailable feature="Snapshot diffs" command="history_diff" />
      ) : (
        pick !== null && (
          <LoadState load={diff} feature="Diff" command="history_diff" onRetry={reload}>
            {(d) => <DiffResult diff={d} />}
          </LoadState>
        )
      )}
    </section>
  );
}

function DiffResult({ diff }: { diff: SnapshotDiff }) {
  const units = useSettings((s) => s.units);
  const sections: [string, DirChange[], string][] = [
    ["Grew", diff.grown, "Nothing grew noticeably."],
    ["Shrank", diff.shrunk, "Nothing shrank noticeably."],
    ["New large folders", diff.newLarge, "No new large folders."],
    ["Deleted large folders", diff.deletedLarge, "No large folders were deleted."],
  ];
  return (
    <>
      <p className="lead-number" role="status">
        Used space <strong>{formatDelta(diff.usedDelta, units)}</strong> from {formatDateTime(diff.from.takenMs)} to {formatDateTime(diff.to.takenMs)}
      </p>
      <div className="diff-grid">
        {sections.map(([title, rows, empty]) => (
          <section key={title} aria-label={title} className="diff">
            <h3>{title}</h3>
            {rows.length === 0 ? (
              <p className="detail__muted">{empty}</p>
            ) : (
              <table className="table">
                <caption className="visually-hidden">{title}</caption>
                <thead>
                  <tr>
                    <th scope="col">Folder</th>
                    <th scope="col" className="num">
                      Change
                    </th>
                    <th scope="col" className="num">
                      Now
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {rows.map((r) => (
                    <tr key={r.path}>
                      <th scope="row" className="cell-path">
                        {r.entryId !== null ? (
                          <button
                            type="button"
                            className="linkish"
                            onClick={() => {
                              if (r.entryId === null) return;
                              useApp.getState().select([r.entryId], r.entryId);
                              useApp.getState().setPane("detail", true);
                            }}
                          >
                            {r.path}
                          </button>
                        ) : (
                          r.path
                        )}
                      </th>
                      <td className={`num ${r.delta > 0 ? "delta-up" : "delta-down"}`}>{formatDelta(r.delta, units)}</td>
                      <td className="num">{r.after ? formatBytes(r.after.allocated, { units }) : "gone"}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </section>
        ))}
      </div>
    </>
  );
}
