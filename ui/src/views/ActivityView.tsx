/**
 * Activity (SPEC §11): opt-in ETW tracking of which processes write where.
 * Off by default; shows the opt-in explanation first. When on: top writers
 * now / last hour / today, overhead state, and one-click clearing.
 */
import { useState } from "react";
import { ConfirmDialog, LoadState, Unavailable, ViewFrame, useCapability, useLoad } from "../components/feature";
import { useFeatures } from "../features";
import { errorMessage } from "../lib/backend";
import type { ActivityStatus, ActivityWindow } from "../lib/activity";
import { formatBytes, formatCount, formatRelative } from "../lib/format";
import { useApp } from "../store/app";
import { useSettings } from "../store/settings";

const WINDOWS: readonly [ActivityWindow, string][] = [
  ["now", "Right now"],
  ["hour", "Last hour"],
  ["today", "Today"],
];

/** Activity view. */
export function ActivityView() {
  const features = useFeatures();
  const has = useCapability("activity_status");
  const [load, reload] = useLoad(() => features.activity.fetchActivityStatus(), [features], has);
  if (!has) {
    return (
      <ViewFrame title="Activity">
        <Unavailable feature="Activity tracking" command="activity_status">
          Optional tracking of which programs write to your disk, kept only on this PC.
        </Unavailable>
      </ViewFrame>
    );
  }
  return (
    <ViewFrame title="Activity" lead="Activity tracking (advanced): which programs write to disk. Data never leaves this PC.">
      <LoadState load={load} feature="Activity" command="activity_status" onRetry={reload}>
        {(s) => <ActivityBody status={s} onChanged={reload} />}
      </LoadState>
    </ViewFrame>
  );
}

function ActivityBody({ status, onChanged }: { status: ActivityStatus; onChanged: () => void }) {
  const features = useFeatures();
  const [busy, setBusy] = useState(false);
  const [confirmClear, setConfirmClear] = useState(false);
  const setEnabled = (on: boolean) => {
    setBusy(true);
    features.activity.setActivityEnabled(on).then(
      () => {
        setBusy(false);
        onChanged();
      },
      (e: unknown) => {
        setBusy(false);
        useApp.getState().notify(errorMessage(e));
      },
    );
  };
  if (!status.enabled) {
    return (
      <section className="optin" aria-labelledby="optin-h">
        <h2 id="optin-h">Activity tracking is off</h2>
        <p>
          When on, Strata listens to Windows file events (ETW) through its elevated helper and records, per hour, how much each program wrote, created and deleted
          in each folder. It powers “Who touched this” in the details and helps attribute folders to apps.
        </p>
        <ul className="plain">
          <li>Stays on this PC; never transmitted. Clear it any time.</li>
          <li>Kept for {status.retentionDays} days (change in Settings).</li>
          <li>Throttles itself if it costs more than its CPU cap.</li>
          <li>Needs the elevated helper (one UAC prompt, or service mode).</li>
        </ul>
        <button
          type="button"
          className="btn btn--primary"
          disabled={busy}
          onClick={() => {
            setEnabled(true);
          }}
        >
          Turn on activity tracking
        </button>
      </section>
    );
  }
  return (
    <>
      <div className="toolbar">
        <p role="status">
          {status.running ? "Tracking is on." : status.needsHelper ? "Tracking is on but paused: the elevated helper is not running." : "Tracking is starting…"}
          {status.throttled && <span className="warn-text"> Sampling to stay under the CPU cap.</span>}
          {status.cpuPercent !== null && <span className="detail__muted"> CPU {status.cpuPercent.toFixed(1)}%.</span>}
          {status.sinceMs !== null && <span className="detail__muted"> Data since {formatRelative(status.sinceMs)}.</span>}
        </p>
        <button
          type="button"
          className="btn"
          disabled={busy}
          onClick={() => {
            setEnabled(false);
          }}
        >
          Turn off
        </button>
        <button
          type="button"
          className="btn"
          onClick={() => {
            setConfirmClear(true);
          }}
        >
          Clear activity data
        </button>
      </div>
      <TopWriters />
      {confirmClear && (
        <ConfirmDialog
          title="Clear activity data?"
          confirmLabel="Clear data"
          danger
          onCancel={() => {
            setConfirmClear(false);
          }}
          onConfirm={() => {
            setConfirmClear(false);
            features.activity.clearActivity().then(
              () => {
                useApp.getState().notify("Activity data cleared.");
                onChanged();
              },
              (e: unknown) => {
                useApp.getState().notify(errorMessage(e));
              },
            );
          }}
        >
          <p>This deletes every recorded write, create and delete rollup and all “last writer” records. Your files are not touched.</p>
        </ConfirmDialog>
      )}
    </>
  );
}

function TopWriters() {
  const features = useFeatures();
  const units = useSettings((s) => s.units);
  const [win, setWin] = useState<ActivityWindow>("hour");
  const [load, reload] = useLoad(() => features.activity.fetchTopWriters(win, 25), [features, win]);
  return (
    <section aria-labelledby="writers-h">
      <h2 id="writers-h" className="section-title">
        Top writers
      </h2>
      <fieldset className="segmented-field">
        <legend className="visually-hidden">Time window</legend>
        {WINDOWS.map(([w, label]) => (
          <label key={w}>
            <input
              type="radio"
              name="activity-window"
              checked={win === w}
              onChange={() => {
                setWin(w);
              }}
            />
            {label}
          </label>
        ))}
      </fieldset>
      <LoadState load={load} feature="Top writers" command="activity_top" onRetry={reload}>
        {(rows) =>
          rows.length === 0 ? (
            <p className="detail__muted">No writes recorded in this window.</p>
          ) : (
            <table className="table">
              <caption className="visually-hidden">Top writers, {WINDOWS.find(([w]) => w === win)?.[1]}</caption>
              <thead>
                <tr>
                  <th scope="col">Program</th>
                  <th scope="col" className="num">
                    Written
                  </th>
                  <th scope="col" className="num">
                    Created
                  </th>
                  <th scope="col" className="num">
                    Deleted
                  </th>
                  <th scope="col">Mostly in</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((r) => (
                  <tr key={r.image}>
                    <th scope="row">
                      <div className="cell-name">{r.name}</div>
                      <div className="cell-path">{r.image}</div>
                    </th>
                    <td className="num">{formatBytes(r.bytesWritten, { units })}</td>
                    <td className="num">{formatCount(r.filesCreated)}</td>
                    <td className="num">{formatCount(r.filesDeleted)}</td>
                    <td className="cell-path">{r.topDirs[0]?.path ?? "—"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )
        }
      </LoadState>
    </section>
  );
}
