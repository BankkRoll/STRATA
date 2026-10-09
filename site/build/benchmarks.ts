/**
 * Build-time rendering of the benchmark section from `site/data/benchmarks.json`.
 * `vite.config.ts` calls {@link renderBenchmarks} and splices the fragments
 * into the `<!--bench:*-->` placeholders of `index.html`, so every number
 * ships as static markup. The page script only adds copy buttons.
 *
 * Responsibilities:
 * - Validate the data file, so a typo or an out-of-scale value fails the build
 *   instead of the page.
 * - Render the outcome groups: headline figures phrased as user outcomes, each
 *   with one chart (a range strip on a linear or log scale, or a stacked
 *   scaling chart) plus an equivalent table for assistive technology.
 * - Render the head-to-head against other tools only when measured results
 *   exist, grouped per volume: full scan time with the first run of the
 *   session marked, and peak memory.
 * - Render the measurement notes and reproduction commands.
 *
 * Every number in the data file must trace to `docs/BENCHMARKS.md`; each
 * outcome and chart row carries a `source` note saying where.
 *
 * Schema of `benchmarks.json`:
 *
 * ```jsonc
 * {
 *   "methodologyUrl": string,          // link to docs/BENCHMARKS.md
 *   "machine": string,                 // reference machine, generic hardware only
 *   "noise": string,                   // run-to-run noise statement
 *   "methodology": string[],           // one bullet each
 *   "reproduce": [{ "area": string, "command": string }], // "\n" separates commands
 *   "groups": [{                       // display order
 *     "id": string,                    // unique, stable; used for element ids
 *     "title": string,                 // user-facing outcome, e.g. "Find anything"
 *     "lede": string,
 *     "outcomes": [{                   // headline figures, 1–3 per group
 *       "id": string,
 *       "value": string,               // as printed, without unit: "155–291", "≤ 1.1", "~3"
 *       "unit": string,
 *       "label": string,               // completes the sentence "<value> <unit> …"
 *       "detail": string,              // workload and secondary readings
 *       "scope": string,               // small print: what kind of measurement this is
 *       "source": string               // where in docs/BENCHMARKS.md the number comes from
 *     }],
 *     "chart": StripChart | StackChart
 *   }],
 *   "comparison": {
 *     "release": string | null,        // Strata version measured, e.g. "v1.0.0"
 *     "measuredOn": string | null,     // ISO date of the run
 *     "machine": string | null,        // generic hardware description
 *     "methodology": string[],
 *     "results": [{                    // empty: the head-to-head is not rendered at all
 *       "metric": string,              // "Full scan, <where>" | "First scan of the session, <where>"
 *                                      // | "Peak memory, <where>"; anything else gets its own chart
 *       "detail": string,              // e.g. "4.6M entries, NVMe SSD, median of 3 runs"
 *       "unit": string,                // "s", "MiB"
 *       "lowerIsBetter": boolean,
 *       "values": [{
 *         "tool": string,              // "Strata" | "Strata (MFT)" | "Strata (standard scanner)"
 *                                      // | "WizTree <ver>" | "Windows File Explorer" | …
 *         "version"?: string,          // shown when the tool name doesn't already include it
 *         "value": number | null,      // null: not measured
 *         "detail"?: string            // scope caveat, e.g. "Explorer counts C:\\Users only"
 *       }]
 *     }]
 *   }
 * }
 *
 * // A range strip: one horizontal mark per row on a shared axis.
 * StripChart = {
 *   "kind": "strip", "title": string, "caption": string,
 *   "scale": "linear" | "log", "min": number, "max": number, "unit": string,
 *   "ticks": [{ "at": number, "label": string }],
 *   "rows": [{
 *     "label": string, "detail"?: string,
 *     "min": number | null,            // null: only an upper bound ("≤") is known
 *     "max": number,                   // same as min for a single reading
 *     "text": string,                  // value as printed, with unit
 *     "highlight"?: boolean,           // the row behind the group's headline figure
 *     "source": string
 *   }]
 * }
 *
 * // A stacked bar per row, e.g. index memory and name storage per drive size.
 * StackChart = {
 *   "kind": "stack", "title": string, "caption": string,
 *   "min": 0, "max": number, "unit": string,
 *   "ticks": [{ "at": number, "label": string }],
 *   "series": [{ "id": string, "label": string }],
 *   "rows": [{
 *     "label": string, "detail"?: string,
 *     "values": number[],              // one per series
 *     "text": string,
 *     "estimate"?: boolean,            // derived, not measured; drawn outlined
 *     "source": string
 *   }]
 * }
 * ```
 *
 * Publishing a head-to-head is a data-only change: copy the `comparison` block
 * that `bench/verify-elevated.ps1` prints and it appears on the next build.
 */
import { readFileSync } from "node:fs";

// -----------------------------------------------------------------------------
// Data model
// -----------------------------------------------------------------------------

/** One tick on a chart axis, in the chart's unit. */
export interface Tick {
  at: number;
  label: string;
}

/** One row of a range strip. */
export interface StripRow {
  label: string;
  detail?: string;
  /** Fastest run; `null` when only an upper bound was recorded. */
  min: number | null;
  /** Slowest run, or the single reading. */
  max: number;
  /** Value as printed, with its unit. */
  text: string;
  /** Marks the row behind the group's headline figure. */
  highlight?: boolean;
  /** Where in `docs/BENCHMARKS.md` the number comes from. */
  source: string;
}

/** Range strip: one mark per row on a shared linear or logarithmic axis. */
export interface StripChart {
  kind: "strip";
  title: string;
  caption: string;
  scale: "linear" | "log";
  min: number;
  max: number;
  unit: string;
  ticks: Tick[];
  rows: StripRow[];
}

/** One stacked bar. */
export interface StackRow {
  label: string;
  detail?: string;
  /** One value per series, in the chart's unit. */
  values: number[];
  text: string;
  /** Derived from measurements rather than measured; drawn outlined. */
  estimate?: boolean;
  source: string;
}

/** Stacked bars on a linear axis from zero. */
export interface StackChart {
  kind: "stack";
  title: string;
  caption: string;
  min: number;
  max: number;
  unit: string;
  ticks: Tick[];
  series: { id: string; label: string }[];
  rows: StackRow[];
}

/** One headline figure, phrased as what a user gets. */
export interface Outcome {
  id: string;
  /** Value exactly as printed in the docs, without the unit. */
  value: string;
  unit: string;
  /** Completes the sentence "<value> <unit> …". */
  label: string;
  detail: string;
  /** Small print: what kind of measurement produced the figure. */
  scope: string;
  source: string;
}

/** A group of related outcomes with one chart. */
export interface Group {
  id: string;
  title: string;
  lede: string;
  outcomes: Outcome[];
  chart: StripChart | StackChart;
}

/** One tool's reading in a head-to-head measurement. */
export interface ComparisonValue {
  tool: string;
  version?: string;
  /** `null` when the tool was not measured on this row. */
  value: number | null;
  /** Scope caveat, e.g. "Explorer counts C:\Users only". */
  detail?: string;
}

/** One head-to-head measurement across tools. */
export interface ComparisonResult {
  metric: string;
  detail: string;
  unit: string;
  lowerIsBetter: boolean;
  values: ComparisonValue[];
}

/** Head-to-head block, as emitted by `bench/verify-elevated.ps1`. */
export interface Comparison {
  release: string | null;
  measuredOn: string | null;
  machine: string | null;
  methodology: string[];
  results: ComparisonResult[];
}

/** The whole data file. */
export interface BenchmarkData {
  methodologyUrl: string;
  machine: string;
  noise: string;
  methodology: string[];
  reproduce: { area: string; command: string }[];
  groups: Group[];
  comparison: Comparison;
}

/** Rendered fragments, keyed by their `<!--bench:*-->` placeholder. */
export interface BenchmarkHtml {
  machine: string;
  versus: string;
  groups: string;
  method: string;
  reproduce: string;
  link: string;
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

const esc = (s: string): string => s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

const pct = (n: number): string => `${n.toFixed(2)}%`;

/** Words that describe internal goals, never results; the build rejects them in page copy. */
const INTERNAL_WORDS = /\b(targets?|budgets?)\b/i;

/** Position of `v` on a chart axis, as a percentage of the plot width. */
function position(chart: { min: number; max: number; scale?: "linear" | "log" }, v: number): number {
  if (chart.scale === "log") {
    const lo = Math.log10(chart.min);
    return ((Math.log10(v) - lo) / (Math.log10(chart.max) - lo)) * 100;
  }
  return ((v - chart.min) / (chart.max - chart.min)) * 100;
}

function fail(where: string, problem: string): never {
  throw new Error(`benchmarks.json: ${where} ${problem}`);
}

function validateTicks(where: string, chart: StripChart | StackChart): void {
  if (!(chart.max > chart.min)) fail(where, "has an empty axis");
  if (chart.kind === "stack" && chart.min !== 0) fail(where, "must start at zero");
  if (chart.kind === "strip" && chart.scale === "log" && !(chart.min > 0)) fail(where, "needs a positive minimum on a log scale");
  for (const t of chart.ticks) if (t.at < chart.min || t.at > chart.max) fail(where, `has tick ${t.at} outside its axis`);
}

function validate(data: BenchmarkData): void {
  const ids = new Set<string>();
  const unique = (id: string, where: string) => {
    if (!/^[a-z0-9-]+$/.test(id)) fail(where, "needs a lowercase id");
    if (ids.has(id)) fail(where, "is duplicated");
    ids.add(id);
  };
  for (const g of data.groups) {
    const where = `group "${g.id}"`;
    unique(g.id, where);
    if (g.outcomes.length === 0) fail(where, "has no outcomes");
    for (const o of g.outcomes) {
      unique(o.id, `outcome "${o.id}"`);
      if (!o.source || !o.scope) fail(`outcome "${o.id}"`, "needs a source and a scope");
    }
    const c = g.chart;
    const cw = `chart of group "${g.id}"`;
    validateTicks(cw, c);
    if (c.rows.length === 0) fail(cw, "has no rows");
    if (c.kind === "strip") {
      for (const r of c.rows) {
        const rw = `${cw}, row "${r.label}",`;
        if (!r.source) fail(rw, "needs a source");
        if (!Number.isFinite(r.max) || (r.min !== null && (!Number.isFinite(r.min) || r.min > r.max))) fail(rw, "has an invalid range");
        if ((r.min ?? r.max) < c.min || r.max > c.max) fail(rw, "falls outside the axis");
      }
    } else {
      for (const r of c.rows) {
        const rw = `${cw}, row "${r.label}",`;
        if (!r.source) fail(rw, "needs a source");
        if (r.values.length !== c.series.length) fail(rw, "needs one value per series");
        if (r.values.some((v) => !(v >= 0))) fail(rw, "has a negative value");
        if (r.values.reduce((a, b) => a + b, 0) > c.max) fail(rw, "falls outside the axis");
      }
    }
  }
  for (const r of data.comparison.results) {
    const where = `comparison "${r.metric}"`;
    if (r.values.length === 0) fail(where, "has no values");
    for (const v of r.values) {
      if (!v.tool) fail(where, "has a value without a tool");
      if (v.value !== null && !(Number.isFinite(v.value) && v.value >= 0)) fail(where, `has an invalid value for ${v.tool}`);
    }
  }
}

/** Fails the build if internal-goal vocabulary reaches visible page text. */
function checkCopy(html: string): void {
  const text = html
    .replace(/<[^>]+>/g, " ")
    .replace(/&#(\d+);/g, (_, n: string) => String.fromCharCode(Number(n)));
  const hit = INTERNAL_WORDS.exec(text);
  if (hit) throw new Error(`benchmarks.json: page copy must describe results, not goals; found "${hit[0]}" in "${text.slice(Math.max(0, hit.index - 40), hit.index + 40).trim()}"`);
}

// -----------------------------------------------------------------------------
// Charts
// -----------------------------------------------------------------------------

function axis(chart: StripChart | StackChart): string {
  const ticks = chart.ticks.map((t) => `<span style="--x:${pct(position(chart, t.at))}">${esc(t.label)}</span>`).join("");
  return `<div class="chart__axis"><div class="chart__ticks">${ticks}</div></div>`;
}

const grid = (chart: StripChart | StackChart): string => chart.ticks.map((t) => `<i class="chart__grid" style="--x:${pct(position(chart, t.at))}"></i>`).join("");

const rowLabel = (label: string, detail?: string): string =>
  `<div class="chart__label"><span class="chart__name">${esc(label)}</span>${detail ? `<span class="chart__detail">${esc(detail)}</span>` : ""}</div>`;

function srTable(caption: string, head: string[], rows: string[][]): string {
  // NOTE: the hiding wrapper is a block because tables ignore `width: 1px` and
  // `overflow: hidden`, so a hidden table still widened the page on phones.
  return `<div class="visually-hidden"><table><caption>${esc(caption)}</caption><thead><tr>${head.map((h) => `<th scope="col">${esc(h)}</th>`).join("")}</tr></thead><tbody>${rows
    .map((r) => `<tr><th scope="row">${esc(r[0] ?? "")}</th>${r
      .slice(1)
      .map((c) => `<td>${esc(c)}</td>`)
      .join("")}</tr>`)
    .join("")}</tbody></table></div>`;
}

function strip(chart: StripChart): string {
  const rows = chart.rows.map((r) => {
    const a = position(chart, r.min ?? chart.min);
    const b = position(chart, r.max);
    const kind = r.min === null ? "is-bound" : r.min === r.max ? "is-point" : "is-range";
    return `<div class="chart__row${r.highlight ? " is-highlight" : ""}">
  ${rowLabel(r.label, r.detail)}
  <div class="chart__plot">${grid(chart)}<span class="strip__mark ${kind}" style="--a:${pct(a)};--b:${pct(b)}"></span></div>
  <span class="chart__value">${esc(r.text)}</span>
</div>`;
  });
  const range = chart.rows.some((r) => r.min !== null && r.min !== r.max);
  const bound = chart.rows.some((r) => r.min === null);
  const key = [range ? `<li><i class="key__range"></i>Spread across runs</li>` : "", `<li><i class="key__point"></i>Single reading</li>`, bound ? `<li><i class="key__bound"></i>Upper bound</li>` : ""].join("");
  return `<div class="chart__plotarea" aria-hidden="true">${rows.join("\n")}${axis(chart)}</div>
<ul class="chart__key" aria-hidden="true">${key}</ul>
${srTable(
  chart.title,
  ["Measurement", "Result"],
  chart.rows.map((r) => [r.detail ? `${r.label}, ${r.detail}` : r.label, r.text]),
)}`;
}

function stack(chart: StackChart): string {
  const rows = chart.rows.map((r) => {
    let start = 0;
    const segs = r.values
      .map((v, i) => {
        const seg = `<span class="stack__seg stack__seg--${i}" style="--a:${pct(position(chart, start))};--b:${pct(position(chart, start + v))}"></span>`;
        start += v;
        return seg;
      })
      .join("");
    return `<div class="chart__row${r.estimate ? " is-estimate" : ""}">
  ${rowLabel(r.label, r.detail)}
  <div class="chart__plot">${grid(chart)}${segs}</div>
  <span class="chart__value">${esc(r.text)}</span>
</div>`;
  });
  const key = chart.series.map((s, i) => `<li><i class="key__seg key__seg--${i}"></i>${esc(s.label)}</li>`).join("");
  const estimate = chart.rows.some((r) => r.estimate) ? `<li><i class="key__seg key__seg--estimate"></i>Estimated</li>` : "";
  const fmt = (v: number) => `${v.toLocaleString("en-US", { maximumFractionDigits: 1 })} ${chart.unit}`;
  return `<div class="chart__plotarea" aria-hidden="true">${rows.join("\n")}${axis(chart)}</div>
<ul class="chart__key" aria-hidden="true">${key}${estimate}</ul>
${srTable(
  chart.title,
  ["Drive size", ...chart.series.map((s) => s.label), "Total"],
  chart.rows.map((r) => [r.estimate ? `${r.label} (estimated)` : r.label, ...r.values.map(fmt), r.text]),
)}`;
}

function chart(c: StripChart | StackChart): string {
  return `<figure class="chart chart--${c.kind}${c.ticks.length > 4 ? " chart--dense" : ""}">
  <figcaption class="chart__head"><span class="chart__title">${esc(c.title)}</span><span class="chart__caption">${esc(c.caption)}</span></figcaption>
  ${c.kind === "strip" ? strip(c) : stack(c)}
</figure>`;
}

// -----------------------------------------------------------------------------
// Outcome groups
// -----------------------------------------------------------------------------

function outcome(o: Outcome): string {
  return `<li class="outcome">
  <p class="outcome__figure"><span class="outcome__value">${esc(o.value)}</span><span class="outcome__unit">${esc(o.unit)}</span></p>
  <p class="outcome__label">${esc(o.label)}</p>
  <p class="outcome__detail">${esc(o.detail)}</p>
  <p class="outcome__scope">${esc(o.scope)}</p>
</li>`;
}

function groups(data: BenchmarkData): string {
  return data.groups
    .map(
      (g) => `<section class="bgroup reveal" aria-labelledby="bg-${g.id}">
  <header class="bgroup__head">
    <h3 class="bgroup__title" id="bg-${g.id}">${esc(g.title)}</h3>
    <p class="bgroup__lede">${esc(g.lede)}</p>
  </header>
  <div class="bgroup__body">
    <ul class="outcomes outcomes--${g.outcomes.length}">${g.outcomes.map(outcome).join("\n")}</ul>
    ${chart(g.chart)}
  </div>
</section>`,
    )
    .join("\n");
}

// -----------------------------------------------------------------------------
// Head-to-head
// -----------------------------------------------------------------------------

/** Display order: Strata's scanners first, then other tools in a fixed order. */
function toolRank(tool: string): number {
  if (tool === "Strata" || tool === "Strata (MFT)") return 0;
  if (tool.startsWith("Strata")) return 1;
  if (tool.startsWith("WizTree")) return 2;
  if (/explorer/i.test(tool)) return 3;
  return 4;
}

/** The current script emits plain "Strata" for the MFT scan. */
const toolName = (tool: string): string => (tool === "Strata" ? "Strata (MFT)" : tool);

/** Short family name for the block title. */
function family(tool: string): string {
  if (tool.startsWith("Strata")) return "Strata";
  if (tool.startsWith("WizTree")) return "WizTree";
  if (/explorer/i.test(tool)) return "File Explorer";
  return tool;
}

/** Precision follows magnitude, so a 412 s run isn't printed as 412.37 s. */
function num(v: number): string {
  const digits = v >= 100 ? 0 : v >= 10 ? 1 : 2;
  return v.toLocaleString("en-US", { minimumFractionDigits: 0, maximumFractionDigits: digits });
}

const factor = (f: number): string => (f < 10 ? f.toFixed(1) : Math.round(f).toLocaleString("en-US"));

type Kind = "full" | "first" | "memory";

const KINDS: [Kind, RegExp][] = [
  ["full", /^Full scan, (.+)$/],
  ["first", /^First scan of the session, (.+)$/],
  ["memory", /^Peak memory, (.+)$/],
];

interface Panel {
  where: string;
  full?: ComparisonResult;
  first?: ComparisonResult;
  memory?: ComparisonResult;
}

function panels(results: ComparisonResult[]): { panels: Panel[]; other: ComparisonResult[] } {
  const byWhere = new Map<string, Panel>();
  const other: ComparisonResult[] = [];
  for (const r of results) {
    const hit = KINDS.map(([k, re]) => [k, re.exec(r.metric)?.[1]] as const).find(([, w]) => w);
    if (!hit?.[1]) {
      other.push(r);
      continue;
    }
    const [kind, where] = hit;
    const p = byWhere.get(where) ?? { where };
    p[kind] = r;
    byWhere.set(where, p);
  }
  return { panels: [...byWhere.values()], other };
}

const sentence = (s: string): string => s.charAt(0).toUpperCase() + s.slice(1);

/**
 * One bar chart: a row per tool, scaled to the largest reading. A matching
 * first-run result is drawn as a marker on the same scale.
 */
function bars(title: string, r: ComparisonResult, first?: ComparisonResult): string {
  const vals = [...r.values].sort((a, b) => toolRank(a.tool) - toolRank(b.tool));
  const firstOf = (tool: string) => first?.values.find((v) => v.tool === tool)?.value ?? null;
  const measured = vals.flatMap((v) => (v.value === null ? [] : [v.value]));
  const scale = Math.max(...measured, ...vals.map((v) => firstOf(v.tool) ?? 0));
  const best = measured.length > 0 ? (r.lowerIsBetter ? Math.min(...measured) : Math.max(...measured)) : null;
  const memory = /mib|gib|mb|gb/i.test(r.unit);
  const bestWord = r.lowerIsBetter ? (memory ? "Least memory" : "Fastest") : "Highest";
  const unit = (v: number) => `${num(v)} ${r.unit}`;

  const rel = (v: number): string => {
    if (best === null || v === best) return bestWord;
    if (best === 0 || v === 0) return "";
    const f = r.lowerIsBetter ? v / best : best / v;
    return r.lowerIsBetter ? `${factor(f)}× ${memory ? "more" : "longer"}` : `${factor(f)}× lower`;
  };

  const rows = vals.map((v) => {
    const name = toolName(v.tool);
    const ver = v.version && !name.includes(v.version) ? v.version : "";
    const f = firstOf(v.tool);
    const rank = toolRank(v.tool);
    const cls = `vs__row${rank === 0 ? " is-self" : rank === 1 ? " is-self-alt" : ""}${v.value !== null && v.value === best ? " is-best" : ""}${v.value === null ? " is-missing" : ""}`;
    const bar =
      v.value === null
        ? ""
        : `<span class="vs__bar" style="--w:${pct(scale > 0 ? (v.value / scale) * 100 : 0)}"></span>${f !== null && scale > 0 ? `<span class="vs__first" style="--x:${pct((f / scale) * 100)}"></span>` : ""}`;
    return `<div class="${cls}">
  <div class="vs__tool"><span class="vs__name">${esc(name)}${ver ? ` <span class="vs__ver">${esc(ver)}</span>` : ""}</span>${v.detail ? `<span class="vs__note">${esc(v.detail)}</span>` : ""}</div>
  <div class="vs__plot">${bar}</div>
  <div class="vs__value">${
    v.value === null
      ? `<span class="vs__num is-none">Not measured</span>`
      : `<span class="vs__num">${esc(num(v.value))}<span class="u"> ${esc(r.unit)}</span></span><span class="vs__rel">${esc(rel(v.value))}</span>`
  }</div>${first ? `<div class="vs__firstval"><span class="vs__firstlabel">first run </span>${f === null ? "—" : esc(unit(f))}</div>` : ""}
</div>`;
  });

  const head = ["Tool", r.lowerIsBetter ? `${sentence(r.metric)} (lower is better)` : sentence(r.metric)];
  if (first) head.push("First run of the session");
  head.push("Note");
  const table = srTable(
    `${title}: ${r.detail}`,
    head,
    vals.map((v) => {
      const cells = [toolName(v.tool) + (v.version && !toolName(v.tool).includes(v.version) ? ` ${v.version}` : ""), v.value === null ? "Not measured" : unit(v.value)];
      if (first) {
        const f = firstOf(v.tool);
        cells.push(f === null ? "Not measured" : unit(f));
      }
      cells.push(v.detail ?? "");
      return cells;
    }),
  );

  return `<div class="vs__chart">
  <div class="vs__chart-head"><h5 class="vs__chart-title">${esc(title)}</h5><span class="vs__chart-sub">${esc(r.detail)}${r.lowerIsBetter ? " · lower is better" : ""}</span></div>
  <div class="vs__rows${first ? " has-first" : ""}" aria-hidden="true">${first ? `<div class="vs__cols"><span>Median</span><span>First run</span></div>` : ""}${rows.join("\n")}</div>
  ${table}
</div>`;
}

function versus(c: Comparison): string {
  if (c.results.length === 0) return "";
  const { panels: ps, other } = panels(c.results);
  const tools = [...new Set(c.results.flatMap((r) => r.values.map((v) => v.tool)))].sort((a, b) => toolRank(a) - toolRank(b));
  const families = [...new Set(tools.map(family))];
  const meta = [c.release ? `Strata ${c.release}` : null, c.measuredOn, c.machine].filter((x): x is string => !!x);
  const hasFirst = ps.some((p) => p.first);

  const volume = (p: Panel): string => {
    const charts = [p.full ? bars("Full scan time", p.full, p.first) : p.first ? bars("First scan of the session", p.first) : "", p.memory ? bars("Peak memory", p.memory) : ""].join("\n");
    return `<article class="vs__volume">
  <h4 class="vs__where">${esc(sentence(p.where))}</h4>
  ${charts}
</article>`;
  };

  const key = `<ul class="chart__key vs__key" aria-hidden="true">${tools.some((t) => toolRank(t) === 0) ? `<li><i class="key__bar key__bar--self"></i>Strata, MFT scan</li>` : ""}${tools.some((t) => toolRank(t) === 1) ? `<li><i class="key__bar key__bar--alt"></i>Strata, standard scanner</li>` : ""}<li><i class="key__bar"></i>Other tools</li>${hasFirst ? `<li><i class="key__first"></i>First run of the session, no cache dropped</li>` : ""}</ul>`;

  return `<div class="block vs reveal" id="head-to-head">
  <header class="block__head">
    <div>
      <h3 class="block__title">${esc(families.join(" vs. "))}</h3>
      <p class="block__sub">Complete scans of the same volume on the same machine, timed from launch to a full size total.</p>
    </div>
    ${meta.length > 0 ? `<p class="block__meta">${meta.map(esc).join(" · ")}</p>` : ""}
  </header>
  ${key}
  ${ps.map(volume).join("\n")}
  ${other.length > 0 ? `<article class="vs__volume">${other.map((r) => bars(sentence(r.metric), r)).join("\n")}</article>` : ""}
  ${c.methodology.length > 0 ? `<ul class="method method--compact">${c.methodology.map((m) => `<li>${esc(m)}</li>`).join("")}</ul>` : ""}
</div>`;
}

// -----------------------------------------------------------------------------
// Method
// -----------------------------------------------------------------------------

function method(data: BenchmarkData): string {
  return `<ul class="method">${data.methodology.map((m) => `<li>${esc(m)}</li>`).join("")}<li>${esc(data.noise)}</li></ul>`;
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

/**
 * Reads, validates and renders the benchmark data file.
 *
 * @param file - Path to `benchmarks.json`.
 * @returns HTML fragments for each placeholder.
 * @throws If the data file is inconsistent (duplicate id, value outside its
 *   axis, missing source) or its copy mentions internal goals.
 */
export function renderBenchmarks(file: string): BenchmarkHtml {
  const data = JSON.parse(readFileSync(file, "utf8")) as BenchmarkData;
  validate(data);
  const out: BenchmarkHtml = {
    machine: `Measured on ${/^[aeiou8]/i.test(data.machine) ? "an" : "a"} ${esc(data.machine)}.`,
    versus: versus(data.comparison),
    groups: groups(data),
    method: method(data),
    reproduce: reproduce(data),
    link: esc(data.methodologyUrl),
  };
  checkCopy(out.machine + out.versus + out.groups + out.method);
  return out;
}
