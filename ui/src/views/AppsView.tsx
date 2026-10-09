/**
 * Apps (SPEC §12.3): every installed app's footprint broken down by
 * location, attribution confidence with evidence, registry-vs-measured
 * mismatches, Uninstall (the app's own uninstaller, confirmed) and Clean
 * caches (queued for review), plus orphaned app data from uninstalled apps.
 */
import { useMemo, useState } from "react";
import { LoadState, SafetyBadge, Unavailable, ViewFrame, useCapability, useLoad } from "../components/feature";
import { useFeatures } from "../features";
import { errorMessage } from "../lib/backend";
import { formatBytes, formatRelative } from "../lib/format";
import type { AppFootprint, FootprintKind } from "../lib/insights";
import type { ToolPrompt } from "../lib/tools";
import { useApp } from "../store/app";
import { reportAdd } from "../store/queue";
import { useSettings } from "../store/settings";
import { ToolConfirm } from "./ToolsView";

const KIND_TEXT: Readonly<Record<FootprintKind, string>> = {
  install: "Install folder",
  data: "Data",
  cache: "Caches",
  logs: "Logs",
  updates: "Updates / old versions",
  other: "Other",
};

const CONFIDENCE_TEXT = { exact: "Exact", high: "High", heuristic: "Heuristic" } as const;

type Sort = "size" | "name" | "mismatch";

/**
 * Filters and sorts apps for display.
 *
 * @param apps - Catalog.
 * @param query - Case-insensitive name/publisher filter.
 * @param sort - Sort key.
 * @returns Sorted apps.
 */
export function arrangeApps(apps: readonly AppFootprint[], query: string, sort: Sort): AppFootprint[] {
  const q = query.trim().toLowerCase();
  const out = apps.filter((a) => q === "" || a.name.toLowerCase().includes(q) || (a.publisher ?? "").toLowerCase().includes(q));
  if (sort === "mismatch") return out.filter((a) => a.mismatch).sort((a, b) => b.totalBytes - a.totalBytes);
  return out.sort((a, b) => (sort === "name" ? a.name.localeCompare(b.name) : b.totalBytes - a.totalBytes));
}

/** Apps view. */
export function AppsView() {
  const features = useFeatures();
  const has = useCapability("apps_footprint");
  const hasOrphans = useCapability("apps_orphans");
  const [load, reload] = useLoad(() => features.insights.fetchApps(), [features], has);
  const [query, setQuery] = useState("");
  const [sort, setSort] = useState<Sort>("size");
  if (!has) {
    return (
      <ViewFrame title="Apps">
        <Unavailable feature="Apps" command="apps_footprint">
          Each installed app’s total footprint: install folder, data, caches, logs and old versions.
        </Unavailable>
      </ViewFrame>
    );
  }
  return (
    <ViewFrame
      title="Apps"
      lead="What each installed app really uses, wherever it put it."
      actions={
        <>
          <label>
            <span className="visually-hidden">Filter apps</span>
            <input
              type="search"
              className="input"
              placeholder="Filter apps"
              value={query}
              onChange={(e) => {
                setQuery(e.target.value);
              }}
            />
          </label>
          <label>
            <span className="visually-hidden">Sort</span>
            <select
              className="select"
              value={sort}
              onChange={(e) => {
                setSort(e.target.value as Sort);
              }}
            >
              <option value="size">Largest first</option>
              <option value="name">By name</option>
              <option value="mismatch">Size mismatches only</option>
            </select>
          </label>
        </>
      }
    >
      <LoadState load={load} feature="Apps" command="apps_footprint" onRetry={reload}>
        {(apps) => <AppList apps={arrangeApps(apps, query, sort)} />}
      </LoadState>
      {hasOrphans && <Orphans />}
    </ViewFrame>
  );
}

function AppList({ apps }: { apps: AppFootprint[] }) {
  const [open, setOpen] = useState<string | null>(null);
  if (apps.length === 0) return <p className="detail__muted">No apps match.</p>;
  return (
    <ul className="app-list">
      {apps.map((a) => (
        <AppRow
          key={a.id}
          app={a}
          open={open === a.id}
          onToggle={() => {
            setOpen(open === a.id ? null : a.id);
          }}
        />
      ))}
    </ul>
  );
}

function AppRow({ app, open, onToggle }: { app: AppFootprint; open: boolean; onToggle: () => void }) {
  const features = useFeatures();
  const units = useSettings((s) => s.units);
  const canUninstall = useCapability("tools_prepare", "tools_run");
  const canClean = useCapability("apps_queue_caches");
  const [prompt, setPrompt] = useState<ToolPrompt | null>(null);
  const byKind = useMemo(() => {
    const m = new Map<FootprintKind, number>();
    for (const l of app.locations) m.set(l.kind, (m.get(l.kind) ?? 0) + l.bytes);
    return m;
  }, [app.locations]);
  const panel = `app-${app.id}`;
  return (
    <li className="app">
      <button type="button" className="app__head" aria-expanded={open} aria-controls={panel} onClick={onToggle}>
        <span className={`twisty${open ? " twisty--open" : ""}`} aria-hidden="true">
          ▸
        </span>
        <span className="app__name">{app.name}</span>
        {app.publisher && <span className="detail__muted"> {app.publisher}</span>}
        <span className="app__size">{formatBytes(app.totalBytes, { units })}</span>
        {app.mismatch && <span className="badge badge--warn">size mismatch</span>}
        {app.running && <span className="badge">running</span>}
      </button>
      <div id={panel} hidden={!open} className="app__body">
        <p>
          Attribution: <span className="badge">{CONFIDENCE_TEXT[app.confidence]} confidence</span> from {app.source}
          {app.version && <span className="detail__muted"> · version {app.version}</span>}
        </p>
        {app.evidence.length > 0 && (
          <ul className="plain evidence" aria-label="Evidence">
            {app.evidence.map((e) => (
              <li key={e}>{e}</li>
            ))}
          </ul>
        )}
        {app.registryEstimateBytes !== null && (
          <p className={app.mismatch ? "warn-text" : "detail__muted"}>
            Windows reports {formatBytes(app.registryEstimateBytes, { units })}; Strata measured {formatBytes(app.totalBytes, { units })}
            {app.mismatch ? " — the registry estimate is far off (data and caches are usually not counted there)." : "."}
          </p>
        )}
        <dl className="kv">
          {[...byKind.entries()].map(([k, b]) => (
            <div key={k} className="kv__pair">
              <dt>{KIND_TEXT[k]}</dt>
              <dd>{formatBytes(b, { units })}</dd>
            </div>
          ))}
        </dl>
        <table className="table">
          <caption className="visually-hidden">Locations of {app.name}</caption>
          <thead>
            <tr>
              <th scope="col">Location</th>
              <th scope="col">Kind</th>
              <th scope="col">Safety</th>
              <th scope="col" className="num">
                Size
              </th>
            </tr>
          </thead>
          <tbody>
            {app.locations.map((l) => (
              <tr key={l.path}>
                <th scope="row" className="cell-path">
                  {l.entryId !== null ? (
                    <button
                      type="button"
                      className="linkish"
                      onClick={() => {
                        const s = useApp.getState();
                        if (s.volumeId === l.volumeId && l.entryId !== null) {
                          s.select([l.entryId], l.entryId);
                          s.setPane("detail", true);
                        }
                      }}
                    >
                      {l.path}
                    </button>
                  ) : (
                    l.path
                  )}
                </th>
                <td>{KIND_TEXT[l.kind]}</td>
                <td>
                  <SafetyBadge tier={l.safety} />
                </td>
                <td className="num">{formatBytes(l.bytes, { units })}</td>
              </tr>
            ))}
          </tbody>
        </table>
        <div className="toolbar">
          <button
            type="button"
            className="btn"
            disabled={!canClean || app.cacheBytes === 0}
            title={app.cacheBytes === 0 ? "No safe caches found." : canClean ? undefined : "Not available in this build."}
            onClick={() => {
              features.insights.queueAppCaches(app.id).then(reportAdd, (e: unknown) => {
                useApp.getState().notify(errorMessage(e));
              });
            }}
          >
            Clean caches ({formatBytes(app.cacheBytes, { units })})
          </button>
          {app.running && app.cacheBytes > 0 && <span className="warn-text">{app.name} is running; close it before cleaning its caches.</span>}
          <button
            type="button"
            className="btn"
            disabled={!canUninstall || !app.canUninstall}
            title={!app.canUninstall ? "This app has no registered uninstaller." : canUninstall ? undefined : "Not available in this build."}
            onClick={() => {
              features.tools.prepareTool({ kind: "uninstall", appId: app.id }).then(setPrompt, (e: unknown) => {
                useApp.getState().notify(errorMessage(e));
              });
            }}
          >
            Uninstall…
          </button>
        </div>
      </div>
      {prompt && (
        <ToolConfirm
          prompt={prompt}
          onCancel={() => {
            setPrompt(null);
          }}
          onRun={(p) => {
            setPrompt(null);
            features.tools
              .runTool(p.promptId, () => undefined)
              .then(
                () => {
                  useApp.getState().notify(`${app.name}’s uninstaller started.`);
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

function Orphans() {
  const features = useFeatures();
  const units = useSettings((s) => s.units);
  const [load, reload] = useLoad(() => features.insights.fetchOrphans(), [features]);
  return (
    <section aria-labelledby="orph-h" className="orphans">
      <h2 id="orph-h" className="section-title">
        Orphaned app data
      </h2>
      <p className="detail__muted">Folders left behind by apps that are no longer installed. They are marked Careful: check before removing.</p>
      <LoadState load={load} feature="Orphaned app data" command="apps_orphans" onRetry={reload}>
        {(rows) =>
          rows.length === 0 ? (
            <p>No leftover folders found.</p>
          ) : (
            <table className="table">
              <caption className="visually-hidden">Orphaned app data</caption>
              <thead>
                <tr>
                  <th scope="col">Folder</th>
                  <th scope="col">Probably from</th>
                  <th scope="col">Last activity</th>
                  <th scope="col" className="num">
                    Size
                  </th>
                </tr>
              </thead>
              <tbody>
                {rows.map((o) => (
                  <tr key={o.path}>
                    <th scope="row">
                      <div className="cell-path">{o.path}</div>
                      <div className="detail__muted">{o.reason}</div>
                    </th>
                    <td>{o.guessedApp ?? "Unknown"}</td>
                    <td>{formatRelative(o.lastActivityMs)}</td>
                    <td className="num">{formatBytes(o.bytes, { units })}</td>
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
