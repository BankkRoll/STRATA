/**
 * Build-time rendering of the benchmark section from `site/data/benchmarks.json`.
 * `vite.config.ts` calls {@link renderBenchmarks} and splices the HTML into
 * `index.html`, so the numbers ship as static markup with no runtime code.
 *
 * Schema of `benchmarks.json`:
 *
 * ```jsonc
 * {
 *   "methodologyUrl": string,          // link in the footnote
 *   "machine": string,                 // reference machine, shown in the footnote
 *   "metrics": [{                      // measured-vs-target bars, lower is better
 *     "label": string,
 *     "detail": string,                // workload, e.g. "5M names"
 *     "measured": number,              // slowest measured value, same unit as target
 *     "measuredText": string,          // as printed, e.g. "0.12–0.29 µs"
 *     "target": number,
 *     "targetText": string,
 *     "note": string | null            // numbered footnote for this row
 *   }],
 *   "stats": [{ "value": string, "label": string, "detail": string }],
 *   "comparison": {
 *     "release": string | null,        // Strata version measured, e.g. "v1.0.0"
 *     "measuredOn": string | null,     // ISO date of the run
 *     "machine": string | null,
 *     "methodology": string[],         // one bullet each
 *     "results": [{                    // empty array: the head-to-head block is not rendered
 *       "metric": string,              // e.g. "Full scan, system volume"
 *       "detail": string,              // e.g. "1.2M entries, cold cache"
 *       "unit": string,                // e.g. "s"
 *       "lowerIsBetter": boolean,
 *       "values": [{ "tool": string, "version": string, "value": number }]
 *     }]
 *   }
 * }
 * ```
 *
 * Adding head-to-head numbers is a data-only change: fill `comparison.results`
 * (and `release`, `measuredOn`, `machine`) and the block appears on the next
 * build.
 */
import { readFileSync } from "node:fs";

/** One measured-vs-target row. */
export interface Metric {
  label: string;
  detail: string;
  /** Slowest measured value, in the target's unit. */
  measured: number;
  measuredText: string;
  target: number;
  targetText: string;
  note: string | null;
}

/** One headline number. */
export interface Stat {
  value: string;
  label: string;
  detail: string;
}

/** One head-to-head measurement across tools. */
export interface ComparisonResult {
  metric: string;
  detail: string;
  unit: string;
  lowerIsBetter: boolean;
  values: { tool: string; version: string; value: number }[];
}

/** The whole data file. */
export interface BenchmarkData {
  methodologyUrl: string;
  machine: string;
  metrics: Metric[];
  stats: Stat[];
  comparison: {
    release: string | null;
    measuredOn: string | null;
    machine: string | null;
    methodology: string[];
    results: ComparisonResult[];
  };
}

/** Rendered fragments, keyed by their `<!--bench:*-->` placeholder. */
export interface BenchmarkHtml {
  chart: string;
  notes: string;
  stats: string;
  versus: string;
  machine: string;
  link: string;
}

const esc = (s: string): string => s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

function share(fraction: number): string {
  if (fraction < 0.001) return "&lt;0.1% of budget";
  if (fraction < 0.1) return `${(fraction * 100).toFixed(1)}% of budget`;
  return `${Math.round(fraction * 100)}% of budget`;
}

/** Bar width: a sliver stays visible however small the share; the text carries the value. */
const width = (fraction: number): string => `${Math.max(0.8, Math.min(100, fraction * 100)).toFixed(2)}%`;

function bars(data: BenchmarkData): string {
  const noted = data.metrics.filter((m) => m.note);
  const rows = data.metrics.map((m, i) => {
    const fraction = Math.max(0, m.measured / m.target);
    const n = m.note ? noted.indexOf(m) + 1 : 0;
    const sup = n ? `<sup><a href="#bench-note-${n}" aria-label="Note ${n}">${n}</a></sup>` : "";
    return `<li class="bars__row" style="--i:${i}">
  <div class="bars__label"><span class="bars__name">${esc(m.label)}${sup}</span><span class="bars__detail">${esc(m.detail)}</span></div>
  <div class="bars__track" aria-hidden="true"><span class="bars__fill" style="--w:${width(fraction)}"></span><span class="bars__target"></span></div>
  <div class="bars__values"><span class="bars__measured">${esc(m.measuredText)}</span><span class="bars__goal">target ${esc(m.targetText)}</span><span class="bars__share">${share(fraction)}</span></div>
</li>`;
  });
  return `<ol class="bars">${rows.join("\n")}</ol>`;
}

function notes(data: BenchmarkData): string {
  return data.metrics
    .filter((m) => m.note)
    .map((m, i) => `<li id="bench-note-${i + 1}">${esc(m.note ?? "")}</li>`)
    .join("");
}

function stats(data: BenchmarkData): string {
  return data.stats
    .map(
      (s) =>
        `<li class="stat reveal"><span class="stat__value">${esc(s.value)}</span><span class="stat__label">${esc(s.label)}</span><span class="stat__detail">${esc(s.detail)}</span></li>`,
    )
    .join("\n");
}

function versus(c: BenchmarkData["comparison"]): string {
  if (c.results.length === 0) return "";
  const meta = [c.release ? `Strata ${c.release}` : null, c.measuredOn, c.machine].filter((x): x is string => !!x);
  const metrics = c.results.map((r) => {
    const vals = r.values.map((v) => v.value);
    const best = r.lowerIsBetter ? Math.min(...vals) : Math.max(...vals);
    const max = Math.max(...vals, Number.MIN_VALUE);
    const rows = r.values.map(
      (v) => `<li class="h2h__row${v.value === best ? " is-best" : ""}">
  <span class="h2h__tool">${esc(v.tool)} <span class="h2h__ver">${esc(v.version)}</span></span>
  <span class="bars__track" aria-hidden="true"><span class="bars__fill" style="--w:${width(v.value / max)}"></span></span>
  <span class="h2h__value">${esc(String(v.value))} ${esc(r.unit)}</span>
</li>`,
    );
    return `<div class="h2h__metric">
  <p class="h2h__name">${esc(r.metric)} <span class="bars__detail">${esc(r.detail)} · ${r.lowerIsBetter ? "lower" : "higher"} is better</span></p>
  <ul class="h2h__rows">${rows.join("")}</ul>
</div>`;
  });
  const method = c.methodology.length > 0 ? `<ul class="h2h__method">${c.methodology.map((m) => `<li>${esc(m)}</li>`).join("")}</ul>` : "";
  return `<div class="card h2h reveal">
  <div class="card__head"><h3 class="card__title">Head-to-head</h3>${meta.length > 0 ? `<p class="h2h__meta">Measured on release · ${meta.map(esc).join(" · ")}</p>` : ""}</div>
  ${metrics.join("\n")}
  ${method}
</div>`;
}

/**
 * Reads and renders the benchmark data file.
 *
 * @param file - Path to `benchmarks.json`.
 * @returns HTML fragments for each placeholder.
 */
export function renderBenchmarks(file: string): BenchmarkHtml {
  const data = JSON.parse(readFileSync(file, "utf8")) as BenchmarkData;
  return {
    chart: bars(data),
    notes: notes(data),
    stats: stats(data),
    versus: versus(data.comparison),
    machine: esc(data.machine),
    link: esc(data.methodologyUrl),
  };
}
