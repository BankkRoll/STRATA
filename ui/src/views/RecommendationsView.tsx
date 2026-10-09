/**
 * "Free up space": ranked, explainable findings. Each one can
 * be expanded to read why and preview its items (deselecting any), then
 * added to the cleanup queue in one click — or, for findings Windows should
 * handle, opens the matching tool with its command shown first.
 */
import { useState } from "react";
import { LoadState, SafetyBadge, Unavailable, ViewFrame, useCapability, useLoad } from "../components/feature";
import { useFeatures } from "../features";
import { errorMessage } from "../lib/backend";
import { formatBytes, formatCount } from "../lib/format";
import type { Recommendation } from "../lib/insights";
import type { ToolPrompt } from "../lib/tools";
import { useApp } from "../store/app";
import { reportAdd } from "../store/queue";
import { useSettings } from "../store/settings";
import { ToolConfirm } from "./ToolsView";

const PREVIEW = 50;

/** Recommendations view. */
export function RecommendationsView() {
  const features = useFeatures();
  const has = useCapability("recommendations_list");
  const units = useSettings((s) => s.units);
  const [load, reload] = useLoad(() => features.insights.fetchRecommendations(), [features], has);
  if (!has) {
    return (
      <ViewFrame title="Free up space">
        <Unavailable feature="Recommendations" command="recommendations_list">
          Ranked findings like caches you can safely clear, stale node_modules, old installers and duplicates.
        </Unavailable>
      </ViewFrame>
    );
  }
  return (
    <ViewFrame
      title="Free up space"
      lead="Ranked by how much you’d get back. Nothing is removed until you review it in the cleanup queue."
      actions={
        <button type="button" className="btn btn--small" onClick={reload}>
          Refresh
        </button>
      }
    >
      <LoadState load={load} feature="Recommendations" command="recommendations_list" onRetry={reload}>
        {(recs) => {
          if (recs.length === 0) {
            return (
              <div className="state state--quiet">
                <h2>Nothing to suggest</h2>
                <p>Scan a volume to get recommendations.</p>
              </div>
            );
          }
          const total = recs.reduce((a, r) => a + r.bytes, 0);
          return (
            <>
              <p className="lead-number">
                Up to <strong>{formatBytes(total, { units })}</strong> could be freed.
              </p>
              <ol className="recs">
                {recs.map((r) => (
                  <RecommendationCard key={r.id} rec={r} />
                ))}
              </ol>
            </>
          );
        }}
      </LoadState>
    </ViewFrame>
  );
}

function RecommendationCard({ rec }: { rec: Recommendation }) {
  const features = useFeatures();
  const units = useSettings((s) => s.units);
  const canQueue = useCapability("recommendations_queue");
  const canPreview = useCapability("recommendations_preview");
  const canTool = useCapability("tools_prepare", "tools_run");
  const [open, setOpen] = useState(false);
  const [exclude, setExclude] = useState<ReadonlySet<number>>(new Set());
  const [prompt, setPrompt] = useState<ToolPrompt | null>(null);
  const [preview] = useLoad(() => features.insights.previewRecommendation(rec.id, PREVIEW), [features, rec.id], open && canPreview);
  const panel = `rec-${rec.id}`;

  let action;
  switch (rec.action.kind) {
    case "queue": {
      const never = rec.safety === "never";
      action = (
        <button
          type="button"
          className="btn btn--primary"
          disabled={!canQueue || never}
          title={never ? "Protected items can’t be queued." : canQueue ? undefined : "Not available in this build."}
          onClick={() => {
            features.insights.queueRecommendation(rec.id, [...exclude]).then(reportAdd, (e: unknown) => {
              useApp.getState().notify(errorMessage(e));
            });
          }}
        >
          Add to cleanup queue
        </button>
      );
      break;
    }
    case "tool": {
      const tool = rec.action.tool;
      action = (
        <button
          type="button"
          className="btn btn--primary"
          disabled={!canTool}
          onClick={() => {
            features.tools.prepareTool(tool).then(setPrompt, (e: unknown) => {
              useApp.getState().notify(errorMessage(e));
            });
          }}
        >
          Open tool…
        </button>
      );
      break;
    }
    case "view": {
      const view = rec.action.view;
      action = (
        <button
          type="button"
          className="btn btn--primary"
          onClick={() => {
            useApp.getState().setView(view);
          }}
        >
          Review
        </button>
      );
      break;
    }
  }

  return (
    <li className="rec">
      <div className="rec__head">
        <div>
          <h2 className="rec__title">{rec.title}</h2>
          <p className="rec__summary">
            <strong>{formatBytes(rec.bytes, { units })}</strong> · {rec.summary} <SafetyBadge tier={rec.safety} />
          </p>
        </div>
        <div className="rec__actions">
          <button
            type="button"
            className="btn"
            aria-expanded={open}
            aria-controls={panel}
            onClick={() => {
              setOpen(!open);
            }}
          >
            {open ? "Hide details" : "Why & preview"}
          </button>
          {action}
        </div>
      </div>
      <div id={panel} hidden={!open} className="rec__body">
        <p>{rec.explain}</p>
        {open && canPreview && (
          <LoadState load={preview} feature="Preview" command="recommendations_preview">
            {(p) => (
              <>
                <ul className="plain rec__items" aria-label={`Items in ${rec.title}`}>
                  {p.items.map((i) => (
                    <li key={`${i.volumeId}-${i.entryId}`}>
                      {rec.action.kind === "queue" ? (
                        <label>
                          <input
                            type="checkbox"
                            checked={!exclude.has(i.entryId)}
                            disabled={i.safety === "never"}
                            onChange={(e) => {
                              const next = new Set(exclude);
                              if (e.target.checked) next.delete(i.entryId);
                              else next.add(i.entryId);
                              setExclude(next);
                            }}
                          />
                          <span className="cell-path">{i.path}</span>
                        </label>
                      ) : (
                        <span className="cell-path">{i.path}</span>
                      )}{" "}
                      — {formatBytes(i.bytes, { units })} <SafetyBadge tier={i.safety} />
                    </li>
                  ))}
                </ul>
                {p.total > p.items.length && (
                  <p className="detail__muted">
                    and {formatCount(p.total - p.items.length)} more (all are listed again on the review screen)
                  </p>
                )}
              </>
            )}
          </LoadState>
        )}
      </div>
      {prompt && (
        <ToolConfirm
          prompt={prompt}
          onCancel={() => {
            setPrompt(null);
          }}
          onRun={(p) => {
            setPrompt(null);
            features.tools.runTool(p.promptId, () => undefined).then(
              () => {
                useApp.getState().notify(`${p.title} started.`);
              },
              (e: unknown) => {
                useApp.getState().notify(errorMessage(e));
              },
            );
          }}
        />
      )}
    </li>
  );
}
