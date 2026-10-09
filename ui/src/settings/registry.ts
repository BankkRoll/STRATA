/**
 * The settings registry: one typed entry per persisted setting, defining
 * where it appears, what it is called, how it is edited, its default, its
 * limits and its badges. The Settings page, its search, "modified"
 * indicators, reset-to-default and inline validation are all driven from
 * here.
 *
 * Keys and defaults mirror `strata_store::Settings` (`crates/strata-store/
 * src/settings.rs`) and the app-level checks in `src-tauri`'s settings
 * commands; the backend validates again on save and its issues win.
 */
import { formatBytes, formatCount } from "../lib/format";
import type { Settings } from "../lib/settings";

// -----------------------------------------------------------------------------
// Keys
// -----------------------------------------------------------------------------

/** A settings section (`scan`, `live`, …). */
export type SectionKey = keyof Settings;

/** A leaf key, `section.field`, as the store names it. */
export type SettingKey = { [S in SectionKey]: `${S}.${Extract<keyof Settings[S], string>}` }[SectionKey];

/** The value type of a leaf key. */
export type SettingValue<K extends SettingKey> = K extends `${infer S}.${infer F}`
  ? S extends SectionKey
    ? F extends keyof Settings[S]
      ? Settings[S][F]
      : never
    : never
  : never;

/**
 * Stored keys deliberately absent from the page.
 *
 * `privacy.crash_reports_opt_in` is written by older builds; Strata has no
 * crash reporting, so there is nothing for it to turn on.
 */
export const UNLISTED_KEYS: readonly string[] = ["privacy.crash_reports_opt_in"];

// -----------------------------------------------------------------------------
// Categories
// -----------------------------------------------------------------------------

/** Settings categories, in navigation order. */
export type CategoryId =
  | "general"
  | "scanning"
  | "live"
  | "cleanup"
  | "duplicates"
  | "activity"
  | "appearance"
  | "rules"
  | "startup"
  | "helper"
  | "updates"
  | "privacy"
  | "about";

/** One category of the Settings page. */
export interface SettingsCategory {
  id: CategoryId;
  title: string;
  /** One sentence under the category heading. */
  description: string;
}

/** Every category in navigation order. */
export const SETTINGS_CATEGORIES: readonly SettingsCategory[] = [
  { id: "general", title: "General", description: "How sizes are measured and shown everywhere in Strata." },
  { id: "scanning", title: "Scanning", description: "What gets scanned, when, and what is skipped." },
  { id: "live", title: "Live updates", description: "Keeping the index current as files change, without rescanning." },
  { id: "cleanup", title: "Cleanup & safety", description: "Defaults and safeguards for removing files." },
  { id: "duplicates", title: "Duplicates", description: "How the duplicate finder chooses what to compare." },
  { id: "activity", title: "Activity tracking", description: "Recording which programs write to disk. Off unless you turn it on." },
  { id: "appearance", title: "Appearance", description: "Theme, colors and density." },
  { id: "rules", title: "Rules", description: "The rules that classify files, and your own rule packs." },
  { id: "startup", title: "Startup & tray", description: "Launching with Windows and the notification-area icon." },
  { id: "helper", title: "Helper & elevation", description: "How Strata gets administrator access for fast scans and live updates." },
  { id: "updates", title: "Updates", description: "Which releases to get and how they install." },
  { id: "privacy", title: "Data & privacy", description: "History retention, clearing Strata’s data, and what leaves this PC." },
  { id: "about", title: "About", description: "Version, licenses and links." },
];

// -----------------------------------------------------------------------------
// Controls and badges
// -----------------------------------------------------------------------------

/** A byte unit offered by a threshold control (binary multiples, as Explorer). */
export type ByteUnit = "KB" | "MB" | "GB" | "TB";

/** Multiplier of each byte unit. */
export const BYTE_UNITS: Readonly<Record<ByteUnit, number>> = { KB: 1024, MB: 1024 ** 2, GB: 1024 ** 3, TB: 1024 ** 4 };

/** One option of a select control. */
export interface SelectOption {
  value: string;
  label: string;
}

/** How a setting is edited. */
export type SettingControl =
  | { kind: "toggle" }
  | { kind: "select"; options: readonly SelectOption[] }
  | {
      kind: "number";
      /** Unit shown after the field (`ms`, `days`, `%`). */
      unit: string;
      min: number;
      max: number;
      step: number;
      integer: boolean;
      /** What 0 means, when it is special ("Automatic"). */
      zeroMeans?: string;
    }
  | { kind: "bytes"; min: number; max: number; units: readonly ByteUnit[] }
  | { kind: "path"; defaultLabel: string; placeholder: string }
  | { kind: "globs"; maxItems: number; maxBytes: number; placeholder: string };

/** Badges shown next to a setting's title. */
export type SettingBadge = "admin" | "restart" | "next-scan" | "advanced";

/** Badge labels and explanations. */
export const BADGES: Readonly<Record<SettingBadge, { label: string; hint: string }>> = {
  admin: { label: "Requires admin", hint: "Uses the elevated helper; Windows asks for administrator permission when needed." },
  restart: { label: "Applies on restart", hint: "Takes effect the next time Strata starts." },
  "next-scan": { label: "Applies on next scan", hint: "Existing indexes keep their current contents until rescanned." },
  advanced: { label: "Advanced", hint: "The default suits almost everyone." },
};

/** One setting. */
export interface SettingDef {
  key: SettingKey;
  category: CategoryId;
  /** Optional sub-heading within the category. */
  group?: string;
  title: string;
  /** One sentence. */
  description: string;
  control: SettingControl;
  default: unknown;
  badges: readonly SettingBadge[];
  /** Extra search terms. */
  keywords: readonly string[];
  /**
   * Checks beyond the control's own limits (cross-field rules).
   *
   * @returns The message, or `null` when valid.
   */
  validate?: (value: unknown, all: Settings) => string | null;
}

interface DefInput<K extends SettingKey> {
  key: K;
  category: CategoryId;
  group?: string;
  title: string;
  description: string;
  control: SettingControl;
  default: SettingValue<K>;
  badges?: readonly SettingBadge[];
  keywords?: readonly string[];
  validate?: (value: SettingValue<K>, all: Settings) => string | null;
}

function def<K extends SettingKey>(d: DefInput<K>): SettingDef {
  const { validate, badges, keywords, ...rest } = d;
  return {
    ...rest,
    badges: badges ?? [],
    keywords: keywords ?? [],
    ...(validate ? { validate: (v: unknown, all: Settings) => validate(v as SettingValue<K>, all) } : {}),
  };
}

const days = (min: number, max: number): SettingControl => ({ kind: "number", unit: "days", min, max, step: 1, integer: true });
const GIB = 1024 ** 3;
const MIB = 1024 ** 2;

/** The smallest low-space threshold the app accepts (`MIN_LOW_SPACE_BYTES`). */
export const MIN_LOW_SPACE_BYTES = 100 * MIB;

// -----------------------------------------------------------------------------
// Registry
// -----------------------------------------------------------------------------

/** Every user-facing setting, in page order. */
export const SETTINGS: readonly SettingDef[] = [
  // General
  def({
    key: "scan.default_size_mode",
    category: "general",
    title: "Default size",
    description: "Which size every view shows when Strata starts: the space files take on disk, or the size they report.",
    control: {
      kind: "select",
      options: [
        { value: "allocated", label: "Size on disk (allocated)" },
        { value: "logical", label: "File size (logical)" },
      ],
    },
    default: "allocated",
    keywords: ["allocated", "logical", "size mode"],
  }),
  def({
    key: "appearance.units",
    category: "general",
    title: "Size units",
    description: "Binary units match File Explorer (1 KB = 1,024 bytes); SI units count in powers of 1,000.",
    control: {
      kind: "select",
      options: [
        { value: "binary", label: "Binary, shown as KB, MB, GB" },
        { value: "decimal", label: "SI: kB, MB, GB (1,000)" },
      ],
    },
    default: "binary",
    keywords: ["kib", "mib", "gib", "decimal", "1024", "1000"],
  }),
  def({
    key: "scan.show_follow_policy",
    category: "general",
    title: "Show the link policy notice",
    description: "Reminds you that scans never follow junctions, symbolic links or mount points, so nothing is counted twice.",
    control: { kind: "toggle" },
    default: true,
    keywords: ["junction", "symlink", "reparse"],
  }),

  // Scanning
  def({
    key: "scan.auto_scan_on_launch",
    category: "scanning",
    title: "Scan the system drive at startup",
    description: "Refreshes the system drive’s index each time Strata starts.",
    control: { kind: "toggle" },
    default: true,
    keywords: ["launch", "automatic"],
  }),
  def({
    key: "scan.auto_scan_removable",
    category: "scanning",
    title: "Scan removable drives when connected",
    description: "Starts a scan when a USB drive or memory card is plugged in.",
    control: { kind: "toggle" },
    default: false,
    keywords: ["usb", "sd card", "plug"],
  }),
  def({
    key: "scan.include_network_drives",
    category: "scanning",
    title: "Show network drives",
    description: "Lists mapped network drives with your volumes so you can scan them; scanning a share reads it over the network.",
    control: { kind: "toggle" },
    default: false,
    keywords: ["smb", "share", "nas", "mapped"],
  }),
  def({
    key: "scan.exclude_globs",
    category: "scanning",
    title: "Excluded paths",
    description: "Files and folders matching these patterns are skipped by every scan; ** matches any number of folders.",
    control: { kind: "globs", maxItems: 1000, maxBytes: 1024, placeholder: "D:\\Backups\\**" },
    default: [],
    badges: ["next-scan"],
    keywords: ["exclude", "ignore", "skip", "glob", "pattern"],
  }),
  def({
    key: "scan.walker_concurrency",
    category: "scanning",
    title: "Fallback scanner threads",
    description: "Threads used when Strata can’t read the file table directly (standard scans, ReFS and FAT drives).",
    control: { kind: "number", unit: "threads", min: 0, max: 256, step: 1, integer: true, zeroMeans: "Automatic" },
    default: 0,
    badges: ["next-scan", "advanced"],
    keywords: ["concurrency", "parallel", "walker", "performance"],
  }),

  // Live updates
  def({
    key: "live.usn_enabled",
    category: "live",
    title: "Keep the index live",
    description: "Follows the NTFS change journal so sizes update as files change, without rescanning.",
    control: { kind: "toggle" },
    default: true,
    badges: ["admin"],
    keywords: ["usn", "journal", "real-time", "watch"],
  }),
  def({
    key: "live.update_tick_ms",
    category: "live",
    title: "Update interval",
    description: "How often collected changes reach the views; a longer interval uses less CPU during heavy disk activity.",
    control: { kind: "number", unit: "ms", min: 100, max: 60_000, step: 100, integer: true },
    default: 1000,
    badges: ["advanced"],
    keywords: ["tick", "refresh", "latency"],
  }),
  def({
    key: "live.auto_rescan_on_journal_loss",
    category: "live",
    title: "Rescan when changes were missed",
    description: "If the change journal wrapped or was reset while Strata wasn’t watching, rescan instead of marking the index stale.",
    control: { kind: "toggle" },
    default: true,
    keywords: ["stale", "journal", "wrap"],
  }),

  // Cleanup & safety
  def({
    key: "cleanup.default_method",
    category: "cleanup",
    title: "Default delete method",
    description: "Preselected on the review screen; you can still change it for each cleanup.",
    control: {
      kind: "select",
      options: [
        { value: "recycle_bin", label: "Move to the Recycle Bin (restorable)" },
        { value: "permanent", label: "Delete permanently" },
      ],
    },
    default: "recycle_bin",
    keywords: ["recycle bin", "permanent", "delete"],
  }),
  def({
    key: "cleanup.large_delete_confirm_bytes",
    category: "cleanup",
    title: "Extra confirmation above",
    description: "Permanent deletes larger than this ask for a second confirmation.",
    control: { kind: "bytes", min: 1, max: Number.MAX_SAFE_INTEGER, units: ["MB", "GB", "TB"] },
    default: 10 * GIB,
    keywords: ["confirm", "large", "threshold"],
  }),
  def({
    key: "cleanup.stale_node_modules_days",
    category: "cleanup",
    group: "Suggestions",
    title: "Stale node_modules after",
    description: "Suggests node_modules folders for cleanup once they have been untouched this long.",
    control: days(1, 3650),
    default: 90,
    keywords: ["javascript", "npm", "developer"],
  }),
  def({
    key: "cleanup.stale_installers_days",
    category: "cleanup",
    group: "Suggestions",
    title: "Old installers after",
    description: "Installer files (.exe, .msi) untouched this long are suggested for cleanup.",
    control: days(1, 3650),
    default: 60,
    keywords: ["setup", "msi", "downloads"],
  }),

  // Duplicates
  def({
    key: "cleanup.duplicates_min_bytes",
    category: "duplicates",
    title: "Minimum file size",
    description: "Files smaller than this are ignored by the duplicate finder, which keeps scans fast.",
    control: { kind: "bytes", min: 1, max: Number.MAX_SAFE_INTEGER, units: ["KB", "MB", "GB"] },
    default: MIB,
    keywords: ["dupes", "copies", "hash"],
  }),

  // Activity tracking
  def({
    key: "activity.enabled",
    category: "activity",
    title: "Track disk activity",
    description: "Records which programs write to which folders using Windows event tracing; the data stays on this PC.",
    control: { kind: "toggle" },
    default: false,
    badges: ["admin"],
    keywords: ["etw", "writers", "programs", "monitor"],
  }),
  def({
    key: "activity.retention_days",
    category: "activity",
    title: "Keep activity for",
    description: "Hourly activity older than this is deleted during daily maintenance.",
    control: days(1, 365),
    default: 30,
    keywords: ["retention", "history"],
  }),
  def({
    key: "activity.cpu_cap_percent",
    category: "activity",
    title: "CPU limit",
    description: "Tracing throttles itself when it uses more than this share of CPU for a sustained period.",
    control: { kind: "number", unit: "%", min: 0.1, max: 50, step: 0.1, integer: false },
    default: 2,
    badges: ["advanced"],
    keywords: ["throttle", "performance", "cpu"],
  }),

  // Appearance
  def({
    key: "appearance.theme",
    category: "appearance",
    title: "Theme",
    description: "Follow the Windows light or dark setting, or always use one.",
    control: {
      kind: "select",
      options: [
        { value: "system", label: "Follow Windows" },
        { value: "light", label: "Light" },
        { value: "dark", label: "Dark" },
      ],
    },
    default: "system",
    keywords: ["dark mode", "light mode", "color scheme"],
  }),
  def({
    key: "appearance.color_mode",
    category: "appearance",
    title: "Default colors",
    description: "What block colors show when a map opens; the toolbar switches them at any time.",
    control: {
      kind: "select",
      options: [
        { value: "category", label: "Category" },
        { value: "file_type", label: "File type" },
        { value: "age", label: "Age" },
        { value: "app", label: "Owning app" },
        { value: "safety", label: "Safety tier" },
      ],
    },
    default: "category",
    keywords: ["color mode", "legend"],
  }),
  def({
    key: "appearance.treemap_style",
    category: "appearance",
    title: "Treemap style",
    description: "Flat fills, or cushion shading that makes nesting easier to see.",
    control: {
      kind: "select",
      options: [
        { value: "cushion", label: "Cushion" },
        { value: "flat", label: "Flat" },
      ],
    },
    default: "cushion",
    keywords: ["shading"],
  }),
  def({
    key: "appearance.compact_density",
    category: "appearance",
    title: "Compact rows",
    description: "Tighter rows in lists and tables, to show more at once.",
    control: { kind: "toggle" },
    default: false,
    keywords: ["density", "dense"],
  }),

  // Rules
  def({
    key: "rules.user_rules_enabled",
    category: "rules",
    title: "Load my rule packs",
    description: "Adds your own TOML rule packs from the user rules folder; they can refine built-in rules but never make protected items deletable.",
    control: { kind: "toggle" },
    default: true,
    keywords: ["toml", "custom rules", "classifier"],
  }),
  def({
    key: "rules.user_rules_dir",
    category: "rules",
    title: "User rules folder",
    description: "Where Strata looks for your rule packs; the default is a folder in your app data.",
    control: { kind: "path", defaultLabel: "Default location (in your app data)", placeholder: "C:\\Users\\me\\Documents\\Strata rules" },
    default: null,
    keywords: ["folder", "directory", "location"],
    validate: (v) => (v !== null && v.trim() === "" ? "Enter a folder, or use the default location." : null),
  }),

  // Startup & tray
  def({
    key: "startup.launch_at_login",
    category: "startup",
    title: "Start with Windows",
    description: "Opens Strata when you sign in.",
    control: { kind: "toggle" },
    default: false,
    keywords: ["autostart", "login", "boot"],
  }),
  def({
    key: "startup.start_minimized_to_tray",
    category: "startup",
    title: "Start in the notification area",
    description: "When Strata starts with Windows, it waits in the notification area instead of opening a window.",
    control: { kind: "toggle" },
    default: false,
    badges: ["restart"],
    keywords: ["minimized", "hidden", "tray"],
    validate: (v, all) => (v && !all.tray.enabled ? "Needs “Show the tray icon” turned on." : null),
  }),
  def({
    key: "tray.enabled",
    category: "startup",
    group: "Tray icon",
    title: "Show the tray icon",
    description: "Shows free space at a glance and quick actions in the notification area.",
    control: { kind: "toggle" },
    default: false,
    keywords: ["notification area", "system tray"],
  }),
  def({
    key: "tray.low_space_notification",
    category: "startup",
    group: "Tray icon",
    title: "Warn when space runs low",
    description: "Shows a notification when a fixed drive’s free space drops below the threshold.",
    control: { kind: "toggle" },
    default: true,
    keywords: ["alert", "toast", "free space"],
  }),
  def({
    key: "tray.low_space_threshold_bytes",
    category: "startup",
    group: "Tray icon",
    title: "Low space threshold",
    description: "A drive counts as low on space when less than this is free.",
    control: { kind: "bytes", min: MIN_LOW_SPACE_BYTES, max: Number.MAX_SAFE_INTEGER, units: ["MB", "GB", "TB"] },
    default: 10 * GIB,
    keywords: ["free space", "alert"],
  }),

  // Helper & elevation
  def({
    key: "helper.mode",
    category: "helper",
    title: "Helper mode",
    description: "Ask for administrator permission each time it is needed, or run a small Windows service so scans never prompt.",
    control: {
      kind: "select",
      options: [
        { value: "on_demand", label: "On demand (UAC prompt when needed)" },
        { value: "service", label: "Windows service (no prompts)" },
      ],
    },
    default: "on_demand",
    badges: ["admin"],
    keywords: ["uac", "elevation", "service", "administrator", "fast scan", "mft"],
  }),

  // Updates
  def({
    key: "updates.channel",
    category: "updates",
    title: "Update channel",
    description: "Stable gets tested releases; Beta gets pre-release builds earlier.",
    control: {
      kind: "select",
      options: [
        { value: "stable", label: "Stable" },
        { value: "beta", label: "Beta" },
      ],
    },
    default: "stable",
    keywords: ["prerelease", "version"],
  }),
  def({
    key: "updates.auto_download",
    category: "updates",
    title: "Download updates automatically",
    description: "Downloads signed updates in the background; they install when you exit Strata.",
    control: { kind: "toggle" },
    default: true,
    badges: ["restart"],
    keywords: ["auto update", "install"],
  }),

  // Data & privacy
  def({
    key: "history.snapshot_interval_hours",
    category: "privacy",
    group: "History",
    title: "Snapshot interval",
    description: "How often Strata records folder sizes while a volume is live, for charts and “what changed”.",
    control: { kind: "number", unit: "hours", min: 1, max: 720, step: 1, integer: true },
    default: 24,
    keywords: ["snapshot", "history", "frequency"],
  }),
  def({
    key: "history.retention_days",
    category: "privacy",
    group: "History",
    title: "Keep snapshots for",
    description: "Snapshots older than this are deleted during daily maintenance.",
    control: days(7, 3650),
    default: 90,
    keywords: ["retention", "history"],
  }),
  def({
    key: "history.thin_after_days",
    category: "privacy",
    group: "History",
    title: "Thin snapshots after",
    description: "Snapshots older than this are reduced to one per week to save space.",
    control: days(0, 3650),
    default: 30,
    keywords: ["retention", "weekly"],
    validate: (v, all) => (v > all.history.retention_days ? `Must not be longer than “Keep snapshots for” (${formatCount(all.history.retention_days)} days).` : null),
  }),
  def({
    key: "history.min_dir_bytes",
    category: "privacy",
    group: "History",
    title: "Smallest folder recorded",
    description: "Folders smaller than this are left out of snapshots, which keeps history small.",
    control: { kind: "bytes", min: 0, max: Number.MAX_SAFE_INTEGER, units: ["KB", "MB", "GB"] },
    default: 16 * MIB,
    keywords: ["snapshot", "threshold"],
  }),
];

const BY_KEY = new Map(SETTINGS.map((s) => [s.key, s]));

/** Looks up a setting by key. */
export function settingDef(key: string): SettingDef | undefined {
  return BY_KEY.get(key as SettingKey);
}

// -----------------------------------------------------------------------------
// Values
// -----------------------------------------------------------------------------

function split(key: string): [string, string] {
  const i = key.indexOf(".");
  return [key.slice(0, i), key.slice(i + 1)];
}

/**
 * Reads a leaf value.
 *
 * @param s - Settings.
 * @param key - `section.field`.
 */
export function getSetting(s: Settings, key: SettingKey): unknown {
  const [section, field] = split(key);
  return (s as unknown as Record<string, Record<string, unknown> | undefined>)[section]?.[field];
}

/**
 * Returns a copy with one leaf replaced.
 *
 * @param s - Settings.
 * @param key - `section.field`.
 * @param value - New value.
 */
export function withSetting(s: Settings, key: SettingKey, value: unknown): Settings {
  const [section, field] = split(key);
  const rec = s as unknown as Record<string, Record<string, unknown>>;
  return { ...s, [section]: { ...rec[section], [field]: value } };
}

/** Deep equality for setting values (scalars, arrays of strings, null). */
export function sameValue(a: unknown, b: unknown): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

/** Whether a setting differs from its default. */
export function isModified(s: Settings, d: SettingDef): boolean {
  return !sameValue(getSetting(s, d.key), d.default);
}

/**
 * Builds the defaults of every registered setting.
 *
 * @returns `section → field → default`.
 */
export function registryDefaults(): Record<string, Record<string, unknown>> {
  const out: Record<string, Record<string, unknown>> = {};
  for (const d of SETTINGS) {
    const [section, field] = split(d.key);
    (out[section] ??= {})[field] = d.default;
  }
  return out;
}

/**
 * Leaf keys of a settings-shaped object.
 *
 * @param s - Any `section → field` object.
 * @returns `section.field` keys.
 */
export function leafKeys(s: object): string[] {
  return Object.entries(s).flatMap(([section, fields]) => Object.keys(fields as object).map((f) => `${section}.${f}`));
}

// -----------------------------------------------------------------------------
// Validation
// -----------------------------------------------------------------------------

function num(n: number, fractional: boolean): string {
  return fractional ? n.toLocaleString("en-US", { maximumFractionDigits: 1 }) : formatCount(n);
}

/**
 * Validates one value against its control's limits and the setting's own
 * rules.
 *
 * @param d - Setting.
 * @param value - Candidate value.
 * @param all - The whole candidate settings (for cross-field rules).
 * @returns The exact message shown under the control, or `null`.
 */
export function validateSetting(d: SettingDef, value: unknown, all: Settings): string | null {
  const c = d.control;
  switch (c.kind) {
    case "number": {
      if (typeof value !== "number" || !Number.isFinite(value)) return "Enter a number.";
      if (c.integer && !Number.isInteger(value)) return "Enter a whole number.";
      if (value < c.min || value > c.max) return `Enter a value from ${num(c.min, !c.integer)} to ${num(c.max, !c.integer)} ${c.unit}.`;
      break;
    }
    case "bytes": {
      if (typeof value !== "number" || !Number.isFinite(value) || value < 0) return "Enter a size.";
      if (value < c.min) return c.min <= 1 ? "Enter a size larger than zero." : `Enter at least ${formatBytes(c.min)}.`;
      if (value > c.max) return "That size is too large.";
      break;
    }
    case "globs": {
      const list = value as readonly string[];
      if (list.length > c.maxItems) return `Use at most ${formatCount(c.maxItems)} patterns.`;
      if (list.some((g) => g.trim() === "")) return "Patterns can’t be empty.";
      if (list.some((g) => new TextEncoder().encode(g).length > c.maxBytes)) return `Patterns can be at most ${formatCount(c.maxBytes)} bytes long.`;
      break;
    }
    case "toggle":
    case "select":
    case "path":
      break;
  }
  return d.validate?.(value, all) ?? null;
}

/**
 * Validates every registered setting.
 *
 * @param s - Candidate settings.
 * @returns Messages by key; empty when valid.
 */
export function validateAll(s: Settings): Map<SettingKey, string> {
  const out = new Map<SettingKey, string>();
  for (const d of SETTINGS) {
    const msg = validateSetting(d, getSetting(s, d.key), s);
    if (msg) out.set(d.key, msg);
  }
  return out;
}

// -----------------------------------------------------------------------------
// Search
// -----------------------------------------------------------------------------

/** A parsed search query. */
export interface SettingsQuery {
  /** Lower-cased words; every word must match somewhere. */
  words: string[];
  /** `@modified`: only settings that differ from their default. */
  modifiedOnly: boolean;
}

/**
 * Parses the search box text.
 *
 * @param text - What the user typed.
 */
export function parseQuery(text: string): SettingsQuery {
  const words: string[] = [];
  let modifiedOnly = false;
  for (const w of text.toLowerCase().split(/\s+/)) {
    if (w === "") continue;
    if (w === "@modified") modifiedOnly = true;
    else words.push(w);
  }
  return { words, modifiedOnly };
}

const CATEGORY_TITLE = new Map(SETTINGS_CATEGORIES.map((c) => [c.id, c.title]));

/**
 * Whether a setting matches a query (titles, descriptions, keys, category,
 * group and keywords).
 *
 * @param d - Setting.
 * @param q - Parsed query.
 * @param s - Current settings, for `@modified`.
 */
export function matchesQuery(d: SettingDef, q: SettingsQuery, s: Settings | null): boolean {
  if (q.modifiedOnly && (!s || !isModified(s, d))) return false;
  if (q.words.length === 0) return true;
  const hay = [d.title, d.description, d.key, CATEGORY_TITLE.get(d.category) ?? "", d.group ?? "", ...d.keywords].join("\n").toLowerCase();
  return q.words.every((w) => hay.includes(w));
}

/**
 * Splits text into runs for highlighting the query words.
 *
 * @param text - Text to show.
 * @param words - Lower-cased query words.
 * @returns Runs; `hit` runs match a word.
 */
export function highlightRuns(text: string, words: readonly string[]): { text: string; hit: boolean }[] {
  if (words.length === 0) return [{ text, hit: false }];
  const lower = text.toLowerCase();
  const marks = new Array<boolean>(text.length).fill(false);
  for (const w of words) {
    for (let i = lower.indexOf(w); i >= 0; i = lower.indexOf(w, i + 1)) marks.fill(true, i, i + w.length);
  }
  const runs: { text: string; hit: boolean }[] = [];
  for (let i = 0; i < text.length; i++) {
    const last = runs[runs.length - 1];
    if (last && last.hit === marks[i]) last.text += text.charAt(i);
    else runs.push({ text: text.charAt(i), hit: marks[i] === true });
  }
  return runs;
}

/**
 * Picks the display unit for a byte value: the largest offered unit that
 * shows it as at least 1.
 *
 * @param bytes - Value.
 * @param units - Offered units, smallest first.
 */
export function bestByteUnit(bytes: number, units: readonly ByteUnit[]): ByteUnit {
  let best = units[0] ?? "MB";
  for (const u of units) if (bytes >= BYTE_UNITS[u]) best = u;
  return best;
}
