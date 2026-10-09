/**
 * The sidebar: navigation for the active area.
 *
 * - Explore: volumes with capacity bars and scan state, and saved filters.
 * - Insights / Cleanup / Activity: the area's views.
 * - History: which volume's history to show.
 *
 * Sections collapse (remembered per user). The sidebar resizes with a
 * splitter and becomes an overlay in narrow windows.
 */
import { useState, type ReactNode } from "react";
import { Icon, type IconName } from "../components/icons";
import { errorMessage } from "../lib/backend";
import { formatBytes, formatCount } from "../lib/format";
import { hasFilters, type ViewFilters, type ViewId } from "../lib/types";
import { volumeName, type VolumeInfo } from "../lib/volumes";
import { useServices } from "../services";
import { useApp } from "../store/app";
import { useQueue } from "../store/queue";
import { useSettings } from "../store/settings";
import { useVolumes } from "../store/volumes";
import { FEATURE_VIEWS } from "../views/featureViews";
import { areaInfo, areaOf, type AreaId } from "./areas";
import { describeFilters } from "./filters";
import { ariaKeys, shortcutOf } from "./keymap";
import { useLayout } from "./layout";
import { openSettings } from "./navigation";
import { etaText, scanPercent } from "./scan";

// -----------------------------------------------------------------------------
// Building blocks
// -----------------------------------------------------------------------------

function Section({ id, title, actions, children }: { id: string; title: string; actions?: ReactNode; children: ReactNode }) {
  const collapsed = useLayout((s) => s.collapsed[id] ?? false);
  const toggle = useLayout((s) => s.toggleSection);
  const bodyId = `sb-${id}`;
  return (
    <section className="sb-section" aria-labelledby={`${bodyId}-h`}>
      <div className="sb-section__head">
        <button
          type="button"
          id={`${bodyId}-h`}
          className="sb-section__toggle"
          aria-expanded={!collapsed}
          aria-controls={bodyId}
          onClick={() => {
            toggle(id);
          }}
        >
          <Icon name="chevron" size={12} className={collapsed ? "sb-caret" : "sb-caret is-open"} />
          {title}
        </button>
        {actions && <div className="sb-section__actions">{actions}</div>}
      </div>
      <div id={bodyId} className="sb-section__body" hidden={collapsed}>
        {children}
      </div>
    </section>
  );
}

function NavItem({ icon, label, current, disabled, reason, badge, onClick }: { icon: IconName; label: string; current: boolean; disabled?: boolean; reason?: string; badge?: ReactNode; onClick: () => void }) {
  return (
    <li>
      <button
        type="button"
        className="sb-item"
        aria-current={current ? "page" : undefined}
        aria-disabled={disabled ? true : undefined}
        title={disabled ? reason : undefined}
        onClick={() => {
          if (!disabled) onClick();
        }}
      >
        <Icon name={icon} size={16} />
        <span className="sb-item__label">{label}</span>
        {badge !== undefined && <span className="sb-item__meta">{badge}</span>}
      </button>
    </li>
  );
}

// -----------------------------------------------------------------------------
// Volumes
// -----------------------------------------------------------------------------

const STATE_TEXT = { never: "Not scanned", scanning: "Scanning", live: "Live", stale: "Stale", partial: "Partial" } as const;

/** One volume row: name, scan state and a capacity bar. */
function VolumeRow({ v, current, onOpen }: { v: VolumeInfo; current: boolean; onOpen: (v: VolumeInfo) => void }) {
  const services = useServices();
  const units = useSettings((s) => s.units);
  const notify = useApp((s) => s.notify);
  const [busy, setBusy] = useState(false);
  const used = Math.max(0, v.totalBytes - v.freeBytes);
  const fill = v.totalBytes > 0 ? used / v.totalBytes : 0;
  const locked = v.bitlocker === "locked";
  const state = v.scan.state;
  const progress = state === "scanning" ? v.scan.progress : null;
  const indexed = v.scan.rootId !== null && v.present;
  const free = `${formatBytes(v.freeBytes, { units })} free of ${formatBytes(v.totalBytes, { units })}`;
  const stateText = locked ? "Locked" : !v.present ? "Disconnected" : progress?.fraction != null ? `Scanning ${scanPercent(progress.fraction)}` : STATE_TEXT[state];
  const scan = () => {
    setBusy(true);
    services.volumes
      .startScan(v.id, "auto")
      .catch((err: unknown) => {
        notify(errorMessage(err));
      })
      .finally(() => {
        setBusy(false);
      });
  };
  return (
    <li className={`vol${current ? " is-current" : ""}${v.present ? "" : " is-absent"}`}>
      <button
        type="button"
        className="vol__main"
        aria-current={current ? "true" : undefined}
        aria-label={`${volumeName(v)}, ${stateText}, ${free}${indexed ? "" : locked ? "" : ". Press to scan"}`}
        disabled={locked || !v.present || (!indexed && (busy || state === "scanning"))}
        onClick={() => {
          if (indexed) onOpen(v);
          else scan();
        }}
      >
        <span className="vol__row">
          <Icon name={locked ? "lock" : "drive"} size={16} />
          <span className="vol__name">{volumeName(v)}</span>
          <span className={`vol__state vol__state--${locked ? "locked" : state}`}>{stateText}</span>
        </span>
        <span className="vol__bar" aria-hidden="true">
          {progress?.fraction != null ? (
            <span className="vol__scan" style={{ width: `${progress.fraction * 100}%` }} />
          ) : (
            <span className={fill > 0.9 ? "vol__fill is-full" : "vol__fill"} style={{ width: `${Math.min(100, fill * 100)}%` }} />
          )}
        </span>
        <span className="vol__meta">{progress ? [`${formatCount(progress.entries)} items`, progress.etaSecs != null ? etaText(progress.etaSecs) : ""].filter(Boolean).join(" · ") : free}</span>
      </button>
      {indexed && state !== "scanning" && !locked && (
        <button type="button" className="icon-btn icon-btn--sm vol__action" aria-label={`Rescan ${volumeName(v)}`} data-tip="Rescan" disabled={busy} onClick={scan}>
          <Icon name="refresh" size={14} />
        </button>
      )}
      {state === "scanning" && (
        <button
          type="button"
          className="icon-btn icon-btn--sm vol__action"
          aria-label={`Cancel scanning ${volumeName(v)}`}
          data-tip="Cancel scan"
          onClick={() => {
            services.volumes.cancelScan(v.id).catch((err: unknown) => {
              notify(errorMessage(err));
            });
          }}
        >
          <Icon name="stop" size={14} />
        </button>
      )}
    </li>
  );
}

function VolumeList({ onOpen, emptyHint }: { onOpen: (v: VolumeInfo) => void; emptyHint: string }) {
  const volumes = useVolumes((s) => s.volumes);
  const error = useVolumes((s) => s.error);
  const unavailable = useVolumes((s) => s.unavailable);
  const volumeId = useApp((s) => s.volumeId);
  if (error) return <p className="sb-note">{unavailable ? "Drive scanning isn’t connected in this build." : `Couldn’t list volumes: ${error}`}</p>;
  if (volumes === null)
    return (
      <div className="sb-skeleton" role="status" aria-label="Finding volumes">
        <span className="skeleton" />
        <span className="skeleton skeleton--short" />
      </div>
    );
  if (volumes.length === 0) return <p className="sb-note">{emptyHint}</p>;
  return (
    <ul className="vol-list">
      {volumes.map((v) => (
        <VolumeRow key={v.id} v={v} current={v.id === volumeId} onOpen={onOpen} />
      ))}
    </ul>
  );
}

// -----------------------------------------------------------------------------
// Saved filters
// -----------------------------------------------------------------------------

function SavedFilters() {
  const saved = useLayout((s) => s.savedFilters);
  const remove = useLayout((s) => s.deleteFilter);
  const current = useApp((s) => s.filters);
  const setFilters = useApp((s) => s.setFilters);
  const hasVolume = useApp((s) => s.volumeId !== null);
  const same = (f: ViewFilters) => JSON.stringify(f) === JSON.stringify(current);
  if (saved.length === 0) return <p className="sb-note">Save the filters you use often from the toolbar’s Filter menu.</p>;
  return (
    <ul className="sb-list">
      {saved.map((f) => (
        <li key={f.id} className="sb-filter">
          <button
            type="button"
            className="sb-item"
            aria-pressed={hasFilters(f.filters) && same(f.filters)}
            aria-disabled={!hasVolume || undefined}
            title={hasVolume ? describeFilters(f.filters).join(" · ") : "Open a scanned volume first"}
            onClick={() => {
              if (hasVolume) setFilters(f.filters);
            }}
          >
            <Icon name="filter" size={16} />
            <span className="sb-item__label">{f.name}</span>
          </button>
          <button
            type="button"
            className="icon-btn icon-btn--sm sb-filter__delete"
            aria-label={`Delete saved filter ${f.name}`}
            data-tip="Delete"
            onClick={() => {
              remove(f.id);
            }}
          >
            <Icon name="close" size={12} />
          </button>
        </li>
      ))}
    </ul>
  );
}

// -----------------------------------------------------------------------------
// Area panels
// -----------------------------------------------------------------------------

function FeatureNav({ area }: { area: AreaId }) {
  const view = useApp((s) => s.view);
  const setView = useApp((s) => s.setView);
  const hasVolume = useApp((s) => s.volumeId !== null);
  const queued = useQueue((s) => s.items?.length ?? 0);
  return (
    <ul className="sb-list">
      {FEATURE_VIEWS.filter((f) => areaOf(f.id) === area).map((f) => (
        <NavItem
          key={f.id}
          icon={f.icon}
          label={f.label}
          current={view === f.id}
          disabled={f.needsVolume && !hasVolume}
          reason={`${f.label}: open a scanned volume first`}
          {...(f.id === "cleanup" && queued > 0 ? { badge: queued } : {})}
          onClick={() => {
            setView(f.id);
          }}
        />
      ))}
    </ul>
  );
}

function openIndexed(v: VolumeInfo, keepView: ViewId | null): void {
  if (v.scan.rootId === null) return;
  const app = useApp.getState();
  if (keepView) {
    useApp.setState({ volumeId: v.id, path: [v.scan.rootId], selection: [], primary: null });
    return;
  }
  if (app.volumeId === v.id) {
    if (app.view === "home") app.setView(app.lastVisual);
    return;
  }
  app.openVolume(v.id, v.scan.rootId);
}

function AreaPanel({ area }: { area: AreaId }) {
  const view = useApp((s) => s.view);
  switch (area) {
    case "explore":
      return (
        <>
          <Section id="explore.volumes" title="Volumes">
            <VolumeList
              emptyHint="No drives found. Strata lists fixed and removable drives."
              onOpen={(v) => {
                openIndexed(v, null);
              }}
            />
          </Section>
          <Section id="explore.filters" title="Saved filters">
            <SavedFilters />
          </Section>
        </>
      );
    case "history":
      return (
        <>
          <Section id="history.views" title="History">
            <FeatureNav area="history" />
          </Section>
          <Section id="history.volumes" title="Volume">
            <VolumeList
              emptyHint="No drives found."
              onOpen={(v) => {
                openIndexed(v, view);
              }}
            />
          </Section>
        </>
      );
    case "activity":
      return (
        <>
          <Section id="activity.views" title="Activity">
            <FeatureNav area="activity" />
          </Section>
          <Section id="activity.about" title="About tracking">
            <p className="sb-note">
              Activity tracking records which programs write to disk. It is off by default, needs the elevated helper, and stays on this PC.
            </p>
            <button
              type="button"
              className="sb-link"
              onClick={() => {
                openSettings("activity");
              }}
            >
              Activity tracking settings
            </button>
          </Section>
        </>
      );
    case "cleanup":
      return (
        <>
          <Section id="cleanup.views" title="Cleanup">
            <FeatureNav area="cleanup" />
          </Section>
          <Section id="cleanup.safety" title="Safety">
            <p className="sb-note">Items go to the Recycle Bin by default, and Never-tier items can’t be queued.</p>
            <button
              type="button"
              className="sb-link"
              onClick={() => {
                openSettings("cleanup");
              }}
            >
              Cleanup and safety settings
            </button>
          </Section>
        </>
      );
    case "insights":
      return (
        <Section id="insights.views" title="Views">
          <FeatureNav area="insights" />
        </Section>
      );
    case "settings":
      return null;
  }
}

/** The sidebar for the active area. */
export function Sidebar({ area, overlay }: { area: AreaId; overlay: boolean }) {
  const set = useLayout((s) => s.set);
  const setOverlay = useLayout((s) => s.setSidebarOverlay);
  const info = areaInfo(area);
  const hide = shortcutOf("toggleSidebar");
  return (
    <nav
      className={overlay ? "sidebar sidebar--overlay" : "sidebar"}
      aria-label={`${info.label} sidebar`}
      data-region="sidebar"
      onKeyDown={(e) => {
        if (overlay && e.key === "Escape") {
          e.stopPropagation();
          setOverlay(false);
        }
      }}
    >
      <div className="sidebar__head">
        <h2 className="sidebar__title">{info.label}</h2>
        <button
          type="button"
          className="icon-btn icon-btn--sm"
          aria-label="Hide sidebar"
          aria-keyshortcuts={ariaKeys(hide)}
          data-tip={`Hide sidebar (${hide ?? ""})`}
          onClick={() => {
            if (overlay) setOverlay(false);
            else set({ sidebarOpen: false });
          }}
        >
          <Icon name="nav" size={16} />
        </button>
      </div>
      <div className="sidebar__body">
        <AreaPanel area={area} />
      </div>
    </nav>
  );
}
