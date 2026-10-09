/**
 * Cleanup queue and flow: queue with tier totals → review
 * (grouped, expandable, deselect, Careful acknowledgements, method choice
 * with Recycle Bin availability and large-delete confirmation) → pre-flight
 * (moved / changed / locked with polite close or skip) → execute with
 * progress → per-item results with retry. A second tab shows the undo
 * history with Restore.
 *
 * Every confirmation is collected here but enforced by the backend again
 * (`flow::gate`); the UI never decides that something may be deleted.
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { ConfirmDialog, LoadState, SafetyBadge, Unavailable, ViewFrame, safetyLabel, useCapability, useLoad } from "../components/feature";
import { useFeatures } from "../features";
import { errorMessage } from "../lib/backend";
import {
  RECYCLE_UNAVAILABLE_TEXT,
  reviewBlockers,
  tierTotals,
  type CleanupPlan,
  type ClosePrompt,
  type Decision,
  type ExecutionReport,
  type ItemVerdict,
  type LockHolder,
  type PlanWarning,
  type QueueEntry,
  type RecycleFit,
  type UndoAction,
} from "../lib/cleanup";
import { formatBytes, formatCount, formatDateTime, formatRelative } from "../lib/format";
import type { Safety } from "../lib/types";
import { useApp } from "../store/app";
import { useQueue } from "../store/queue";
import { useSettings } from "../store/settings";

const TIERS: readonly Safety[] = ["safe", "probably", "careful", "never"];

type Stage =
  | { kind: "queue" }
  | { kind: "review"; plan: CleanupPlan }
  | { kind: "preflight"; plan: CleanupPlan; verdicts: ItemVerdict[] | null }
  | { kind: "running"; plan: CleanupPlan; done: number; total: number; bytes: number }
  | { kind: "results"; plan: CleanupPlan; report: ExecutionReport };

function emptyDecision(plan: CleanupPlan): Decision {
  return {
    method: plan.defaultMethod,
    acks: { careful: [], permanent: false, largePermanent: false, permanentInsteadOfRecycle: [] },
    // Never-tier items are never acted on; skipping them keeps the payload honest.
    skip: plan.items.filter((i) => i.safety === "never").map((i) => i.id),
  };
}

// -----------------------------------------------------------------------------
// View
// -----------------------------------------------------------------------------

/** The cleanup view with Queue and Undo history tabs. */
export function CleanupView() {
  const [tab, setTab] = useState<"queue" | "history">("queue");
  const tabs = [
    ["queue", "Queue"],
    ["history", "Undo history"],
  ] as const;
  return (
    <ViewFrame title="Cleanup" lead="Review what will be removed, choose how, and undo it later from the Recycle Bin.">
      <div role="tablist" aria-label="Cleanup sections" className="tabs">
        {tabs.map(([id, label]) => (
          <button
            key={id}
            type="button"
            role="tab"
            id={`tab-${id}`}
            aria-selected={tab === id}
            aria-controls={`panel-${id}`}
            tabIndex={tab === id ? 0 : -1}
            className="tabs__tab"
            onClick={() => {
              setTab(id);
            }}
            onKeyDown={(e) => {
              if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
                e.preventDefault();
                const next = tab === "queue" ? "history" : "queue";
                setTab(next);
                document.getElementById(`tab-${next}`)?.focus();
              }
            }}
          >
            {label}
          </button>
        ))}
      </div>
      <div role="tabpanel" id={`panel-${tab}`} aria-labelledby={`tab-${tab}`}>
        {tab === "queue" ? <CleanupFlow /> : <UndoHistory />}
      </div>
    </ViewFrame>
  );
}

function CleanupFlow() {
  const features = useFeatures();
  const canPlan = useCapability("cleanup_plan", "cleanup_preflight", "cleanup_execute");
  const [stage, setStage] = useState<Stage>({ kind: "queue" });
  const [decision, setDecision] = useState<Decision | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const startReview = (ids: number[]) => {
    setBusy(true);
    setError(null);
    features.cleanup.planCleanup(ids).then(
      (plan) => {
        setDecision(emptyDecision(plan));
        setStage({ kind: "review", plan });
        setBusy(false);
      },
      (err: unknown) => {
        setError(errorMessage(err));
        setBusy(false);
      },
    );
  };

  const runPreflight = (plan: CleanupPlan, d: Decision) => {
    setStage({ kind: "preflight", plan, verdicts: null });
    setError(null);
    features.cleanup.preflightCleanup(plan.planId, d).then(
      (verdicts) => {
        setStage({ kind: "preflight", plan, verdicts });
      },
      (err: unknown) => {
        setError(errorMessage(err));
        setStage({ kind: "review", plan });
      },
    );
  };

  const execute = (plan: CleanupPlan, d: Decision) => {
    const total = plan.items.filter((i) => !d.skip.includes(i.id)).length;
    setStage({ kind: "running", plan, done: 0, total, bytes: 0 });
    features.cleanup
      .executeCleanup(plan.planId, d, (p) => {
        if (p.event === "item_finished") {
          const bytes = p.removed ? (plan.items.find((i) => i.id === p.id)?.bytes ?? 0) : 0;
          setStage((s) => (s.kind === "running" ? { ...s, done: s.done + 1, bytes: s.bytes + bytes } : s));
        }
      })
      .then(
        (report) => {
          setStage({ kind: "results", plan, report });
          const s = report.summary;
          useApp.getState().notify(`Cleanup finished: ${s.succeeded} removed, ${s.failed} failed, ${s.skipped} skipped.`);
        },
        (err: unknown) => {
          setError(errorMessage(err));
          setStage({ kind: "preflight", plan, verdicts: null });
        },
      );
  };

  return (
    <>
      {error && (
        <p className="banner banner--warn" role="alert">
          {error}
        </p>
      )}
      {stage.kind === "queue" && <QueuePanel canReview={canPlan} busy={busy} onReview={startReview} />}
      {stage.kind === "review" && decision && (
        <ReviewPanel
          plan={stage.plan}
          decision={decision}
          onChange={setDecision}
          onBack={() => {
            setStage({ kind: "queue" });
          }}
          onContinue={() => {
            runPreflight(stage.plan, decision);
          }}
        />
      )}
      {stage.kind === "preflight" && decision && (
        <PreflightPanel
          plan={stage.plan}
          verdicts={stage.verdicts}
          decision={decision}
          onChange={setDecision}
          onRecheck={(d) => {
            runPreflight(stage.plan, d);
          }}
          onBack={() => {
            setStage({ kind: "review", plan: stage.plan });
          }}
          onExecute={() => {
            execute(stage.plan, decision);
          }}
        />
      )}
      {stage.kind === "running" && <RunningPanel stage={stage} />}
      {stage.kind === "results" && (
        <ResultsPanel
          plan={stage.plan}
          report={stage.report}
          onRetry={() => {
            setBusy(true);
            features.cleanup.retryPlan(stage.plan.planId).then(
              (plan) => {
                setBusy(false);
                setDecision(emptyDecision(plan));
                setStage({ kind: "review", plan });
              },
              (err: unknown) => {
                setBusy(false);
                setError(errorMessage(err));
              },
            );
          }}
          onDone={() => {
            setStage({ kind: "queue" });
          }}
        />
      )}
    </>
  );
}

// -----------------------------------------------------------------------------
// Queue
// -----------------------------------------------------------------------------

function TierTotals({ items, skip }: { items: readonly QueueEntry[]; skip?: ReadonlySet<number> }) {
  const units = useSettings((s) => s.units);
  return (
    <dl className="tier-totals">
      {tierTotals(items, skip).map((t) => (
        <div key={t.safety} className={`tier-totals__cell tier-totals__cell--${t.safety}`}>
          <dt>
            <SafetyBadge tier={t.safety} />
          </dt>
          <dd>
            <strong>{formatBytes(t.bytes, { units })}</strong> · {formatCount(t.items)} {t.items === 1 ? "item" : "items"}
          </dd>
        </div>
      ))}
    </dl>
  );
}

function QueuePanel({ canReview, busy, onReview }: { canReview: boolean; busy: boolean; onReview: (ids: number[]) => void }) {
  const features = useFeatures();
  const items = useQueue((s) => s.items);
  const refused = useQueue((s) => s.refused);
  const units = useSettings((s) => s.units);
  if (items === null) {
    return (
      <Unavailable feature="The cleanup queue" command="cleanup_queue_list">
        Items you add from the map, the list, recommendations or duplicates collect here for review.
      </Unavailable>
    );
  }
  const total = items.reduce((a, i) => a + i.bytes, 0);
  return (
    <>
      <TierTotals items={items} />
      {refused.length > 0 && (
        <section className="callout callout--warn" aria-labelledby="refused-h">
          <h2 id="refused-h">Not added</h2>
          <ul className="plain">
            {refused.map((r) => (
              <li key={`${r.entryId}-${r.path}`}>
                <code>{r.path}</code> — {r.message}
              </li>
            ))}
          </ul>
        </section>
      )}
      {items.length === 0 ? (
        <div className="state state--quiet">
          <h2>The queue is empty</h2>
          <p>Add items from the map or list (Delete key or “Add to cleanup”), from Free up space, or from Duplicates.</p>
        </div>
      ) : (
        <>
          <table className="table">
            <caption className="visually-hidden">Queued items</caption>
            <thead>
              <tr>
                <th scope="col">Item</th>
                <th scope="col">Safety</th>
                <th scope="col" className="num">
                  Size
                </th>
                <th scope="col">
                  <span className="visually-hidden">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {items.map((i) => (
                <tr key={i.id}>
                  <td>
                    <div className="cell-name">{i.name}</div>
                    <div className="cell-path">{i.path}</div>
                  </td>
                  <td>
                    <SafetyBadge tier={i.safety} />
                  </td>
                  <td className="num">{formatBytes(i.bytes, { units })}</td>
                  <td>
                    <button
                      type="button"
                      className="btn btn--small"
                      aria-label={`Remove ${i.name} from the queue`}
                      onClick={() => {
                        features.cleanup.removeFromQueue([i.id]).then(
                          () => {
                            useQueue.getState().setItems((useQueue.getState().items ?? []).filter((x) => x.id !== i.id));
                          },
                          (err: unknown) => {
                            useApp.getState().notify(errorMessage(err));
                          },
                        );
                      }}
                    >
                      Remove
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          <div className="toolbar">
            <button
              type="button"
              className="btn btn--primary"
              disabled={!canReview || busy}
              title={canReview ? undefined : "Review needs cleanup_plan, which this build doesn’t include."}
              onClick={() => {
                onReview(items.map((i) => i.id));
              }}
            >
              Review {formatCount(items.length)} {items.length === 1 ? "item" : "items"} ({formatBytes(total, { units })})
            </button>
            <button
              type="button"
              className="btn"
              onClick={() => {
                features.cleanup.clearQueue().then(
                  () => {
                    useQueue.getState().setItems([]);
                  },
                  (err: unknown) => {
                    useApp.getState().notify(errorMessage(err));
                  },
                );
              }}
            >
              Clear queue
            </button>
            {!canReview && <span className="detail__muted">Reviewing and deleting aren’t available in this build.</span>}
          </div>
        </>
      )}
    </>
  );
}

// -----------------------------------------------------------------------------
// Review
// -----------------------------------------------------------------------------

function fitText(fit: RecycleFit, units: "binary" | "si"): string {
  switch (fit.fit) {
    case "too_large":
      return `Too large for the Recycle Bin (holds ${formatBytes(fit.capacity, { units })}).`;
    case "unavailable":
      return RECYCLE_UNAVAILABLE_TEXT[fit.reason];
    default:
      return "";
  }
}

function warningsFor(plan: CleanupPlan, id: number): PlanWarning[] {
  return plan.warnings.filter((w) => w.id === id);
}

/** Props for {@link ReviewPanel}. */
export interface ReviewPanelProps {
  plan: CleanupPlan;
  decision: Decision;
  onChange: (d: Decision) => void;
  onBack: () => void;
  onContinue: () => void;
}

/** Review screen: grouped by tier, expandable, deselect, acknowledgements and method. */
export function ReviewPanel({ plan, decision, onChange, onBack, onContinue }: ReviewPanelProps) {
  const units = useSettings((s) => s.units);
  // Careful and never groups start open, as does any group holding an item
  // that needs a decision, so nothing the user must act on is hidden.
  const [open, setOpen] = useState<ReadonlySet<Safety>>(() => {
    const needs = new Set(plan.warnings.filter((w) => w.kind === "cannot_recycle" || w.kind === "running_app").map((w) => w.id));
    return new Set<Safety>(["careful", "never", ...plan.items.filter((i) => needs.has(i.id)).map((i) => i.safety)]);
  });
  const skip = useMemo(() => new Set(decision.skip), [decision.skip]);
  const blockers = reviewBlockers(plan, decision);
  const removed = plan.warnings.filter((w) => w.kind === "refused" || w.kind === "duplicate" || w.kind === "nested");
  const permanentLarge = plan.items.some((i) => !skip.has(i.id) && i.safety !== "never" && i.bytes > plan.largeDeleteBytes);
  const instead = new Set(decision.acks.permanentInsteadOfRecycle);
  const needsPermanentAck = decision.method === "permanent" || instead.size > 0;

  const toggle = (list: number[], id: number, on: boolean) => (on ? [...new Set([...list, id])] : list.filter((x) => x !== id));

  return (
    <div className="review">
      <h2 className="section-title">Review</h2>
      <TierTotals items={plan.items} skip={skip} />
      <section aria-labelledby="vol-h" className="review__volumes">
        <h3 id="vol-h">Recycle Bin by drive</h3>
        <ul className="plain">
          {plan.volumes.map((v) => (
            <li key={v.mountPoint} className={v.recycleBin.state === "unavailable" ? "warn-text" : undefined}>
              <strong>{v.mountPoint}</strong> — {formatCount(v.items)} items, {formatBytes(v.bytes, { units })}:{" "}
              {v.recycleBin.state === "available"
                ? v.recycleBin.capacity === null
                  ? "Recycle Bin available."
                  : `Recycle Bin holds up to ${formatBytes(v.recycleBin.capacity, { units })}.`
                : `No Recycle Bin. ${RECYCLE_UNAVAILABLE_TEXT[v.recycleBin.reason]} Items here can only be deleted permanently or skipped.`}
            </li>
          ))}
        </ul>
      </section>
      {TIERS.map((tier) => {
        const of = plan.items.filter((i) => i.safety === tier);
        if (of.length === 0) return null;
        const expanded = open.has(tier);
        const groupId = `grp-${tier}`;
        return (
          <section key={tier} className={`group group--${tier}`} aria-label={`${safetyLabel(tier)} items`}>
            <h3 className="group__head">
              <button
                type="button"
                className="group__toggle"
                aria-expanded={expanded}
                aria-controls={groupId}
                onClick={() => {
                  const next = new Set(open);
                  if (expanded) next.delete(tier);
                  else next.add(tier);
                  setOpen(next);
                }}
              >
                <span className={`twisty${expanded ? " twisty--open" : ""}`} aria-hidden="true">
                  ▸
                </span>
                <SafetyBadge tier={tier} /> {formatCount(of.length)} {of.length === 1 ? "item" : "items"} ·{" "}
                {formatBytes(
                  of.reduce((a, i) => a + i.bytes, 0),
                  { units },
                )}
              </button>
            </h3>
            {tier === "never" && <p className="detail__muted">Never-tier items are protected and will not be deleted. Strata only shows why.</p>}
            <ul id={groupId} className="review__items" hidden={!expanded}>
              {of.map((item) => {
                const ws = warningsFor(plan, item.id);
                const cannot = ws.find((w): w is Extract<PlanWarning, { kind: "cannot_recycle" }> => w.kind === "cannot_recycle");
                const running = ws.filter((w): w is Extract<PlanWarning, { kind: "running_app" }> => w.kind === "running_app");
                const included = !skip.has(item.id) && tier !== "never";
                return (
                  <li key={item.id} className={`review__item${included ? "" : " review__item--off"}`}>
                    <div className="review__row">
                      {tier === "never" ? (
                        <span className="review__locked" aria-hidden="true">
                          ⛔
                        </span>
                      ) : (
                        <input
                          type="checkbox"
                          id={`inc-${item.id}`}
                          checked={included}
                          onChange={(e) => {
                            onChange({ ...decision, skip: toggle(decision.skip, item.id, !e.target.checked) });
                          }}
                        />
                      )}
                      <label htmlFor={`inc-${item.id}`} className="review__name">
                        {tier !== "never" && <span className="visually-hidden">Include </span>}
                        {item.name}
                      </label>
                      <span className="review__size">{formatBytes(item.bytes, { units })}</span>
                    </div>
                    <div className="cell-path">{item.path}</div>
                    <p className="review__why">
                      {item.ruleName && <strong>{item.ruleName}: </strong>}
                      {item.explain}
                      {item.regenerable && <span className="badge">regenerable</span>}
                    </p>
                    {tier === "careful" && included && (
                      <label className="ack">
                        <input
                          type="checkbox"
                          checked={decision.acks.careful.includes(item.id)}
                          onChange={(e) => {
                            onChange({ ...decision, acks: { ...decision.acks, careful: toggle(decision.acks.careful, item.id, e.target.checked) } });
                          }}
                        />
                        I reviewed this item and want to remove it (it may hold user data or large re-downloads).
                      </label>
                    )}
                    {running.map((w) => (
                      <p key={w.warning.app} className="warn-text">
                        {w.warning.app} is running ({w.warning.reason === "owns_cache" ? "it owns this cache" : "it holds files here"}). Close it first for a clean result.
                      </p>
                    ))}
                    {cannot && included && decision.method === "recycle_bin" && (
                      <fieldset className="choice">
                        <legend className="warn-text">{fitText(cannot.fit, units)} Strata never decides this for you:</legend>
                        <label>
                          <input
                            type="radio"
                            name={`fit-${item.id}`}
                            checked={instead.has(item.id)}
                            onChange={() => {
                              onChange({ ...decision, acks: { ...decision.acks, permanentInsteadOfRecycle: toggle(decision.acks.permanentInsteadOfRecycle, item.id, true) } });
                            }}
                          />
                          Delete permanently
                        </label>
                        <label>
                          <input
                            type="radio"
                            name={`fit-${item.id}`}
                            checked={false}
                            onChange={() => {
                              onChange({
                                ...decision,
                                skip: toggle(decision.skip, item.id, true),
                                acks: { ...decision.acks, permanentInsteadOfRecycle: toggle(decision.acks.permanentInsteadOfRecycle, item.id, false) },
                              });
                            }}
                          />
                          Skip
                        </label>
                      </fieldset>
                    )}
                  </li>
                );
              })}
            </ul>
          </section>
        );
      })}
      {removed.length > 0 && (
        <section className="callout" aria-labelledby="removed-h">
          <h3 id="removed-h">Left out of the plan</h3>
          <ul className="plain">
            {removed.map((w) => (
              <li key={`${w.kind}-${w.id}`}>
                {w.kind === "refused" ? w.message : w.kind === "duplicate" ? "Queued twice; kept once." : "Inside another queued folder, which already covers it."}
              </li>
            ))}
          </ul>
        </section>
      )}
      <fieldset className="method">
        <legend>How to remove</legend>
        <label>
          <input
            type="radio"
            name="method"
            checked={decision.method === "recycle_bin"}
            onChange={() => {
              onChange({ ...decision, method: "recycle_bin", acks: { ...decision.acks, permanent: false, largePermanent: false } });
            }}
          />
          Move to Recycle Bin (recommended; restore from Undo history)
        </label>
        <label>
          <input
            type="radio"
            name="method"
            checked={decision.method === "permanent"}
            onChange={() => {
              onChange({ ...decision, method: "permanent" });
            }}
          />
          Delete permanently (can’t be undone)
        </label>
        {needsPermanentAck && (
          <label className="ack ack--danger">
            <input
              type="checkbox"
              checked={decision.acks.permanent}
              onChange={(e) => {
                onChange({ ...decision, acks: { ...decision.acks, permanent: e.target.checked } });
              }}
            />
            I understand permanently deleted items can’t be restored.
          </label>
        )}
        {needsPermanentAck && permanentLarge && (
          <label className="ack ack--danger">
            <input
              type="checkbox"
              checked={decision.acks.largePermanent}
              onChange={(e) => {
                onChange({ ...decision, acks: { ...decision.acks, largePermanent: e.target.checked } });
              }}
            />
            Confirm again: some items are larger than {formatBytes(plan.largeDeleteBytes, { units })}.
          </label>
        )}
      </fieldset>
      {blockers.length > 0 && (
        <div className="callout" id="blockers">
          <h3>Before continuing</h3>
          <ul className="plain">
            {blockers.map((b, i) => (
              <li key={`${b.id ?? "plan"}-${i}`}>{b.message}</li>
            ))}
          </ul>
        </div>
      )}
      <div className="toolbar">
        <button type="button" className="btn" onClick={onBack}>
          Back to queue
        </button>
        <button type="button" className="btn btn--primary" disabled={blockers.length > 0} aria-describedby={blockers.length > 0 ? "blockers" : undefined} onClick={onContinue}>
          Check items
        </button>
      </div>
    </div>
  );
}

// -----------------------------------------------------------------------------
// Pre-flight
// -----------------------------------------------------------------------------

function holderText(h: LockHolder): string {
  return `${h.appName} (PID ${h.pid})`;
}

/** Props for {@link PreflightPanel}. */
export interface PreflightPanelProps {
  plan: CleanupPlan;
  verdicts: ItemVerdict[] | null;
  decision: Decision;
  onChange: (d: Decision) => void;
  onRecheck: (d: Decision) => void;
  onBack: () => void;
  onExecute: () => void;
}

/** Pre-flight results: ready items, and blocked ones with Close app / Skip. */
export function PreflightPanel({ plan, verdicts, decision, onChange, onRecheck, onBack, onExecute }: PreflightPanelProps) {
  const features = useFeatures();
  const units = useSettings((s) => s.units);
  const canClose = useCapability("cleanup_close_prompt", "cleanup_close_app");
  const canReboot = useCapability("cleanup_delete_on_reboot");
  const [prompt, setPrompt] = useState<ClosePrompt | null>(null);
  const [note, setNote] = useState<string | null>(null);
  if (verdicts === null) {
    return (
      <div className="state state--quiet" role="status">
        <span className="spinner" aria-hidden="true" />
        <p>Checking every item still exists, is unchanged and isn’t in use…</p>
      </div>
    );
  }
  const skip = new Set(decision.skip);
  const active = verdicts.filter((v) => !skip.has(v.id));
  const blocked = active.filter((v) => v.verdict.status === "blocked");
  const ready = active.filter((v) => v.verdict.status === "ready");
  const bytes = ready.reduce((a, v) => a + (plan.items.find((i) => i.id === v.id)?.bytes ?? 0), 0);
  const skipItem = (id: number) => {
    onChange({ ...decision, skip: [...new Set([...decision.skip, id])] });
  };
  return (
    <div className="preflight">
      <h2 className="section-title">Pre-flight check</h2>
      <p role="status">
        {formatCount(ready.length)} ready ({formatBytes(bytes, { units })}), {formatCount(blocked.length)} need attention.
      </p>
      {note && (
        <p className="banner" role="status">
          {note}
        </p>
      )}
      {blocked.length > 0 && (
        <ul className="preflight__list" aria-label="Items that need attention">
          {blocked.map((v) => {
            if (v.verdict.status !== "blocked") return null;
            const err = v.verdict.error;
            const holders = v.holders.length > 0 ? v.holders : err.holders;
            return (
              <li key={v.id} className="preflight__item">
                <div className="cell-path">{v.path}</div>
                <p className="warn-text">{err.message}</p>
                {holders.map((h) => (
                  <p key={`${h.pid}-${h.startTime}`} className="holder">
                    In use by: <strong>{holderText(h)}</strong>
                    {" — "}
                    <button
                      type="button"
                      className="btn btn--small"
                      disabled={!canClose || h.kind === "critical"}
                      title={h.kind === "critical" ? "Critical system process; it can’t be closed." : canClose ? undefined : "Closing apps isn’t available in this build."}
                      onClick={() => {
                        features.cleanup.prepareClose(h).then(setPrompt, (e: unknown) => {
                          setNote(errorMessage(e));
                        });
                      }}
                    >
                      Close app
                    </button>{" "}
                    <button
                      type="button"
                      className="btn btn--small"
                      onClick={() => {
                        skipItem(v.id);
                      }}
                    >
                      Skip
                    </button>
                  </p>
                ))}
                {holders.length === 0 && (
                  <button
                    type="button"
                    className="btn btn--small"
                    onClick={() => {
                      skipItem(v.id);
                    }}
                  >
                    Skip
                  </button>
                )}
                {err.kind === "locked" && canReboot && (
                  <button
                    type="button"
                    className="btn btn--small"
                    onClick={() => {
                      features.cleanup.deleteOnReboot(plan.planId, v.id).then(
                        () => {
                          setNote("Scheduled for deletion at the next restart.");
                          skipItem(v.id);
                        },
                        (e: unknown) => {
                          setNote(errorMessage(e));
                        },
                      );
                    }}
                  >
                    Delete on next restart
                  </button>
                )}
              </li>
            );
          })}
        </ul>
      )}
      {active.some((v) => v.runningApps.length > 0) && (
        <section className="callout">
          <h3>Running apps</h3>
          <ul className="plain">
            {[...new Map(active.flatMap((v) => v.runningApps).map((w) => [w.app, w])).values()].map((w) => (
              <li key={w.app}>
                {w.app} is running. Close it first so it doesn’t recreate or hold its cache.
              </li>
            ))}
          </ul>
        </section>
      )}
      <div className="toolbar">
        <button type="button" className="btn" onClick={onBack}>
          Back to review
        </button>
        <button
          type="button"
          className="btn"
          onClick={() => {
            onRecheck(decision);
          }}
        >
          Check again
        </button>
        <button
          type="button"
          className={decision.method === "permanent" ? "btn btn--danger" : "btn btn--primary"}
          disabled={blocked.length > 0 || ready.length === 0}
          title={blocked.length > 0 ? "Resolve or skip the items above first." : undefined}
          onClick={onExecute}
        >
          {decision.method === "permanent" ? "Delete permanently" : "Move to Recycle Bin"} ({formatCount(ready.length)})
        </button>
      </div>
      {prompt && (
        <ConfirmDialog
          title={`Close ${prompt.app}?`}
          confirmLabel={`Close ${prompt.app}`}
          onCancel={() => {
            setPrompt(null);
          }}
          onConfirm={() => {
            const p = prompt;
            setPrompt(null);
            if (Date.now() > p.expiresMs) {
              setNote("That confirmation expired. Choose Close app again.");
              return;
            }
            features.cleanup.closeApp(p.promptId).then(
              (out) => {
                setNote(out.kind === "shut_down" ? `${p.app} closed.` : `${p.app} was asked to close; it may ask to save work.`);
                onRecheck(decision);
              },
              (e: unknown) => {
                setNote(errorMessage(e));
              },
            );
          }}
        >
          <p>{prompt.message}</p>
          <p className="detail__muted">Strata asks the app to close the normal way. It is never killed; unsaved work prompts stay with the app.</p>
        </ConfirmDialog>
      )}
    </div>
  );
}

// -----------------------------------------------------------------------------
// Running and results
// -----------------------------------------------------------------------------

function RunningPanel({ stage }: { stage: Extract<Stage, { kind: "running" }> }) {
  const features = useFeatures();
  const units = useSettings((s) => s.units);
  const [cancelling, setCancelling] = useState(false);
  return (
    <div className="running">
      <h2 className="section-title">Removing items</h2>
      <div role="progressbar" aria-label="Cleanup progress" aria-valuemin={0} aria-valuemax={stage.total} aria-valuenow={stage.done} className="progress">
        <span className="progress__fill" style={{ width: `${stage.total === 0 ? 0 : (stage.done / stage.total) * 100}%` }} />
      </div>
      <p aria-live="polite">
        {formatCount(stage.done)} of {formatCount(stage.total)} · {formatBytes(stage.bytes, { units })} freed
      </p>
      <button
        type="button"
        className="btn"
        disabled={cancelling}
        onClick={() => {
          setCancelling(true);
          void features.cleanup.cancelCleanup(stage.plan.planId).catch((e: unknown) => {
            useApp.getState().notify(errorMessage(e));
          });
        }}
      >
        {cancelling ? "Cancelling…" : "Cancel"}
      </button>
    </div>
  );
}

/** Props for {@link ResultsPanel}. */
export interface ResultsPanelProps {
  plan: CleanupPlan;
  report: ExecutionReport;
  onRetry: () => void;
  onDone: () => void;
}

/** Per-item results with failures, reasons and retry. */
export function ResultsPanel({ plan, report, onRetry, onDone }: ResultsPanelProps) {
  const units = useSettings((s) => s.units);
  const heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => {
    heading.current?.focus();
  }, []);
  const s = report.summary;
  const retryable = report.results.filter((r) => (r.outcome.kind === "failed" && r.outcome.error.retryable) || (r.outcome.kind === "skipped" && r.outcome.reason.retryable));
  const name = (id: number) => plan.items.find((i) => i.id === id)?.name;
  return (
    <div className="results">
      <h2 className="section-title" tabIndex={-1} ref={heading}>
        {s.cancelled ? "Cleanup cancelled" : "Cleanup finished"}
      </h2>
      <p>
        <strong>{formatBytes(s.bytes, { units })}</strong> freed · {formatCount(s.succeeded)} removed · {formatCount(s.failed)} failed · {formatCount(s.skipped)} skipped
      </p>
      <table className="table">
        <caption className="visually-hidden">Results per item</caption>
        <thead>
          <tr>
            <th scope="col">Item</th>
            <th scope="col">Result</th>
          </tr>
        </thead>
        <tbody>
          {report.results.map((r) => (
            <tr key={r.id}>
              <td>
                <div className="cell-name">{name(r.id) ?? r.path}</div>
                <div className="cell-path">{r.path}</div>
              </td>
              <td>
                {r.outcome.kind === "recycled" && <span className="ok-text">Moved to Recycle Bin</span>}
                {r.outcome.kind === "deleted" && <span className="ok-text">Deleted ({formatBytes(r.outcome.bytes, { units })})</span>}
                {r.outcome.kind === "failed" && (
                  <span className="warn-text">
                    Failed: {r.outcome.error.message}
                    {r.outcome.error.retryable && " (can retry)"}
                  </span>
                )}
                {r.outcome.kind === "skipped" && <span className="detail__muted">Skipped: {r.outcome.reason.message}</span>}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <div className="toolbar">
        {retryable.length > 0 && (
          <button type="button" className="btn btn--primary" onClick={onRetry}>
            Retry {formatCount(retryable.length)} failed
          </button>
        )}
        <button type="button" className="btn" onClick={onDone}>
          Done
        </button>
      </div>
    </div>
  );
}

// -----------------------------------------------------------------------------
// Undo history
// -----------------------------------------------------------------------------

const STATUS_TEXT: Readonly<Record<UndoAction["status"], string>> = {
  in_progress: "In progress",
  completed: "Completed",
  partial: "Partly completed",
  failed: "Failed",
  cancelled: "Cancelled",
  interrupted: "Interrupted (recovered)",
};

function UndoHistory() {
  const features = useFeatures();
  const has = useCapability("cleanup_history");
  const canRestore = useCapability("cleanup_restore");
  const units = useSettings((s) => s.units);
  const [load, reload] = useLoad(() => features.cleanup.fetchUndoHistory(50, null), [features], has);
  const [picked, setPicked] = useState<ReadonlySet<number>>(new Set());
  const [msg, setMsg] = useState<string | null>(null);
  if (!has) return <Unavailable feature="Undo history" command="cleanup_history" />;
  const restore = (ids: number[]) => {
    features.cleanup.restoreItems(ids).then(
      (res) => {
        const ok = res.filter((r) => r.ok).length;
        const bad = res.filter((r) => !r.ok);
        setMsg(`${ok} restored.${bad.length > 0 ? ` ${bad.length} not restored: ${bad[0]?.message ?? ""}` : ""}`);
        setPicked(new Set());
        reload();
      },
      (e: unknown) => {
        setMsg(errorMessage(e));
      },
    );
  };
  return (
    <LoadState load={load} feature="Undo history" command="cleanup_history" onRetry={reload}>
      {(actions) =>
        actions.length === 0 ? (
          <div className="state state--quiet">
            <h2>No cleanups yet</h2>
            <p>Every cleanup is logged here before it acts. Items moved to the Recycle Bin can be restored.</p>
          </div>
        ) : (
          <>
            {msg && (
              <p className="banner" role="status">
                {msg}
              </p>
            )}
            <div className="toolbar">
              <button
                type="button"
                className="btn btn--primary"
                disabled={!canRestore || picked.size === 0}
                onClick={() => {
                  restore([...picked]);
                }}
              >
                Restore selected ({picked.size})
              </button>
            </div>
            {actions.map((a) => (
              <section key={a.actionId} className="undo" aria-label={`${a.kind} on ${formatDateTime(a.startedMs)}`}>
                <h3 className="undo__head">
                  {formatDateTime(a.startedMs)} <span className="detail__muted">({formatRelative(a.startedMs)})</span> · {STATUS_TEXT[a.status]} · {formatCount(a.doneCount)} of{" "}
                  {formatCount(a.itemCount)} · {formatBytes(a.bytesDone, { units })}
                </h3>
                <ul className="plain undo__items">
                  {a.items.map((i) => (
                    <li key={i.itemId} className="undo__item">
                      {i.restorable ? (
                        <input
                          type="checkbox"
                          aria-label={`Select ${i.path} to restore`}
                          checked={picked.has(i.itemId)}
                          onChange={(e) => {
                            const next = new Set(picked);
                            if (e.target.checked) next.add(i.itemId);
                            else next.delete(i.itemId);
                            setPicked(next);
                          }}
                        />
                      ) : (
                        <span className="undo__spacer" />
                      )}
                      <span className="cell-path">{i.path}</span>
                      <span className="detail__muted">
                        {formatBytes(i.bytes, { units })} ·{" "}
                        {i.restoredMs !== null
                          ? `restored ${formatRelative(i.restoredMs)}`
                          : i.result === "done"
                            ? i.method === "recycle"
                              ? "in Recycle Bin"
                              : "deleted permanently"
                            : i.result === "failed"
                              ? `failed: ${i.error ?? "unknown error"}`
                              : i.result}
                      </span>
                      {i.restorable && (
                        <button
                          type="button"
                          className="btn btn--small"
                          disabled={!canRestore}
                          onClick={() => {
                            restore([i.itemId]);
                          }}
                        >
                          Restore
                        </button>
                      )}
                    </li>
                  ))}
                </ul>
              </section>
            ))}
          </>
        )
      }
    </LoadState>
  );
}
