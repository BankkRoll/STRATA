/**
 * Categories (SPEC §12.5, §16.2): totals per category with the fixed palette
 * and patterns, and drill-through to the map (filtered), Largest files, or a
 * top contributor.
 */
import { useState } from "react";
import { BarList } from "../components/charts";
import { LoadState, Unavailable, ViewFrame, useCapability, useLoad } from "../components/feature";
import { useFeatures } from "../features";
import { useDark } from "../lib/hooks";
import { formatBytes, formatCount, formatPercent } from "../lib/format";
import { NO_LARGEST_FILTERS } from "../lib/insights";
import { categoryInfo } from "../lib/palette";
import { useApp } from "../store/app";
import { useInsights } from "../store/insights";
import { useSettings } from "../store/settings";

/** Categories view. */
export function CategoriesView() {
  const features = useFeatures();
  const has = useCapability("insights_categories");
  const volumeId = useApp((s) => s.volumeId) as string;
  const root = useApp((s) => s.path[s.path.length - 1] ?? null);
  const sizeMode = useApp((s) => s.sizeMode);
  const units = useSettings((s) => s.units);
  const patterns = useSettings((s) => s.patterns);
  const dark = useDark();
  const [open, setOpen] = useState<number | null>(null);
  const [load, reload] = useLoad(() => features.insights.fetchCategories(volumeId, root, sizeMode), [features, volumeId, root, sizeMode], has);
  if (!has) {
    return (
      <ViewFrame title="Categories">
        <Unavailable feature="Categories" command="insights_categories">
          Space per category (system, apps, caches, media…) with drill-through.
        </Unavailable>
      </ViewFrame>
    );
  }
  return (
    <ViewFrame title="Categories" lead="Under the current folder. Choose a category to see what fills it.">
      <LoadState load={load} feature="Categories" command="insights_categories" onRetry={reload}>
        {(rows) => {
          const sorted = [...rows].filter((r) => r.bytes > 0).sort((a, b) => b.bytes - a.bytes);
          const total = sorted.reduce((a, r) => a + r.bytes, 0);
          const cur = sorted.find((r) => r.category === open) ?? null;
          return (
            <>
              <BarList
                label="Space by category"
                selectHint="Show details of"
                bars={sorted.map((r) => {
                  const c = categoryInfo(r.category);
                  return {
                    key: String(r.category),
                    label: c.label,
                    value: r.bytes,
                    valueText: `${formatBytes(r.bytes, { units })} · ${formatPercent(total === 0 ? 0 : r.bytes / total)}`,
                    color: dark ? c.dark : c.light,
                    ...(patterns ? { className: `pattern pattern--${c.pattern}` } : {}),
                  };
                })}
                onSelect={(k) => {
                  setOpen(Number(k));
                }}
              />
              {cur && (
                <section className="callout" aria-labelledby="cat-h">
                  <h2 id="cat-h">
                    {categoryInfo(cur.category).label}: {formatBytes(cur.bytes, { units })} in {formatCount(cur.files)} files
                  </h2>
                  <div className="toolbar">
                    <button
                      type="button"
                      className="btn"
                      onClick={() => {
                        const s = useApp.getState();
                        s.setFilters({ ...s.filters, categories: [cur.category] });
                        s.setView(s.lastVisual);
                      }}
                    >
                      Show only this on the map
                    </button>
                    <button
                      type="button"
                      className="btn"
                      onClick={() => {
                        useInsights.getState().setLargest({ ...NO_LARGEST_FILTERS, categories: [cur.category] });
                        useApp.getState().setView("largest");
                      }}
                    >
                      Largest files in this category
                    </button>
                  </div>
                  <h3>Biggest contributors</h3>
                  <ul className="plain">
                    {cur.top.map((t) => (
                      <li key={t.id}>
                        <button
                          type="button"
                          className="linkish"
                          onClick={() => {
                            useApp.getState().select([t.id], t.id);
                            useApp.getState().setPane("detail", true);
                          }}
                        >
                          {t.name}
                        </button>{" "}
                        — {formatBytes(t.bytes, { units })} <span className="cell-path">{t.path}</span>
                      </li>
                    ))}
                  </ul>
                </section>
              )}
            </>
          );
        }}
      </LoadState>
    </ViewFrame>
  );
}
