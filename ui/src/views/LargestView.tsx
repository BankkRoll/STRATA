/**
 * Largest files and folders (SPEC §16.2): global or under the current
 * folder, top-N (default 1000) with combinable filters. Selecting a row
 * drives the shared selection, so the detail panel explains it.
 */
import { useState } from "react";
import { LoadState, SafetyBadge, Unavailable, ViewFrame, useCapability, useLoad } from "../components/feature";
import { useFeatures } from "../features";
import { formatBytes, formatCount, formatDate } from "../lib/format";
import { NO_LARGEST_FILTERS, type LargestEntry, type LargestFilters } from "../lib/insights";
import { CATEGORIES, categoryInfo } from "../lib/palette";
import type { Safety } from "../lib/types";
import { useServices } from "../services";
import { useApp } from "../store/app";
import { useInsights } from "../store/insights";
import { useSettings } from "../store/settings";

const PAGE = 200;
const SIZE_STEPS: readonly [number, string][] = [
  [0, "Any size"],
  [1 << 20, "≥ 1 MB"],
  [100 << 20, "≥ 100 MB"],
  [1 << 30, "≥ 1 GB"],
  [10 * 2 ** 30, "≥ 10 GB"],
];

/**
 * Parses the extension filter field ("mp4, .mkv iso") into normalized
 * extensions.
 *
 * @param text - User input.
 * @returns Lowercase extensions without dots, deduplicated.
 */
export function parseExtensions(text: string): string[] {
  return [
    ...new Set(
      text
        .split(/[\s,;]+/)
        .map((t) => t.trim().replace(/^\*?\./, "").toLowerCase())
        .filter((t) => t.length > 0),
    ),
  ];
}

/** Largest files view. */
export function LargestView() {
  const features = useFeatures();
  const services = useServices();
  const has = useCapability("insights_largest");
  const volumeId = useApp((s) => s.volumeId) as string;
  const root = useApp((s) => s.path[s.path.length - 1] ?? null);
  const atRoot = useApp((s) => s.path.length <= 1);
  const sizeMode = useApp((s) => s.sizeMode);
  const selection = useApp((s) => s.selection);
  const filters = useInsights((s) => s.largest);
  const setFilters = useInsights((s) => s.setLargest);
  const units = useSettings((s) => s.units);
  const [kind, setKind] = useState<"files" | "folders">("files");
  const [underFolder, setUnderFolder] = useState(false);
  const [extText, setExtText] = useState(filters.extensions.join(", "));
  const [shown, setShown] = useState(PAGE);
  const scope = underFolder ? root : null;
  const [load, reload] = useLoad(
    () => features.insights.fetchLargest({ volumeId, scope, kind, limit: 1000, sizeMode, filters }),
    [features, volumeId, scope, kind, sizeMode, JSON.stringify(filters)],
    has,
  );

  if (!has) {
    return (
      <ViewFrame title="Largest files">
        <Unavailable feature="Largest files" command="insights_largest">
          The biggest files and folders on the volume, filterable by type, category, age and safety.
        </Unavailable>
      </ViewFrame>
    );
  }

  const update = (patch: Partial<LargestFilters>) => {
    setShown(PAGE);
    setFilters({ ...filters, ...patch });
  };
  const toggleSafety = (t: Safety, on: boolean) => {
    update({ safety: on ? [...filters.safety, t] : filters.safety.filter((x) => x !== t) });
  };

  return (
    <ViewFrame
      title="Largest files"
      lead={underFolder && !atRoot ? "Under the current folder." : "Across the whole volume."}
      actions={
        <button
          type="button"
          className="btn btn--small"
          onClick={() => {
            setFilters(NO_LARGEST_FILTERS);
            setExtText("");
          }}
        >
          Clear filters
        </button>
      }
    >
      <form
        className="filters"
        aria-label="Filters"
        onSubmit={(e) => {
          e.preventDefault();
          update({ extensions: parseExtensions(extText) });
        }}
      >
        <fieldset className="segmented-field">
          <legend className="visually-hidden">Show</legend>
          {(["files", "folders"] as const).map((k) => (
            <label key={k}>
              <input
                type="radio"
                name="kind"
                checked={kind === k}
                onChange={() => {
                  setKind(k);
                }}
              />
              {k === "files" ? "Files" : "Folders"}
            </label>
          ))}
        </fieldset>
        <label>
          <input
            type="checkbox"
            checked={underFolder}
            disabled={atRoot}
            onChange={(e) => {
              setUnderFolder(e.target.checked);
            }}
          />
          Only under the current folder
        </label>
        <label>
          Extensions{" "}
          <input
            type="text"
            className="input"
            value={extText}
            placeholder="mp4, iso"
            onChange={(e) => {
              setExtText(e.target.value);
            }}
            onBlur={() => {
              update({ extensions: parseExtensions(extText) });
            }}
          />
        </label>
        <label>
          Category{" "}
          <select
            className="select"
            value={filters.categories[0] ?? ""}
            onChange={(e) => {
              update({ categories: e.target.value === "" ? [] : [Number(e.target.value)] });
            }}
          >
            <option value="">Any</option>
            {CATEGORIES.map((c) => (
              <option key={c.id} value={c.id}>
                {c.label}
              </option>
            ))}
          </select>
        </label>
        <label>
          Size{" "}
          <select
            className="select"
            value={filters.minBytes}
            onChange={(e) => {
              update({ minBytes: Number(e.target.value) });
            }}
          >
            {SIZE_STEPS.map(([v, l]) => (
              <option key={v} value={v}>
                {l}
              </option>
            ))}
          </select>
        </label>
        <label>
          Age{" "}
          <select
            className="select"
            value={filters.untouchedForDays !== null ? `old:${filters.untouchedForDays}` : filters.modifiedWithinDays !== null ? `new:${filters.modifiedWithinDays}` : ""}
            onChange={(e) => {
              const [k, n] = e.target.value.split(":");
              update({ modifiedWithinDays: k === "new" ? Number(n) : null, untouchedForDays: k === "old" ? Number(n) : null });
            }}
          >
            <option value="">Any time</option>
            <option value="new:1">Modified in the last 24 h</option>
            <option value="new:7">Modified in the last 7 days</option>
            <option value="new:30">Modified in the last 30 days</option>
            <option value="old:365">Untouched for 1 year+</option>
          </select>
        </label>
        <fieldset className="inline-checks">
          <legend>Safety</legend>
          {(["safe", "probably", "careful", "never"] as const).map((t) => (
            <label key={t}>
              <input
                type="checkbox"
                checked={filters.safety.includes(t)}
                onChange={(e) => {
                  toggleSafety(t, e.target.checked);
                }}
              />
              <SafetyBadge tier={t} />
            </label>
          ))}
        </fieldset>
        <button type="submit" className="visually-hidden">
          Apply
        </button>
      </form>
      <LoadState load={load} feature="Largest files" command="insights_largest" onRetry={reload}>
        {(r) =>
          r.entries.length === 0 ? (
            <div className="state state--quiet">
              <h2>Nothing matches</h2>
              <p>Try clearing some filters.</p>
            </div>
          ) : (
            <>
              <p className="detail__muted" role="status">
                Showing {formatCount(Math.min(shown, r.entries.length))} of {formatCount(r.entries.length)}
                {r.matched > r.entries.length ? ` (top ${formatCount(r.entries.length)} of ${formatCount(r.matched)} matches)` : ""}
              </p>
              <table className="table table--select">
                <caption className="visually-hidden">Largest {kind}</caption>
                <thead>
                  <tr>
                    <th scope="col">Name</th>
                    <th scope="col" className="num">
                      Size
                    </th>
                    <th scope="col">Modified</th>
                    <th scope="col">Category</th>
                    <th scope="col">Safety</th>
                    <th scope="col">
                      <span className="visually-hidden">Actions</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {r.entries.slice(0, shown).map((e) => (
                    <LargestRow key={e.id} e={e} selected={selection.includes(e.id)} volumeId={volumeId} units={units} bus={services.bus} />
                  ))}
                </tbody>
              </table>
              {shown < r.entries.length && (
                <button
                  type="button"
                  className="btn"
                  onClick={() => {
                    setShown((n) => n + PAGE);
                  }}
                >
                  Show {formatCount(Math.min(PAGE, r.entries.length - shown))} more
                </button>
              )}
            </>
          )
        }
      </LoadState>
    </ViewFrame>
  );
}

function LargestRow({
  e,
  selected,
  volumeId,
  units,
  bus,
}: {
  e: LargestEntry;
  selected: boolean;
  volumeId: string;
  units: "binary" | "si";
  bus: ReturnType<typeof useServices>["bus"];
}) {
  const target = { volumeId, ids: [e.id] };
  const av = bus.availability({ type: "addToCleanup", target });
  const never = e.safety === "never";
  const reason = never ? "Protected (Never tier): Strata won’t delete this." : av.enabled ? null : av.reason;
  return (
    <tr aria-selected={selected} className={selected ? "is-selected" : undefined}>
      <th scope="row" className="cell-main">
        <button
          type="button"
          className="linkish cell-name"
          aria-label={`${e.name}, ${formatBytes(e.bytes, { units })}: show details`}
          onClick={() => {
            useApp.getState().select([e.id], e.id);
            useApp.getState().setPane("detail", true);
          }}
        >
          {e.name}
        </button>
        <div className="cell-path">{e.path}</div>
      </th>
      <td className="num">{formatBytes(e.bytes, { units })}</td>
      <td>{formatDate(e.modifiedMs)}</td>
      <td>
        <span className="cat-dot" style={{ background: categoryInfo(e.category).light }} aria-hidden="true" />
        {categoryInfo(e.category).label}
      </td>
      <td>
        <SafetyBadge tier={e.safety} />
      </td>
      <td>
        <button
          type="button"
          className="btn btn--small"
          aria-disabled={reason !== null}
          title={reason ?? "Add to cleanup queue"}
          aria-label={`Add ${e.name} to cleanup`}
          onClick={() => {
            if (reason === null) void bus.dispatch({ type: "addToCleanup", target });
            else useApp.getState().notify(reason);
          }}
        >
          Add to cleanup
        </button>
        {reason !== null && <span className="visually-hidden">{reason}</span>}
      </td>
    </tr>
  );
}
