/**
 * Build-time rendering of the benchmark section from `site/data/benchmarks.json`.
 * `vite.config.ts` calls {@link renderBenchmarks} and splices the fragments
 * into the `<!--bench:*-->` placeholders of `index.html`, so every number
 * ships as static markup. The page script only adds sorting and copy buttons.
 *
 * Responsibilities:
 * - Validate the data file, so a typo fails the build instead of the page.
 * - Render the headline stats, the log-scale range chart, one panel per area,
 *   the full results table, footnotes, methodology and reproduction commands.
 * - Render the competitor head-to-head only when measured results exist.
 *
 * Every number in the data file must trace to `docs/BENCHMARKS.md`.
 *
 * Schema of `benchmarks.json`:
 *
 * ```jsonc
 * {
 *   "methodologyUrl": string,          // link to docs/BENCHMARKS.md
 *   "machine": string,                 // reference machine
 *   "noise": string,                   // run-to-run noise statement
 *   "methodology": string[],           // one bullet each
 *   "reproduce": [{ "area": string, "command": string }], // "\n" separates commands
 *   "areas": [{ "id": string, "label": string, "summary": string }], // display order
 *   "metrics": [{
 *     "id": string,                    // unique, stable
 *     "area": string,                  // an areas[].id
 *     "group": string,                 // sub-heading in BENCHMARKS.md, e.g. "MFT scanner"
 *     "label": string,
 *     "detail": string,                // workload, e.g. "5M names"
 *     "text": string,                  // value as printed, without unit: "155–291", "≤ 1.1", "~3"
 *     "unit": string,                  // "ms", "B", "M records/s", ...
 *     "min": number | null,            // fastest run; null when only an upper bound is known
 *     "max": number,                   // slowest run (same as min for a single value)
 *     "aside"?: string,                // secondary reading, e.g. "3.4–6.4 M records/s"
 *     "lowerIsBetter": boolean,
 *     "target"?: number,               // numeric target, in `unit`; enables the range chart
 *     "targetText"?: string,           // target as printed; may exist without a number
 *     "note"?: string                  // numbered footnote
 *   }],
 *   "stats": [{ "value": string, "unit": string, "label": string, "detail": string }],
 *   "comparison": {
 *     "release": string | null,        // Strata version measured, e.g. "v1.0.0"
 *     "measuredOn": string | null,     // ISO date of the run
 *     "machine": string | null,
 *     "methodology": string[],
 *     "results": [{                    // empty: the head-to-head is not rendered at all
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
 * Publishing competitor measurements is a data-only change: fill
 * `comparison.results` (with `release`, `measuredOn` and `machine`) and the
 * head-to-head appears on the next build.
 */
import { readFileSync } from "node:fs";

// -----------------------------------------------------------------------------
// Data model
// -----------------------------------------------------------------------------

/** One benchmark area (a panel on the page). */
export interface Area {
  id: string;
  label: string;
  summary: string;
}

/** One measured result. */
export interface Metric {
  id: string;
  area: string;
  /** Sub-heading of the results table in `docs/BENCHMARKS.md`. */
  group: string;
  label: string;
  /** Workload, e.g. "5M names". */
  detail: string;
  /** Value exactly as printed in the docs, without the unit. */
  text: string;
  unit: string;
  /** Fastest run; `null` when only an upper bound was recorded. */
  min: number | null;
  /** Slowest run. */
  max: number;
  /** Secondary reading of the same run. */
  aside?: string;
  lowerIsBetter: boolean;
  /** Numeric target in `unit`; metrics with one appear in the range chart. */
  target?: number;
  targetText?: string;
  note?: string;
}

/** One headline figure. */
export interface Stat {
  value: string;
  unit: string;
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
  noise: string;
  methodology: string[];
  reproduce: { area: string; command: string }[];
  areas: Area[];
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
  stats: string;
  range: string;
  panels: string;
  versus: string;
  table: string;
  notes: string;
  method: string;
  reproduce: string;
  machine: string;
  noise: string;
  link: string;
  count: string;
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

const esc = (s: string): string => s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

/** Thin space between a number and its unit; "%" sticks to the number. */
const withUnit = (text: string, unit: string): string => (unit === "%" ? `${esc(text)}%` : `${esc(text)}<span class="u"> ${esc(unit)}</span>`);

/**
 * Share of the target budget the worst run used: slowest run for
 * lower-is-better metrics, lowest run for higher-is-better ones.
 */
function budgetUsed(m: Metric): number | null {
  if (m.target === undefined) return null;
  if (m.lowerIsBetter) return m.max / m.target;
  const worst = m.min ?? m.max;
  return worst > 0 ? m.target / worst : null;
}

/** Thousands separators, then two significant figures above 1,000 so ±30% noise isn't overstated. */
function ratioText(r: number): string {
  if (r < 10) return r.toFixed(1).replace(/\.0$/, "");
  if (r < 1000) return Math.round(r).toLocaleString("en-US");
  const p = 10 ** (Math.floor(Math.log10(r)) - 1);
  return (Math.round(r / p) * p).toLocaleString("en-US");
}

/**
 * Margin against target in words, e.g. "16× under target" or "7% under target".
 *
 * @returns `null` for metrics without a numeric target.
 */
export function margin(m: Metric): { text: string; short: string; ratio: number } | null {
  const used = budgetUsed(m);
  if (used === null || used <= 0) return null;
  const ratio = 1 / used;
  const word = m.lowerIsBetter ? "under" : "over";
  const amount = ratio >= 2 ? `${ratioText(ratio)}×` : `${Math.round(m.lowerIsBetter ? (1 - used) * 100 : (ratio - 1) * 100)}%`;
  const short = `${amount} ${word}`;
  return { text: `${short} target`, short, ratio };
}

/** Log-scale domain of the range chart, as a share of target: 0.01% … 100%. */
const LOG_MIN = -4;
const LOG_MAX = 0;
const TICKS = [
  { at: -4, label: "0.01%" },
  { at: -3, label: "0.1%" },
  { at: -2, label: "1%" },
  { at: -1, label: "10%" },
  { at: 0, label: "Target" },
];

const logPos = (fraction: number): number => {
  const v = Math.log10(Math.max(10 ** LOG_MIN, Math.min(10 ** LOG_MAX, fraction)));
  return ((v - LOG_MIN) / (LOG_MAX - LOG_MIN)) * 100;
};

const pct = (n: number): string => `${n.toFixed(2)}%`;

function validate(data: BenchmarkData): void {
  const areas = new Set(data.areas.map((a) => a.id));
  const ids = new Set<string>();
  for (const m of data.metrics) {
    const where = `benchmarks.json: metric "${m.id}"`;
    if (ids.has(m.id)) throw new Error(`${where} is duplicated`);
    ids.add(m.id);
    if (!areas.has(m.area)) throw new Error(`${where} has unknown area "${m.area}"`);
    if (!Number.isFinite(m.max) || (m.min !== null && (!Number.isFinite(m.min) || m.min > m.max))) throw new Error(`${where} has an invalid range`);
    if (m.target !== undefined && !(m.target > 0)) throw new Error(`${where} has a non-positive target`);
    if (m.target !== undefined && !m.targetText) throw new Error(`${where} has a target without targetText`);
  }
  for (const r of data.comparison.results) {
    if (r.values.length < 2) throw new Error(`benchmarks.json: comparison "${r.metric}" needs at least two tools`);
  }
}

// -----------------------------------------------------------------------------
// Fragments
// -----------------------------------------------------------------------------

/** Footnote numbers in metric order. */
function footnotes(data: BenchmarkData): Map<string, number> {
  const out = new Map<string, number>();
  for (const m of data.metrics) if (m.note) out.set(m.id, out.size + 1);
  return out;
}

const sup = (n: number | undefined): string => (n ? `<sup class="fn"><a href="#bench-note-${n}" aria-label="Note ${n}">${n}</a></sup>` : "");

function stats(data: BenchmarkData): string {
  return data.stats
    .map(
      (s) => `<li class="stat">
  <span class="stat__value">${esc(s.value)}<span class="stat__unit">${esc(s.unit)}</span></span>
  <span class="stat__label">${esc(s.label)}</span>
  <span class="stat__detail">${esc(s.detail)}</span>
</li>`,
    )
    .join("\n");
}

function range(data: BenchmarkData, notes: Map<string, number>): string {
  const row = (m: Metric): string => {
    const target = m.target as number;
    const share = (v: number) => (m.lowerIsBetter ? v / target : target / v);
    const hi = share(m.lowerIsBetter ? m.max : (m.min ?? m.max));
    const lo = m.min === null ? null : share(m.lowerIsBetter ? m.min : m.max);
    const a = logPos(lo ?? hi);
    const b = logPos(hi);
    const kind = m.min === null ? " is-bound" : a === b ? " is-point" : "";
    const mg = margin(m);
    return `<li class="rng__row">
  <div class="rng__label"><span class="rng__name">${esc(m.label)}${sup(notes.get(m.id))}</span><span class="rng__detail">${esc(m.detail)}</span></div>
  <div class="rng__plot" aria-hidden="true">${TICKS.map((t) => `<i class="rng__tick" style="--x:${pct(((t.at - LOG_MIN) / (LOG_MAX - LOG_MIN)) * 100)}"></i>`).join("")}<span class="rng__bar${kind}" style="--a:${pct(Math.min(a, b))};--b:${pct(Math.max(a, b))}"></span></div>
  <div class="rng__value"><span class="rng__measured">${withUnit(m.text, m.unit)}</span>${mg ? `<span class="rng__margin">${esc(mg.text)}</span>` : ""}<span class="rng__target">target ${esc(m.targetText ?? "")}</span></div>
</li>`;
  };
  const groups = data.areas
    .map((area) => {
      const rows = data.metrics.filter((m) => m.area === area.id && m.target !== undefined);
      if (rows.length === 0) return "";
      return `<li class="rng__group"><span class="rng__group-title" id="rng-${area.id}">${esc(area.label)}</span><ol class="rng__rows" aria-labelledby="rng-${area.id}">${rows.map(row).join("\n")}</ol></li>`;
    })
    .join("\n");
  const axis = TICKS.map((t) => `<span style="--x:${pct(((t.at - LOG_MIN) / (LOG_MAX - LOG_MIN)) * 100)}">${t.label}</span>`).join("");
  return `<div class="rng__axis" aria-hidden="true"><span class="rng__axis-title">Budget used</span><div class="rng__scale">${axis}</div><span class="rng__axis-title rng__axis-title--end">Result</span></div>
<ol class="rng" aria-label="Measured results against their targets, by area">${groups}</ol>`;
}

const PANEL_EXTRAS = 4;

function panels(data: BenchmarkData, notes: Map<string, number>): string {
  return data.areas
    .map((area) => {
      const metrics = data.metrics.filter((m) => m.area === area.id);
      const targeted = metrics.filter((m) => m.target !== undefined);
      const others = metrics.filter((m) => m.target === undefined);
      const shown = others.slice(0, PANEL_EXTRAS);
      const more = others.length - shown.length;
      const targetRows = targeted
        .map((m) => {
          const used = budgetUsed(m) ?? 0;
          const mg = margin(m);
          return `<tr>
  <th scope="row"><span class="pt__name">${esc(m.label)}${sup(notes.get(m.id))}</span><span class="pt__bar" aria-hidden="true"><i style="--w:${pct(Math.min(100, used * 100))}"></i></span></th>
  <td class="num">${withUnit(m.text, m.unit)}</td>
  <td class="num pt__target">${esc(m.targetText ?? "")}</td>
  <td class="num pt__margin">${mg ? esc(mg.short) : "—"}</td>
</tr>`;
        })
        .join("");
      const otherRows = shown
        .map((m) => `<tr><th scope="row">${esc(m.label)}${sup(notes.get(m.id))}</th><td class="num">${withUnit(m.text, m.unit)}</td></tr>`)
        .join("");
      return `<article class="panel" aria-labelledby="panel-${area.id}">
  <header class="panel__head">
    <h4 id="panel-${area.id}" class="panel__title">${esc(area.label)}</h4>
    <span class="panel__count">${metrics.length} results</span>
    <p class="panel__summary">${esc(area.summary)}</p>
  </header>
  <table class="ptable ptable--targets">
    <caption class="visually-hidden">${esc(area.label)}: measured against target</caption>
    <thead><tr><th scope="col">Against target</th><th scope="col" class="num">Measured</th><th scope="col" class="num">Target</th><th scope="col" class="num">Margin</th></tr></thead>
    <tbody>${targetRows}</tbody>
  </table>
  ${
    shown.length > 0
      ? `<table class="ptable">
    <caption class="visually-hidden">${esc(area.label)}: other results</caption>
    <thead><tr><th scope="col">Also measured</th><th scope="col" class="num">Result</th></tr></thead>
    <tbody>${otherRows}</tbody>
  </table>`
      : ""
  }
  ${more > 0 ? `<a class="panel__more" href="#results" data-area-link="${esc(area.id)}">${more} more in the full table<span aria-hidden="true"> ↓</span></a>` : ""}
</article>`;
    })
    .join("\n");
}

function table(data: BenchmarkData, notes: Map<string, number>): string {
  const order = new Map(data.areas.map((a, i) => [a.id, i]));
  const rows = data.metrics.map((m, i) => {
    const area = data.areas.find((a) => a.id === m.area);
    const mg = margin(m);
    const rank = (order.get(m.area) ?? 0) * 1000 + i;
    return `<tr data-area="${rank}" data-metric="${esc(m.label.toLowerCase())}" data-margin="${mg ? mg.ratio.toFixed(4) : "-1"}">
  <td class="rt__area">${esc(area?.label ?? m.area)}<span class="rt__group">${esc(m.group)}</span></td>
  <th scope="row" class="rt__metric">${esc(m.label)}${sup(notes.get(m.id))}<span class="rt__detail">${esc(m.detail)}${m.aside ? ` · ${esc(m.aside)}` : ""}</span></th>
  <td class="num rt__value">${esc(m.text)}</td>
  <td class="rt__unit">${esc(m.unit)}</td>
  <td class="rt__target">${m.targetText ? esc(m.targetText) : '<span class="rt__none">—</span>'}</td>
  <td class="num rt__margin">${mg ? esc(mg.short) : '<span class="rt__none">—</span>'}</td>
</tr>`;
  });
  const sortable = (key: string, label: string, cls = "") =>
    `<th scope="col"${cls ? ` class="${cls}"` : ""} data-sort="${key}"${key === "area" ? ' aria-sort="ascending"' : ""}><button type="button" class="sort">${label}<svg viewBox="0 0 10 10" aria-hidden="true"><path d="M2.5 4 5 1.5 7.5 4M2.5 6 5 8.5 7.5 6" /></svg></button></th>`;
  return `<table class="rt" data-results>
  <caption class="visually-hidden">All ${data.metrics.length} benchmark results. Column headers with buttons sort the table.</caption>
  <thead><tr>${sortable("area", "Area")}${sortable("metric", "Metric")}<th scope="col" class="num">Result</th><th scope="col"><span class="visually-hidden">Unit</span></th><th scope="col">Target</th>${sortable("margin", "Margin", "num")}</tr></thead>
  <tbody>${rows.join("\n")}</tbody>
</table>`;
}

function notesHtml(data: BenchmarkData, notes: Map<string, number>): string {
  return data.metrics
    .filter((m) => m.note)
    .map((m) => `<li id="bench-note-${notes.get(m.id)}"><span class="fn__label">${esc(m.label)}.</span> ${esc(m.note ?? "")}</li>`)
    .join("");
}

function method(data: BenchmarkData): string {
  return `<ul class="method">${data.methodology.map((m) => `<li>${esc(m)}</li>`).join("")}<li>Reference machine: ${esc(data.machine)}.</li><li>${esc(data.noise)}</li></ul>`;
}

function reproduce(data: BenchmarkData): string {
  return `<ul class="repro">${data.reproduce
    .map(
      (r) => `<li class="repro__item">
  <span class="repro__area">${esc(r.area)}</span>
  <div class="repro__cmd"><pre><code>${esc(r.command)}</code></pre><button type="button" class="copy" data-copy="${esc(r.command)}" aria-label="Copy command: ${esc(r.area)}">Copy</button></div>
</li>`,
    )
    .join("")}</ul>`;
}

function versus(c: BenchmarkData["comparison"]): string {
  if (c.results.length === 0) return "";
  const tools: string[] = [];
  const versions = new Map<string, string>();
  for (const r of c.results) {
    for (const v of r.values) {
      if (!tools.includes(v.tool)) tools.push(v.tool);
      if (!versions.has(v.tool)) versions.set(v.tool, v.version);
    }
  }
  tools.sort((a, b) => (a === "Strata" ? -1 : b === "Strata" ? 1 : 0));
  const meta = [c.release ? `Strata ${c.release}` : null, c.measuredOn, c.machine].filter((x): x is string => !!x);
  const head = tools
    .map((t) => `<th scope="col" class="${t === "Strata" ? "is-self" : ""}">${esc(t)}<span class="h2h__ver">${esc(versions.get(t) ?? "")}</span></th>`)
    .join("");
  const rows = c.results.map((r) => {
    const vals = r.values.map((v) => v.value);
    const best = r.lowerIsBetter ? Math.min(...vals) : Math.max(...vals);
    const max = Math.max(...vals);
    const cells = tools.map((t) => {
      const v = r.values.find((x) => x.tool === t);
      if (!v) return `<td class="h2h__cell is-missing" data-tool="${esc(t)}"><span class="rt__none">—</span></td>`;
      const isBest = v.value === best;
      const factor = best > 0 ? (r.lowerIsBetter ? v.value / best : best / v.value) : 1;
      const rel = isBest ? "Best" : `${factor < 10 ? factor.toFixed(1) : Math.round(factor)}× ${r.lowerIsBetter ? "slower" : "lower"}`;
      return `<td class="h2h__cell${isBest ? " is-best" : ""}${t === "Strata" ? " is-self" : ""}" data-tool="${esc(t)}">
  <span class="h2h__val">${withUnit(v.value.toLocaleString("en-US"), r.unit)}</span>
  <span class="h2h__bar" aria-hidden="true"><i style="--w:${pct(max > 0 ? Math.max(1, (v.value / max) * 100) : 0)}"></i></span>
  <span class="h2h__rel">${rel}</span>
</td>`;
    });
    return `<tr><th scope="row">${esc(r.metric)}<span class="rt__detail">${esc(r.detail)} · ${r.lowerIsBetter ? "lower" : "higher"} is better</span></th>${cells.join("")}</tr>`;
  });
  return `<div class="block h2h" id="head-to-head">
  <header class="block__head">
    <div><h3 class="block__title">Head-to-head</h3><p class="block__sub">Same machine, same volume, each tool at its public release.</p></div>
    ${meta.length > 0 ? `<p class="block__meta">${meta.map(esc).join(" · ")}</p>` : ""}
  </header>
  <div class="h2h__scroll"><table class="h2h__table">
    <caption class="visually-hidden">Strata compared with other disk-space tools</caption>
    <thead><tr><th scope="col">Metric</th>${head}</tr></thead>
    <tbody>${rows.join("\n")}</tbody>
  </table></div>
  ${c.methodology.length > 0 ? `<ul class="method method--compact">${c.methodology.map((m) => `<li>${esc(m)}</li>`).join("")}</ul>` : ""}
</div>`;
}

/**
 * Reads, validates and renders the benchmark data file.
 *
 * @param file - Path to `benchmarks.json`.
 * @returns HTML fragments for each placeholder.
 * @throws If the data file is inconsistent (unknown area, bad range, missing target text).
 */
export function renderBenchmarks(file: string): BenchmarkHtml {
  const data = JSON.parse(readFileSync(file, "utf8")) as BenchmarkData;
  validate(data);
  const notes = footnotes(data);
  return {
    stats: stats(data),
    range: range(data, notes),
    panels: panels(data, notes),
    versus: versus(data.comparison),
    table: table(data, notes),
    notes: notesHtml(data, notes),
    method: method(data),
    reproduce: reproduce(data),
    machine: esc(data.machine),
    noise: esc(data.noise),
    link: esc(data.methodologyUrl),
    count: String(data.metrics.length),
  };
}
