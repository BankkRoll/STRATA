/**
 * Number, size and time formatting shared by every view.
 *
 * Responsibilities:
 * - Byte sizes in binary units shown as KB/MB/GB (Windows Explorer's
 *   convention) or SI units, with locale-aware separators.
 * - Counts and percentages.
 * - Timestamps: absolute local time, relative ("3 days ago"), and the
 *   suspicious-timestamp check (before 1990 or more than a day ahead).
 *
 * `Intl` formatters are cached per locale/option set because creating them
 * costs far more than formatting, and lists format thousands of cells.
 */

/** Unit system for sizes. `binary` divides by 1024; `si` divides by 1000. */
export type SizeUnits = "binary" | "si";

const BINARY_LABELS = ["B", "KB", "MB", "GB", "TB", "PB", "EB"] as const;
const SI_LABELS = ["B", "kB", "MB", "GB", "TB", "PB", "EB"] as const;

const numberFormats = new Map<string, Intl.NumberFormat>();

function numberFormat(locale: string | undefined, opts: Intl.NumberFormatOptions): Intl.NumberFormat {
  const key = `${locale ?? ""}|${JSON.stringify(opts)}`;
  let f = numberFormats.get(key);
  if (!f) {
    f = new Intl.NumberFormat(locale, opts);
    numberFormats.set(key, f);
  }
  return f;
}

/** Fraction digits that give three significant digits for a value in `[1, 1024)`. */
function significantDigits(value: number): number {
  return value >= 100 ? 0 : value >= 10 ? 1 : 2;
}

/** Options shared by the formatting helpers. */
export interface FormatOptions {
  /** BCP 47 locale; `undefined` uses the runtime default (the Windows locale in WebView2). */
  locale?: string | undefined;
}

/** Options for {@link formatBytes}. */
export interface ByteFormatOptions extends FormatOptions {
  /** Unit system; defaults to `binary`. */
  units?: SizeUnits;
}

/**
 * Formats a byte count with three significant digits, the way Explorer does
 * ("1.45 GB", "14.5 GB", "145 GB").
 *
 * @param bytes - Byte count; negative values format with a leading minus
 *   (used for deltas), non-finite values as an em dash.
 * @param options - Unit system and locale.
 * @returns The formatted size.
 * @example
 * formatBytes(1536) // "1.50 KB"
 * formatBytes(1_500_000, { units: "si" }) // "1.50 MB"
 */
export function formatBytes(bytes: number, options: ByteFormatOptions = {}): string {
  if (!Number.isFinite(bytes)) return "—";
  const units = options.units ?? "binary";
  const base = units === "binary" ? 1024 : 1000;
  const labels = units === "binary" ? BINARY_LABELS : SI_LABELS;
  const sign = bytes < 0 ? "-" : "";
  let value = Math.abs(bytes);
  if (value < base) {
    return `${sign}${numberFormat(options.locale, { maximumFractionDigits: 0 }).format(value)} B`;
  }
  let unit = 0;
  while (value >= base && unit < labels.length - 1) {
    value /= base;
    unit++;
  }
  let digits = significantDigits(value);
  let rounded = Number(value.toFixed(digits));
  // NOTE: rounding can carry 9.996 to "10.00" or 1023.7 KB to "1024 KB";
  // re-pick the precision, and promote to the next unit on overflow.
  if (significantDigits(rounded) !== digits) {
    digits = significantDigits(rounded);
    rounded = Number(value.toFixed(digits));
  }
  if (rounded >= base && unit < labels.length - 1) {
    value /= base;
    unit++;
    digits = 2;
    rounded = Number(value.toFixed(digits));
  }
  const text = numberFormat(options.locale, {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  }).format(rounded);
  return `${sign}${text} ${labels[unit] ?? ""}`;
}

/**
 * Formats a byte count exactly, with grouping ("1,234,567 bytes"), for
 * tooltips and the detail panel.
 *
 * @param bytes - Byte count.
 * @param options - Locale.
 * @returns The exact count with a unit word.
 */
export function formatBytesExact(bytes: number, options: FormatOptions = {}): string {
  if (!Number.isFinite(bytes)) return "—";
  const n = numberFormat(options.locale, { maximumFractionDigits: 0 }).format(bytes);
  return `${n} ${Math.abs(bytes) === 1 ? "byte" : "bytes"}`;
}

/**
 * Formats an integer count with locale grouping.
 *
 * @param n - The count.
 * @param options - Locale.
 * @returns The grouped integer, or an em dash for non-finite input.
 */
export function formatCount(n: number, options: FormatOptions = {}): string {
  if (!Number.isFinite(n)) return "—";
  return numberFormat(options.locale, { maximumFractionDigits: 0 }).format(n);
}

/**
 * Formats a fraction as a percentage with one decimal. Non-zero values that
 * would round to 0.0% show as "<0.1%" so tiny items never read as empty.
 *
 * @param fraction - Value in `[0, 1]` (values outside are clamped).
 * @param options - Locale.
 * @returns The percentage text.
 * @example
 * formatPercent(0.1234) // "12.3%"
 * formatPercent(0.00001) // "<0.1%"
 */
export function formatPercent(fraction: number, options: FormatOptions = {}): string {
  if (!Number.isFinite(fraction)) return "—";
  const f = Math.min(1, Math.max(0, fraction));
  if (f > 0 && f < 0.0005) {
    return `<${numberFormat(options.locale, { style: "percent", minimumFractionDigits: 1 }).format(0.001)}`;
  }
  return numberFormat(options.locale, {
    style: "percent",
    minimumFractionDigits: 1,
    maximumFractionDigits: 1,
  }).format(f);
}

// -----------------------------------------------------------------------------
// Time
// -----------------------------------------------------------------------------

/** 2000-01-01T00:00:00Z in Unix milliseconds: the index's compact time epoch. */
export const EPOCH_2000_MS = Date.UTC(2000, 0, 1);

/** Timestamps before this are flagged suspicious. */
export const SUSPICIOUS_BEFORE_MS = Date.UTC(1990, 0, 1);

/** Slack for clock skew before a future timestamp counts as suspicious. */
const FUTURE_SLACK_MS = 24 * 60 * 60 * 1000;

/**
 * Converts the index's compact seconds-since-2000 to Unix milliseconds.
 *
 * @param secs - Seconds since 2000-01-01 UTC; `0` means unknown.
 * @returns Unix milliseconds, or `null` when unknown.
 */
export function epoch2000ToMs(secs: number): number | null {
  return secs > 0 ? EPOCH_2000_MS + secs * 1000 : null;
}

/**
 * Whether a timestamp is implausible: before 1990 or more than a day in the
 * future. Such values sort and display but carry a warning.
 *
 * @param ms - Unix milliseconds (UTC).
 * @param now - Current time in Unix milliseconds.
 * @returns `true` when the timestamp should be flagged.
 */
export function isSuspiciousTimestamp(ms: number, now: number = Date.now()): boolean {
  return !Number.isFinite(ms) || ms < SUSPICIOUS_BEFORE_MS || ms > now + FUTURE_SLACK_MS;
}

const dateFormats = new Map<string, Intl.DateTimeFormat>();

function dateFormat(locale: string | undefined, opts: Intl.DateTimeFormatOptions): Intl.DateTimeFormat {
  const key = `${locale ?? ""}|${JSON.stringify(opts)}`;
  let f = dateFormats.get(key);
  if (!f) {
    f = new Intl.DateTimeFormat(locale, opts);
    dateFormats.set(key, f);
  }
  return f;
}

/** Options for date formatting. */
export interface DateFormatOptions extends FormatOptions {
  /** IANA time zone; defaults to the system zone. Tests pin it for determinism. */
  timeZone?: string | undefined;
}

/**
 * Formats a UTC timestamp as local date and time ("Mar 4, 2025, 14:05").
 *
 * @param ms - Unix milliseconds (UTC), or `null` for unknown.
 * @param options - Locale and time zone.
 * @returns The local date/time, or "Unknown".
 */
export function formatDateTime(ms: number | null, options: DateFormatOptions = {}): string {
  if (ms === null || !Number.isFinite(ms)) return "Unknown";
  const opts: Intl.DateTimeFormatOptions = { dateStyle: "medium", timeStyle: "short" };
  if (options.timeZone) opts.timeZone = options.timeZone;
  return dateFormat(options.locale, opts).format(ms);
}

/**
 * Formats a UTC timestamp as a local date only.
 *
 * @param ms - Unix milliseconds (UTC), or `null` for unknown.
 * @param options - Locale and time zone.
 * @returns The local date, or "Unknown".
 */
export function formatDate(ms: number | null, options: DateFormatOptions = {}): string {
  if (ms === null || !Number.isFinite(ms)) return "Unknown";
  const opts: Intl.DateTimeFormatOptions = { dateStyle: "medium" };
  if (options.timeZone) opts.timeZone = options.timeZone;
  return dateFormat(options.locale, opts).format(ms);
}

const relativeFormats = new Map<string, Intl.RelativeTimeFormat>();

const RELATIVE_STEPS: readonly [Intl.RelativeTimeFormatUnit, number][] = [
  ["second", 60],
  ["minute", 60],
  ["hour", 24],
  ["day", 7],
  ["week", 4.348],
  ["month", 12],
  ["year", Number.POSITIVE_INFINITY],
];

/**
 * Formats a timestamp relative to `now` ("3 days ago", "in 2 hours").
 *
 * @param ms - Unix milliseconds (UTC), or `null` for unknown.
 * @param now - Reference time in Unix milliseconds.
 * @param options - Locale.
 * @returns The relative phrase, or "Unknown".
 */
export function formatRelative(ms: number | null, now: number = Date.now(), options: FormatOptions = {}): string {
  if (ms === null || !Number.isFinite(ms)) return "Unknown";
  const key = options.locale ?? "";
  let rtf = relativeFormats.get(key);
  if (!rtf) {
    rtf = new Intl.RelativeTimeFormat(options.locale, { numeric: "auto" });
    relativeFormats.set(key, rtf);
  }
  let value = (ms - now) / 1000;
  for (const [unit, step] of RELATIVE_STEPS) {
    if (Math.abs(value) < step) {
      return rtf.format(Math.round(value), unit);
    }
    value /= step;
  }
  return rtf.format(Math.round(value), "year");
}

/** A timestamp ready for display. */
export interface TimestampDisplay {
  /** Absolute local date/time. */
  absolute: string;
  /** Relative phrase. */
  relative: string;
  /** Whether the value is implausible (shown with a warning). */
  suspicious: boolean;
}

/**
 * Formats a timestamp for display with its suspicious flag.
 *
 * @param ms - Unix milliseconds (UTC), or `null` for unknown.
 * @param now - Reference time.
 * @param options - Locale and time zone.
 * @returns Absolute and relative text plus the suspicious flag.
 */
export function describeTimestamp(
  ms: number | null,
  now: number = Date.now(),
  options: DateFormatOptions = {},
): TimestampDisplay {
  return {
    absolute: formatDateTime(ms, options),
    relative: formatRelative(ms, now, options),
    suspicious: ms !== null && isSuspiciousTimestamp(ms, now),
  };
}
