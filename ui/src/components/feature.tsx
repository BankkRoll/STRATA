/**
 * Building blocks shared by the wave-2 views: capability checks, the
 * designed "unavailable" state, async loading with loading/error states,
 * view framing, safety badges and an accessible confirmation dialog.
 */
import { useCallback, useEffect, useId, useRef, useState, useSyncExternalStore, type ReactNode } from "react";
import { BackendUnavailableError, errorMessage } from "../lib/backend";
import type { Safety } from "../lib/types";
import { useServices } from "../services";
import { Icon } from "./icons";
import "../views/features.css";

// -----------------------------------------------------------------------------
// Capabilities
// -----------------------------------------------------------------------------

const capListeners = new Set<() => void>();
let capTimer: ReturnType<typeof setInterval> | null = null;

/**
 * Whether every listed backend command exists in this build.
 *
 * NOTE: `app_capabilities` loads asynchronously at startup and the services
 * object exposes no change event, so while the set is still empty this
 * re-reads it every 250 ms for up to 10 s instead of showing "unavailable"
 * for commands that are about to appear.
 *
 * @param commands - Command names.
 * @returns `true` when all exist.
 */
export function useCapability(...commands: string[]): boolean {
  const services = useServices();
  return useSyncExternalStore(
    (cb) => {
      capListeners.add(cb);
      if (services.capabilities().size === 0 && capTimer === null) {
        let tries = 0;
        capTimer = setInterval(() => {
          tries++;
          for (const l of capListeners) l();
          if (services.capabilities().size > 0 || tries >= 40) {
            if (capTimer !== null) clearInterval(capTimer);
            capTimer = null;
          }
        }, 250);
      }
      return () => {
        capListeners.delete(cb);
        if (capListeners.size === 0 && capTimer !== null) {
          clearInterval(capTimer);
          capTimer = null;
        }
      };
    },
    () => commands.every((c) => services.capabilities().has(c)),
  );
}

// -----------------------------------------------------------------------------
// States
// -----------------------------------------------------------------------------

/** Props for {@link Unavailable}. */
export interface UnavailableProps {
  /** What the user was trying to see ("Duplicate finder"). */
  feature: string;
  /** Backend command that is missing (shown for support). */
  command: string;
  /** What the feature will do once present. */
  children?: ReactNode;
}

/** Designed state for a feature whose backend command is not in this build. */
export function Unavailable({ feature, command, children }: UnavailableProps) {
  return (
    <div className="state state--quiet unavailable" role="note">
      <Icon name="info" size={28} />
      <h2>{feature} isn’t available in this build</h2>
      {children && <p>{children}</p>}
      <p className="detail__muted">
        The engine doesn’t provide <code>{command}</code> yet. Nothing here is simulated.
      </p>
    </div>
  );
}

/** Async load state. */
export type Load<T> =
  | { state: "loading" }
  | { state: "ready"; data: T }
  | { state: "error"; message: string }
  | { state: "unavailable"; message: string };

/**
 * Runs `fn` when `enabled` and whenever `deps` change; exposes the result
 * and a reload function. Stale results are dropped.
 *
 * @param fn - Loader.
 * @param deps - Dependencies that trigger a reload.
 * @param enabled - Skip loading (e.g. capability missing).
 * @returns Current state and `reload`.
 */
export function useLoad<T>(fn: () => Promise<T>, deps: readonly unknown[], enabled = true): [Load<T>, () => void] {
  const [result, setResult] = useState<{ key: number; load: Load<T> } | null>(null);
  const [key, setKey] = useState(0);
  const [prevDeps, setPrevDeps] = useState(deps);
  // Changed inputs start a new request generation; adjusting state during
  // render (not in an effect) avoids an extra committed render.
  if (prevDeps.length !== deps.length || prevDeps.some((d, i) => !Object.is(d, deps[i]))) {
    setPrevDeps(deps);
    setKey(key + 1);
  }
  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    fn().then(
      (data) => {
        if (!cancelled) setResult({ key, load: { state: "ready", data } });
      },
      (err: unknown) => {
        if (cancelled) return;
        setResult({
          key,
          load: err instanceof BackendUnavailableError ? { state: "unavailable", message: err.reason } : { state: "error", message: errorMessage(err) },
        });
      },
    );
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps -- `fn` is a new closure every render; `key` tracks the caller's deps.
  }, [enabled, key]);
  const reload = useCallback(() => {
    setKey((k) => k + 1);
  }, []);
  // While a newer request runs, keep showing the last data instead of flashing a spinner.
  const load: Load<T> = result && (result.key === key || result.load.state === "ready") ? result.load : { state: "loading" };
  return [load, reload];
}

/** Props for {@link LoadState}. */
export interface LoadStateProps<T> {
  load: Load<T>;
  /** Feature name for the unavailable state. */
  feature: string;
  command: string;
  onRetry?: () => void;
  children: (data: T) => ReactNode;
}

/** Renders loading / error / unavailable states, or `children` with the data. */
export function LoadState<T>({ load, feature, command, onRetry, children }: LoadStateProps<T>) {
  switch (load.state) {
    case "loading":
      return (
        <div className="state state--quiet" role="status">
          <span className="spinner" aria-hidden="true" />
          <p>Loading…</p>
        </div>
      );
    case "unavailable":
      return <Unavailable feature={feature} command={command} />;
    case "error":
      return (
        <div className="state state--error" role="alert">
          <h2>Couldn’t load {feature.toLowerCase()}</h2>
          <p>{load.message}</p>
          {onRetry && (
            <button type="button" className="btn" onClick={onRetry}>
              Try again
            </button>
          )}
        </div>
      );
    case "ready":
      return <>{children(load.data)}</>;
  }
}

// -----------------------------------------------------------------------------
// Framing and badges
// -----------------------------------------------------------------------------

/** Props for {@link ViewFrame}. */
export interface ViewFrameProps {
  title: string;
  /** One-line description under the title. */
  lead?: ReactNode;
  /** Toolbar on the right of the title. */
  actions?: ReactNode;
  children: ReactNode;
}

/** Page frame of a non-visual view: heading, lead, toolbar and scrollable body. */
export function ViewFrame({ title, lead, actions, children }: ViewFrameProps) {
  const id = useId();
  return (
    <section className="fview" aria-labelledby={id}>
      <header className="fview__head">
        <div>
          <h1 id={id} className="fview__title">
            {title}
          </h1>
          {lead && <p className="fview__lead">{lead}</p>}
        </div>
        {actions && <div className="fview__actions">{actions}</div>}
      </header>
      <div className="fview__body">{children}</div>
    </section>
  );
}

const SAFETY_TEXT: Readonly<Record<Safety, string>> = { safe: "Safe", probably: "Probably safe", careful: "Careful", never: "Never" };

/**
 * Safety tier badge: text plus a shape per tier so it never relies on color.
 *
 * @param props - `tier`, or `null` for unclassified.
 */
export function SafetyBadge({ tier }: { tier: Safety | null }) {
  if (tier === null) return <span className="sbadge sbadge--none">Not classified</span>;
  return (
    <span className={`sbadge sbadge--${tier}`}>
      <span className="sbadge__mark" aria-hidden="true" />
      {SAFETY_TEXT[tier]}
    </span>
  );
}

/** Display label of a safety tier. */
export function safetyLabel(tier: Safety): string {
  return SAFETY_TEXT[tier];
}

// -----------------------------------------------------------------------------
// Confirmation dialog
// -----------------------------------------------------------------------------

/** Props for {@link ConfirmDialog}. */
export interface ConfirmDialogProps {
  title: string;
  children: ReactNode;
  /** Label of the confirming button (a verb: "Empty Recycle Bin"). */
  confirmLabel: string;
  /** Styles the confirm button as destructive. */
  danger?: boolean;
  /** Disable confirming (e.g. a required checkbox is unticked). */
  confirmDisabled?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}

/**
 * Modal confirmation (ARIA dialog pattern): focus moves to Cancel, Tab is
 * trapped, Escape cancels, focus returns to the opener on close. Cancel is
 * focused first so Enter never confirms a destructive action by accident.
 */
export function ConfirmDialog({ title, children, confirmLabel, danger, confirmDisabled, onConfirm, onCancel }: ConfirmDialogProps) {
  const id = useId();
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    ref.current?.querySelector<HTMLElement>("[data-autofocus]")?.focus();
    return () => {
      opener?.focus();
    };
  }, []);
  const onKey = (e: React.KeyboardEvent) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      onCancel();
      return;
    }
    if (e.key !== "Tab" || !ref.current) return;
    const items = [...ref.current.querySelectorAll<HTMLElement>("button:not([disabled]), input:not([disabled]), select, textarea, [tabindex='0']")];
    const first = items[0];
    const last = items[items.length - 1];
    if (!first || !last) return;
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  };
  return (
    <div className="modal-backdrop">
      <div ref={ref} className="modal" role="dialog" aria-modal="true" aria-labelledby={id} onKeyDown={onKey}>
        <h2 id={id} className="modal__title">
          {title}
        </h2>
        <div className="modal__body">{children}</div>
        <div className="modal__actions">
          <button type="button" className="btn" data-autofocus onClick={onCancel}>
            Cancel
          </button>
          <button type="button" className={danger ? "btn btn--danger" : "btn btn--primary"} disabled={confirmDisabled} onClick={onConfirm}>
            {confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
