/**
 * Settings (SPEC §19), rules tooling, data clearing, helper service mode and
 * About.
 *
 * {@link Settings} is exactly the serde shape of `strata_store::Settings`
 * (snake_case), so the backend passes it through unchanged and validation
 * issue keys (`live.update_tick_ms`) name fields directly. Validation is
 * mirrored here ({@link validateSettings}) for instant feedback; the backend
 * validates again on save and import and its issues win.
 */
import { call } from "./backend";
import type { Safety, SizeMode } from "./types";

// -----------------------------------------------------------------------------
// Settings shape
// -----------------------------------------------------------------------------

/** Every user setting (`strata_store::Settings`). */
export interface Settings {
  scan: {
    default_size_mode: SizeMode;
    exclude_globs: string[];
    include_network_drives: boolean;
    auto_scan_on_launch: boolean;
    auto_scan_removable: boolean;
    /** 0 picks automatically. */
    walker_concurrency: number;
    show_follow_policy: boolean;
  };
  live: { usn_enabled: boolean; update_tick_ms: number; auto_rescan_on_journal_loss: boolean };
  activity: { enabled: boolean; retention_days: number; cpu_cap_percent: number };
  helper: { mode: "on_demand" | "service" };
  cleanup: {
    default_method: "recycle_bin" | "permanent";
    large_delete_confirm_bytes: number;
    stale_node_modules_days: number;
    stale_installers_days: number;
    duplicates_min_bytes: number;
  };
  history: { snapshot_interval_hours: number; retention_days: number; thin_after_days: number; min_dir_bytes: number };
  appearance: {
    theme: "system" | "light" | "dark";
    color_mode: "category" | "age" | "file_type" | "safety" | "app";
    treemap_style: "flat" | "cushion";
    units: "binary" | "decimal";
    compact_density: boolean;
  };
  rules: { user_rules_enabled: boolean; user_rules_dir: string | null };
  startup: { launch_at_login: boolean; start_minimized_to_tray: boolean };
  tray: { enabled: boolean; low_space_notification: boolean; low_space_threshold_bytes: number };
  updates: { channel: "stable" | "beta"; auto_download: boolean };
  privacy: { crash_reports_opt_in: boolean };
}

/** One validation failure (`strata_store::SettingsIssue`). */
export interface SettingsIssue {
  /** Leaf key, e.g. `live.update_tick_ms`. */
  key: string;
  message: string;
}

/** Result of saving or importing. */
export interface SettingsResult {
  /** The settings as stored (after save) or as imported. */
  settings: Settings;
  /** Non-empty means nothing was written. */
  issues: SettingsIssue[];
}

/** Loads settings (`settings_load`). */
export function loadSettings(): Promise<Settings> {
  return call<Settings>("settings_load");
}

/** Validates and saves (`settings_save`). */
export function saveSettings(settings: Settings): Promise<SettingsResult> {
  return call<SettingsResult>("settings_save", { settings });
}

/** Exports to the `strata-settings` JSON envelope (`settings_export`). */
export function exportSettings(): Promise<string> {
  return call<string>("settings_export");
}

/** Validates then imports an export (`settings_import`); writes nothing when invalid. */
export function importSettings(json: string): Promise<SettingsResult> {
  return call<SettingsResult>("settings_import", { json });
}

// -----------------------------------------------------------------------------
// Validation (mirror of `Settings::validate`)
// -----------------------------------------------------------------------------

function range(out: SettingsIssue[], key: string, v: number, min: number, max: number): void {
  if (!Number.isFinite(v) || v < min || v > max) out.push({ key, message: `${v} is outside ${min}..=${max}` });
}

/**
 * Checks ranges and cross-field constraints exactly like the Rust store.
 *
 * @param s - Settings.
 * @returns Issues; empty means valid.
 */
export function validateSettings(s: Settings): SettingsIssue[] {
  const out: SettingsIssue[] = [];
  const globs = s.scan.exclude_globs;
  if (globs.length > 1000) out.push({ key: "scan.exclude_globs", message: "more than 1000 patterns" });
  if (globs.some((g) => g.trim() === "" || new TextEncoder().encode(g).length > 1024)) {
    out.push({ key: "scan.exclude_globs", message: "patterns must be non-empty and at most 1024 bytes" });
  }
  range(out, "scan.walker_concurrency", s.scan.walker_concurrency, 0, 256);
  range(out, "live.update_tick_ms", s.live.update_tick_ms, 100, 60_000);
  range(out, "activity.retention_days", s.activity.retention_days, 1, 365);
  const cap = s.activity.cpu_cap_percent;
  if (!Number.isFinite(cap) || cap < 0.1 || cap > 50) out.push({ key: "activity.cpu_cap_percent", message: "must be within 0.1..=50" });
  range(out, "cleanup.large_delete_confirm_bytes", s.cleanup.large_delete_confirm_bytes, 1, Number.MAX_SAFE_INTEGER);
  range(out, "cleanup.stale_node_modules_days", s.cleanup.stale_node_modules_days, 1, 3650);
  range(out, "cleanup.stale_installers_days", s.cleanup.stale_installers_days, 1, 3650);
  range(out, "cleanup.duplicates_min_bytes", s.cleanup.duplicates_min_bytes, 1, Number.MAX_SAFE_INTEGER);
  range(out, "history.snapshot_interval_hours", s.history.snapshot_interval_hours, 1, 24 * 30);
  range(out, "history.retention_days", s.history.retention_days, 7, 3650);
  if (s.history.thin_after_days > s.history.retention_days) {
    out.push({ key: "history.thin_after_days", message: "must not exceed history.retention_days" });
  }
  if (s.rules.user_rules_dir !== null && s.rules.user_rules_dir.trim() === "") {
    out.push({ key: "rules.user_rules_dir", message: "must not be empty" });
  }
  return out;
}

/**
 * Lists leaf keys whose values differ.
 *
 * @param a - Before.
 * @param b - After.
 * @returns `section.field` keys.
 */
export function changedKeys(a: Settings, b: Settings): string[] {
  const out: string[] = [];
  for (const section of Object.keys(b) as (keyof Settings)[]) {
    const sa = a[section] as Record<string, unknown>;
    const sb = b[section] as Record<string, unknown>;
    for (const field of Object.keys(sb)) {
      if (JSON.stringify(sa[field]) !== JSON.stringify(sb[field])) out.push(`${section}.${field}`);
    }
  }
  return out;
}

// -----------------------------------------------------------------------------
// Rules
// -----------------------------------------------------------------------------

/** A rule as listed in settings (`strata_classify::RuleSummary`). */
export interface RuleInfo {
  id: string;
  name: string;
  pack: string;
  source: "builtin" | "user";
  category: number;
  safety: Safety;
  explain: string;
  action: "delete" | "open_tool" | "info_only";
  app: string | null;
  regenerable: boolean;
  /** Id of the user rule overriding this built-in, or `null`. */
  overriddenBy: string | null;
}

/** Result of reloading rule packs. */
export interface RulesReload {
  builtin: number;
  user: number;
  /** User-pack problems (bad TOML, unknown keys, refused never overrides). */
  problems: { file: string; message: string }[];
}

/** One step of an explanation (`explain::ExplainStep`). */
export interface ExplainStep {
  path: string;
  ruleId: string | null;
  category: number;
  safety: Safety;
}

/** "Why is this classified as X?" (`Classifier::explain`). */
export interface Explanation {
  path: string;
  result: { category: number; safety: Safety; ruleId: string | null; regenerable: boolean };
  rule: RuleInfo | null;
  /** Ancestor the classification was inherited from, or `null` when it matched the path itself. */
  originPath: string | null;
  /** Classification at each ancestor from the root down. */
  steps: ExplainStep[];
  /** Candidate rules considered and why each won or lost, in order. */
  trace: string[];
}

/** Lists built-in and user rules (`rules_list`). */
export function fetchRules(): Promise<RuleInfo[]> {
  return call<RuleInfo[]>("rules_list");
}

/** Opens the user rules folder in Explorer (`rules_open_folder`). */
export function openRulesFolder(): Promise<null> {
  return call<null>("rules_open_folder");
}

/** Reloads rule packs and reclassifies (`rules_reload`). */
export function reloadRules(): Promise<RulesReload> {
  return call<RulesReload>("rules_reload");
}

/** Explains a path's classification (`rules_explain`); reads metadata only. */
export function explainPath(path: string): Promise<Explanation> {
  return call<Explanation>("rules_explain", { path });
}

// -----------------------------------------------------------------------------
// Data, helper service, about
// -----------------------------------------------------------------------------

/** App data that can be cleared (never user files). */
export type ClearableData = "history" | "activity" | "caches";

/** Clears Strata's own data (`data_clear`). */
export function clearData(what: ClearableData): Promise<null> {
  return call<null>("data_clear", { what });
}

/** Helper service state. */
export interface HelperServiceStatus {
  installed: boolean;
  running: boolean;
}

/** Reads the service state (`helper_service_status`). */
export function fetchHelperService(): Promise<HelperServiceStatus> {
  return call<HelperServiceStatus>("helper_service_status");
}

/** Installs the helper service (`helper_service_install`, UAC prompt). */
export function installHelperService(): Promise<HelperServiceStatus> {
  return call<HelperServiceStatus>("helper_service_install");
}

/** Removes the helper service (`helper_service_uninstall`, UAC prompt). */
export function uninstallHelperService(): Promise<HelperServiceStatus> {
  return call<HelperServiceStatus>("helper_service_uninstall");
}

/** One third-party component. */
export interface LicenseEntry {
  name: string;
  version: string;
  /** SPDX expression. */
  license: string;
  ecosystem: "cargo" | "npm";
  repository: string | null;
}

/** Third-party licenses (`about_licenses`). */
export function fetchLicenses(): Promise<LicenseEntry[]> {
  return call<LicenseEntry[]>("about_licenses");
}

/** Update check (`updates_check`). */
export interface UpdateCheck {
  current: string;
  latest: string | null;
  available: boolean;
  channel: "stable" | "beta";
}

/** Checks for an update on the configured channel (`updates_check`). */
export function checkForUpdates(): Promise<UpdateCheck> {
  return call<UpdateCheck>("updates_check");
}
