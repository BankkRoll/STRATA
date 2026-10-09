/**
 * Duplicates: background scan with progress and cancel, groups
 * sorted by wasted bytes, a keep suggestion with its reason, and guardrailed
 * selection (never every copy). Selected copies go to the cleanup queue, or,
 * behind explicit warnings, are replaced with hardlinks to the kept copy.
 */
import { useEffect, useMemo, useState } from "react";
import { ConfirmDialog, LoadState, SafetyBadge, Unavailable, ViewFrame, useCapability, useLoad } from "../components/feature";
import { useFeatures } from "../features";
import { errorMessage } from "../lib/backend";
import {
  KEEP_REASON_TEXT,
  selectAllButKeep,
  selectionIsSafe,
  toggleDupe,
  type DupeGroup,
  type DupeScanStatus,
  type DupeSelection,
  type HardlinkPrompt,
} from "../lib/dupes";
import { formatBytes, formatCount, formatDate, formatRelative } from "../lib/format";
import { useApp } from "../store/app";
import { reportAdd } from "../store/queue";
import { useSettings } from "../store/settings";
import { useVolumes } from "../store/volumes";

const PAGE = 50;

/** Duplicates view. */
export function DuplicatesView() {
  const features = useFeatures();
  const has = useCapability("dupes_status", "dupes_groups");
  const [status, setStatus] = useState<DupeScanStatus | null>(null);
  const [statusError, setStatusError] = useState<string | null>(null);

  useEffect(() => {
    if (!has) return;
    let cancelled = false;
    features.dupes.fetchDupeStatus().then(
      (s) => {
        if (!cancelled) setStatus(s);
      },
      (e: unknown) => {
        if (!cancelled) setStatusError(errorMessage(e));
      },
    );
    const un = features.dupes.watchDupeStatus(setStatus);
    return () => {
      cancelled = true;
      un();
    };
  }, [features, has]);

  if (!has) {
    return (
      <ViewFrame title="Duplicates">
        <Unavailable feature="The duplicate finder" command="dupes_status">
          Finds identical files by content (never downloading cloud-only files), sorted by wasted space.
        </Unavailable>
      </ViewFrame>
    );
  }
  return (
    <ViewFrame title="Duplicates" lead="Identical files by content. Hardlinks and cloud-only files are never counted.">
      {statusError && (
        <p className="banner banner--warn" role="alert">
          {statusError}
        </p>
      )}
      {status && <ScanPanel status={status} />}
      {status && status.lastRunMs !== null && <Groups key={status.lastRunMs} />}
    </ViewFrame>
  );
}

function ScanPanel({ status }: { status: DupeScanStatus }) {
  const features = useFeatures();
  const units = useSettings((s) => s.units);
  const volumes = useVolumes((s) => s.volumes);
  const [minBytes, setMinBytes] = useState(1 << 20);
  const scanned = (volumes ?? []).filter((v) => v.scan.rootId !== null).map((v) => v.id);
  const running = status.state === "running";
  const p = status.progress;
  return (
    <section className="callout" aria-labelledby="dscan-h">
      <h2 id="dscan-h" className="visually-hidden">
        Scan
      </h2>
      {running && p ? (
        <>
          <p aria-live="polite">
            {status.phase === "grouping" ? "Grouping by size" : status.phase === "partial_hash" ? "Checking file samples" : "Hashing candidates"} · {formatCount(p.filesDone)} of{" "}
            {formatCount(p.filesTotal)} files
            {p.etaSecs !== null && ` · about ${Math.max(1, Math.round(p.etaSecs / 60))} min left`}
          </p>
          <div role="progressbar" aria-label="Duplicate scan" aria-valuemin={0} aria-valuemax={p.bytesTotal} aria-valuenow={p.bytesDone} className="progress">
            <span className="progress__fill" style={{ width: `${p.bytesTotal === 0 ? 0 : (p.bytesDone / p.bytesTotal) * 100}%` }} />
          </div>
          <button
            type="button"
            className="btn"
            onClick={() => {
              void features.dupes.cancelDupeScan().catch((e: unknown) => {
                useApp.getState().notify(errorMessage(e));
              });
            }}
          >
            Cancel (resumes later)
          </button>
        </>
      ) : (
        <div className="toolbar">
          <p>
            {status.lastRunMs === null
              ? "No duplicate scan yet."
              : `${formatCount(status.groups)} groups, ${formatBytes(status.wastedBytes, { units })} wasted · last checked ${formatRelative(status.lastRunMs)}`}
            {status.state === "error" && status.message && <span className="warn-text"> Last run failed: {status.message}</span>}
          </p>
          <label>
            Ignore files under{" "}
            <select
              className="select"
              value={minBytes}
              onChange={(e) => {
                setMinBytes(Number(e.target.value));
              }}
            >
              <option value={1 << 20}>1 MB</option>
              <option value={10 << 20}>10 MB</option>
              <option value={100 << 20}>100 MB</option>
            </select>
          </label>
          <button
            type="button"
            className="btn btn--primary"
            disabled={scanned.length === 0}
            title={scanned.length === 0 ? "Scan a volume first." : undefined}
            onClick={() => {
              void features.dupes.startDupeScan(scanned, minBytes).catch((e: unknown) => {
                useApp.getState().notify(errorMessage(e));
              });
            }}
          >
            {status.lastRunMs === null ? "Find duplicates" : "Check again"}
          </button>
        </div>
      )}
    </section>
  );
}

function Groups() {
  const features = useFeatures();
  const units = useSettings((s) => s.units);
  const canQueue = useCapability("dupes_queue");
  const canLink = useCapability("dupes_hardlink_prompt", "dupes_hardlink");
  const [limit, setLimit] = useState(PAGE);
  const [load, reload] = useLoad(() => features.dupes.fetchDupeGroups(0, limit), [features, limit]);
  const [selected, setSelected] = useState<ReadonlyMap<number, number[]>>(new Map());
  const [refusal, setRefusal] = useState<string | null>(null);
  const [link, setLink] = useState<HardlinkPrompt | null>(null);
  const [linkAck, setLinkAck] = useState(false);
  const groups = useMemo(() => new Map(load.state === "ready" ? load.data.groups.map((g) => [g.id, g]) : []), [load]);
  const selections: DupeSelection[] = [...selected.entries()].filter(([, ids]) => ids.length > 0).map(([groupId, fileIds]) => ({ groupId, fileIds }));
  const count = selections.reduce((a, s) => a + s.fileIds.length, 0);
  const bytes = selections.reduce((a, s) => a + (groups.get(s.groupId)?.size ?? 0) * s.fileIds.length, 0);
  const safe = selectionIsSafe(groups, selections);

  const setFor = (g: DupeGroup, ids: number[]) => {
    const next = new Map(selected);
    next.set(g.id, ids);
    setSelected(next);
  };

  return (
    <LoadState load={load} feature="Duplicate groups" command="dupes_groups" onRetry={reload}>
      {(page) =>
        page.groups.length === 0 ? (
          <div className="state state--quiet">
            <h2>No duplicates found</h2>
          </div>
        ) : (
          <>
            <div className="toolbar sticky-bar">
              <span role="status">
                {formatCount(count)} {count === 1 ? "copy" : "copies"} selected · {formatBytes(bytes, { units })}
              </span>
              <button
                type="button"
                className="btn"
                onClick={() => {
                  setSelected(new Map(page.groups.map((g) => [g.id, selectAllButKeep(g)])));
                }}
              >
                Select all but the suggested keep
              </button>
              <button
                type="button"
                className="btn"
                disabled={count === 0}
                onClick={() => {
                  setSelected(new Map());
                }}
              >
                Clear selection
              </button>
              <button
                type="button"
                className="btn btn--primary"
                disabled={!canQueue || count === 0 || !safe}
                onClick={() => {
                  features.dupes.queueDupes(selections).then(
                    (r) => {
                      reportAdd(r);
                      setSelected(new Map());
                    },
                    (e: unknown) => {
                      useApp.getState().notify(errorMessage(e));
                    },
                  );
                }}
              >
                Add to cleanup queue
              </button>
              <button
                type="button"
                className="btn"
                disabled={!canLink || count === 0 || !safe}
                title={canLink ? "Advanced: keeps one copy and links the others to it" : "Not available in this build."}
                onClick={() => {
                  setLinkAck(false);
                  features.dupes.prepareHardlinks(selections).then(setLink, (e: unknown) => {
                    useApp.getState().notify(errorMessage(e));
                  });
                }}
              >
                Replace with hardlinks…
              </button>
            </div>
            {refusal && (
              <p className="banner banner--warn" role="alert">
                {refusal}
              </p>
            )}
            <p className="detail__muted">
              {formatCount(page.total)} groups, largest waste first. At least one copy in every group always stays.
            </p>
            <ol className="dupes">
              {page.groups.map((g) => (
                <DupeGroupRow
                  key={g.id}
                  group={g}
                  selected={selected.get(g.id) ?? []}
                  onToggle={(fileId) => {
                    const r = toggleDupe(g, selected.get(g.id) ?? [], fileId);
                    setRefusal(r.ok ? null : r.reason);
                    setFor(g, r.selected);
                  }}
                  onAllButKeep={() => {
                    setRefusal(null);
                    setFor(g, selectAllButKeep(g));
                  }}
                />
              ))}
            </ol>
            {page.total > page.groups.length && (
              <button
                type="button"
                className="btn"
                onClick={() => {
                  setLimit((n) => n + PAGE);
                }}
              >
                Show more groups
              </button>
            )}
            {link && (
              <ConfirmDialog
                title="Replace duplicates with hardlinks?"
                confirmLabel="Replace with hardlinks"
                danger
                confirmDisabled={!linkAck}
                onCancel={() => {
                  setLink(null);
                }}
                onConfirm={() => {
                  const p = link;
                  setLink(null);
                  features.dupes.replaceWithHardlinks(p.promptId).then(
                    (r) => {
                      useApp.getState().notify(
                        `${r.replaced} copies replaced, ${formatBytes(r.bytesSaved, { units })} saved.${r.failed.length > 0 ? ` ${r.failed.length} failed: ${r.failed[0]?.message ?? ""}` : ""}`,
                      );
                      setSelected(new Map());
                      reload();
                    },
                    (e: unknown) => {
                      useApp.getState().notify(errorMessage(e));
                    },
                  );
                }}
              >
                <p>{link.message}</p>
                <p>
                  {formatCount(link.files)} copies become links to the kept file, saving {formatBytes(link.bytesSaved, { units })}.
                </p>
                {link.refusedCrossVolume > 0 && <p className="warn-text">{formatCount(link.refusedCrossVolume)} selected copies are on another drive and will be left alone (hardlinks only work within one drive).</p>}
                <label className="ack ack--danger">
                  <input
                    type="checkbox"
                    checked={linkAck}
                    onChange={(e) => {
                      setLinkAck(e.target.checked);
                    }}
                  />
                  I understand that afterwards the copies are one file: editing any of them changes all of them.
                </label>
              </ConfirmDialog>
            )}
          </>
        )
      }
    </LoadState>
  );
}

function DupeGroupRow({ group, selected, onToggle, onAllButKeep }: { group: DupeGroup; selected: number[]; onToggle: (fileId: number) => void; onAllButKeep: () => void }) {
  const units = useSettings((s) => s.units);
  return (
    <li className="dupe">
      <h2 className="dupe__head">
        {formatCount(group.files.length)} copies of {formatBytes(group.size, { units })} · <strong>{formatBytes(group.wastedBytes, { units })} wasted</strong>
        {!group.sameVolume && <span className="badge">across drives</span>}
      </h2>
      <button type="button" className="btn btn--small" onClick={onAllButKeep}>
        Select all but the suggested keep
      </button>
      <ul className="plain dupe__files">
        {group.files.map((f) => {
          const keep = f.fileId === group.keep.fileId;
          const id = `dupe-${group.id}-${f.fileId}`;
          return (
            <li key={f.fileId} className={keep ? "dupe__file dupe__file--keep" : "dupe__file"}>
              <input
                type="checkbox"
                id={id}
                checked={selected.includes(f.fileId)}
                onChange={() => {
                  onToggle(f.fileId);
                }}
              />
              <label htmlFor={id} className="cell-path">
                <span className="visually-hidden">Remove copy </span>
                {f.path}
              </label>
              <span className="detail__muted">{formatDate(f.modifiedMs)}</span>
              {f.safety && f.safety !== "safe" && <SafetyBadge tier={f.safety} />}
              {keep && (
                <span className="badge badge--keep" title={group.keep.explain}>
                  Suggested keep: {KEEP_REASON_TEXT[group.keep.reason]}
                </span>
              )}
            </li>
          );
        })}
      </ul>
    </li>
  );
}
