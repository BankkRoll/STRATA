/**
 * Lightweight SVG charts: a sparkline, a time-series line chart and a
 * horizontal bar list. Each chart is an `img` with a summary label and has
 * a real data table next to it (visually hidden or toggled), so screen
 * reader and keyboard users get the numbers, not a picture.
 */
import { useId, useState, type ReactNode } from "react";
import { byteTicks, formatByteTick, linePath, linearScale, nearestIndex, type SeriesPoint } from "../lib/chart";
import { formatBytes, formatDate, formatDateTime, type SizeUnits } from "../lib/format";

// -----------------------------------------------------------------------------
// Sparkline
// -----------------------------------------------------------------------------

/** Props for {@link Sparkline}. */
export interface SparklineProps {
  /** Points sorted by time; `bytes: null` is a gap (below the snapshot threshold). */
  points: readonly { atMs: number; bytes: number | null }[];
  /** Accessible name ("Size history of models"). */
  label: string;
  units?: SizeUnits;
  width?: number;
  height?: number;
}

/**
 * Tiny size-history line for the detail panel, with first/last values in
 * its label and a hidden table of every point.
 */
export function Sparkline({ points, label, units = "binary", width = 220, height = 40 }: SparklineProps) {
  const known = points.filter((p): p is { atMs: number; bytes: number } => p.bytes !== null);
  if (known.length < 2) return <p className="detail__muted">Not enough history yet</p>;
  const xs = points.map((p) => p.atMs);
  const ys = known.map((p) => p.bytes);
  const sx = linearScale([Math.min(...xs), Math.max(...xs)], [1, width - 1]);
  const sy = linearScale([Math.min(...ys), Math.max(...ys)], [height - 3, 3]);
  const series: SeriesPoint[] = points.map((p) => ({ x: p.atMs, y: p.bytes }));
  const first = known[0] as { atMs: number; bytes: number };
  const last = known[known.length - 1] as { atMs: number; bytes: number };
  const summary = `${label}: ${formatBytes(first.bytes, { units })} on ${formatDate(first.atMs)} to ${formatBytes(last.bytes, { units })} on ${formatDate(last.atMs)}`;
  return (
    <figure className="spark">
      <svg className="sparkline" viewBox={`0 0 ${width} ${height}`} role="img" aria-label={summary}>
        <path d={linePath(series, sx, sy)} fill="none" stroke="currentColor" strokeWidth="1.5" />
        <circle cx={sx(last.atMs)} cy={sy(last.bytes)} r="2.5" fill="currentColor" />
      </svg>
      <table className="visually-hidden">
        <caption>{label}</caption>
        <thead>
          <tr>
            <th scope="col">Date</th>
            <th scope="col">Size</th>
          </tr>
        </thead>
        <tbody>
          {points.map((p) => (
            <tr key={p.atMs}>
              <td>{formatDateTime(p.atMs)}</td>
              <td>{p.bytes === null ? "Below threshold" : formatBytes(p.bytes, { units })}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </figure>
  );
}

// -----------------------------------------------------------------------------
// Line chart
// -----------------------------------------------------------------------------

/** One series of a {@link TimeChart}. */
export interface TimeSeries {
  name: string;
  /** CSS color token, e.g. `var(--mark-1)`. */
  color: string;
  /** Dash pattern so series differ without color (color-blind safe). */
  dash?: string;
  points: readonly SeriesPoint[];
}

/** Props for {@link TimeChart}. */
export interface TimeChartProps {
  title: string;
  series: readonly TimeSeries[];
  units?: SizeUnits;
  height?: number;
  /** Fixed y-axis maximum (e.g. volume capacity). */
  yMax?: number;
}

function valueText(y: number | null | undefined, units: SizeUnits, missing: string): string {
  return y === null || y === undefined ? missing : formatBytes(y, { units });
}

const W = 640;
const PAD = { l: 64, r: 12, t: 10, b: 26 };

/**
 * Time × bytes line chart. Arrow keys move a focus cursor between points
 * (announced through a live region); hover does the same with the mouse.
 * "Show as table" reveals the data table that screen readers always get.
 */
export function TimeChart({ title, series, units = "binary", height = 220, yMax }: TimeChartProps) {
  const [cursor, setCursor] = useState<number | null>(null);
  const [table, setTable] = useState(false);
  const id = useId();
  const base = series[0]?.points ?? [];
  const all = series.flatMap((s) => s.points);
  const xs = all.map((p) => p.x);
  const ys = all.flatMap((p) => (p.y === null ? [] : [p.y]));
  if (base.length === 0 || ys.length === 0) return <p className="detail__muted">No data points yet.</p>;
  const ticks = byteTicks(Math.min(0, ...ys), Math.max(yMax ?? 0, ...ys), 4, units);
  const sx = linearScale([Math.min(...xs), Math.max(...xs)], [PAD.l, W - PAD.r]);
  const sy = linearScale([ticks[0] ?? 0, ticks[ticks.length - 1] ?? 1], [height - PAD.b, PAD.t]);
  const xTicks = [0, Math.floor((base.length - 1) / 2), base.length - 1].filter((v, i, a) => a.indexOf(v) === i);
  const cur = cursor === null ? null : base[cursor];
  const describe = (i: number) => {
    const p = base[i];
    if (!p) return "";
    return `${formatDateTime(p.x)}: ${series.map((s) => `${s.name} ${valueText(s.points[i]?.y, units, "no data")}`).join(", ")}`;
  };
  const onKey = (e: React.KeyboardEvent) => {
    const n = base.length;
    let next = cursor ?? n - 1;
    if (e.key === "ArrowLeft") next = Math.max(0, next - 1);
    else if (e.key === "ArrowRight") next = Math.min(n - 1, next + 1);
    else if (e.key === "Home") next = 0;
    else if (e.key === "End") next = n - 1;
    else return;
    e.preventDefault();
    setCursor(next);
  };
  return (
    <figure className="chart">
      <figcaption className="chart__head">
        <span id={id} className="chart__title">
          {title}
        </span>
        <span className="chart__legend">
          {series.map((s) => (
            <span key={s.name} className="chart__key">
              <svg width="22" height="8" aria-hidden="true">
                <line x1="0" y1="4" x2="22" y2="4" stroke={s.color} strokeWidth="2" strokeDasharray={s.dash} />
              </svg>
              {s.name}
            </span>
          ))}
        </span>
        <button
          type="button"
          className="btn btn--small"
          aria-pressed={table}
          onClick={() => {
            setTable((t) => !t);
          }}
        >
          Show as table
        </button>
      </figcaption>
      <svg
        className="chart__svg"
        viewBox={`0 0 ${W} ${height}`}
        role="img"
        aria-labelledby={id}
        aria-describedby={`${id}-hint`}
        tabIndex={0}
        onKeyDown={onKey}
        onPointerMove={(e) => {
          const r = e.currentTarget.getBoundingClientRect();
          if (r.width === 0) return;
          const vx = ((e.clientX - r.left) / r.width) * W;
          const t = sx.domain[0] + ((vx - PAD.l) / (W - PAD.l - PAD.r)) * (sx.domain[1] - sx.domain[0]);
          setCursor(nearestIndex(base, t));
        }}
        onPointerLeave={() => {
          setCursor(null);
        }}
        onBlur={() => {
          setCursor(null);
        }}
      >
        {ticks.map((t) => (
          <g key={t} className="chart__grid">
            <line x1={PAD.l} x2={W - PAD.r} y1={sy(t)} y2={sy(t)} />
            <text x={PAD.l - 6} y={sy(t)} dy="0.32em" textAnchor="end">
              {formatByteTick(t, units)}
            </text>
          </g>
        ))}
        {xTicks.map((i) => {
          const p = base[i];
          if (!p) return null;
          return (
            <text key={i} className="chart__xlabel" x={sx(p.x)} y={height - 6} textAnchor={i === 0 ? "start" : i === base.length - 1 ? "end" : "middle"}>
              {formatDate(p.x)}
            </text>
          );
        })}
        {series.map((s) => (
          <path key={s.name} d={linePath(s.points, sx, sy)} fill="none" stroke={s.color} strokeWidth="2" strokeDasharray={s.dash} />
        ))}
        {cur && (
          <g className="chart__cursor">
            <line x1={sx(cur.x)} x2={sx(cur.x)} y1={PAD.t} y2={height - PAD.b} />
            {series.map((s) => {
              const y = cursor === null ? null : s.points[cursor]?.y;
              return y == null ? null : <circle key={s.name} cx={sx(cur.x)} cy={sy(y)} r="3.5" fill={s.color} />;
            })}
          </g>
        )}
      </svg>
      <p id={`${id}-hint`} className="visually-hidden">
        Use Left and Right arrow keys to read values.
      </p>
      <p className="chart__readout" aria-live="polite">
        {cursor === null ? "" : describe(cursor)}
      </p>
      <table className={table ? "table" : "visually-hidden"}>
        <caption className="visually-hidden">{title}</caption>
        <thead>
          <tr>
            <th scope="col">Date</th>
            {series.map((s) => (
              <th key={s.name} scope="col" className="num">
                {s.name}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {base.map((p, i) => (
            <tr key={p.x}>
              <td>{formatDateTime(p.x)}</td>
              {series.map((s) => (
                <td key={s.name} className="num">
                  {valueText(s.points[i]?.y, units, "—")}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </figure>
  );
}

// -----------------------------------------------------------------------------
// Bar list
// -----------------------------------------------------------------------------

/** One bar. */
export interface Bar {
  key: string;
  label: ReactNode;
  value: number;
  /** Text for the value column. */
  valueText: string;
  /** Fill color (CSS); patterns are added by the caller via `className`. */
  color?: string;
  className?: string;
}

/** Props for {@link BarList}. */
export interface BarListProps {
  /** Accessible name of the list. */
  label: string;
  bars: readonly Bar[];
  /** Makes each bar a button. */
  onSelect?: (key: string) => void;
  /** Accessible verb for `onSelect` ("Filter by"). */
  selectHint?: string;
}

/**
 * Horizontal bars as a real list (each row has label, value and a decorative
 * bar), so it reads correctly without the graphics.
 */
export function BarList({ label, bars, onSelect, selectHint = "Show" }: BarListProps) {
  const max = Math.max(1, ...bars.map((b) => b.value));
  return (
    <ul className="bars" aria-label={label}>
      {bars.map((b) => {
        const inner = (
          <>
            <span className="bars__label">{b.label}</span>
            <span className="bars__track" aria-hidden="true">
              <span className={`bars__fill ${b.className ?? ""}`} style={{ width: `${Math.max(0.5, (b.value / max) * 100)}%`, ...(b.color ? { backgroundColor: b.color } : {}) }} />
            </span>
            <span className="bars__value">{b.valueText}</span>
          </>
        );
        return (
          <li key={b.key} className="bars__row">
            {onSelect ? (
              <button
                type="button"
                className="bars__btn"
                aria-label={`${selectHint} ${typeof b.label === "string" ? b.label : b.key}, ${b.valueText}`}
                onClick={() => {
                  onSelect(b.key);
                }}
              >
                {inner}
              </button>
            ) : (
              <div className="bars__btn">{inner}</div>
            )}
          </li>
        );
      })}
    </ul>
  );
}
