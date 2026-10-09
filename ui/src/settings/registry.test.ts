// @vitest-environment node
import { describe, expect, it } from "vitest";
import { validateSettings, type Settings } from "../lib/settings";
import { defaultSettings } from "../test/features";
import {
  SETTINGS,
  SETTINGS_CATEGORIES,
  UNLISTED_KEYS,
  getSetting,
  highlightRuns,
  isModified,
  leafKeys,
  matchesQuery,
  parseQuery,
  registryDefaults,
  settingDef,
  validateAll,
  validateSetting,
  withSetting,
  type SettingKey,
} from "./registry";

const listed = (keys: string[]) => keys.filter((k) => !UNLISTED_KEYS.includes(k)).sort();

describe("settings registry", () => {
  it("covers every key of the default settings and nothing extra", () => {
    const registryKeys = SETTINGS.map((d) => d.key as string).sort();
    expect(registryKeys).toEqual(listed(leafKeys(defaultSettings())));
    expect(new Set(registryKeys).size).toBe(registryKeys.length);
  });

  it("never lists the retired crash-reporting key", () => {
    expect(settingDef("privacy.crash_reports_opt_in")).toBeUndefined();
    expect(SETTINGS.some((d) => /crash/i.test(`${d.key} ${d.title} ${d.description}`))).toBe(false);
  });

  it("has the store's default for every key", () => {
    const defaults = defaultSettings();
    for (const d of SETTINGS) expect(d.default, d.key).toEqual(getSetting(defaults, d.key));
    const built = registryDefaults();
    for (const k of listed(leafKeys(defaults))) {
      const [section, field] = k.split(".") as [string, string];
      expect(built[section]?.[field], k).toEqual(getSetting(defaults, k as SettingKey));
    }
  });

  it("defines every setting completely", () => {
    const categories = new Set(SETTINGS_CATEGORIES.map((c) => c.id));
    for (const d of SETTINGS) {
      expect(categories.has(d.category), d.key).toBe(true);
      expect(d.title.length, d.key).toBeGreaterThan(2);
      expect(d.description, d.key).toMatch(/^[A-Z].*\.$/);
      // One sentence: no full stop followed by another sentence.
      expect(d.description.replace(/\b(e\.g|i\.e)\./g, ""), d.key).not.toMatch(/\.\s+[A-Z]/);
      if (d.control.kind === "number") expect(d.control.min, d.key).toBeLessThan(d.control.max);
      if (d.control.kind === "select") expect(d.control.options.map((o) => o.value), d.key).toContain(d.default);
    }
  });

  it("accepts the defaults", () => {
    expect([...validateAll(defaultSettings())]).toEqual([]);
  });

  it("validates exactly the keys the store validates, with readable messages", () => {
    const bad: [SettingKey, unknown, string][] = [
      ["live.update_tick_ms", 50, "Enter a value from 100 to 60,000 ms."],
      ["scan.walker_concurrency", 300, "Enter a value from 0 to 256 threads."],
      ["activity.retention_days", 0, "Enter a value from 1 to 365 days."],
      ["activity.cpu_cap_percent", 0.05, "Enter a value from 0.1 to 50 %."],
      ["cleanup.stale_node_modules_days", Number.NaN, "Enter a number."],
      ["cleanup.duplicates_min_bytes", 0, "Enter a size larger than zero."],
      ["history.retention_days", 3, "Enter a value from 7 to 3,650 days."],
      ["history.snapshot_interval_hours", 721, "Enter a value from 1 to 720 hours."],
      ["scan.exclude_globs", ["ok/**", " "], "Patterns can’t be empty."],
      ["rules.user_rules_dir", "  ", "Enter a folder, or use the default location."],
    ];
    for (const [key, value, message] of bad) {
      const s = withSetting(defaultSettings(), key, value);
      expect(validateAll(s).get(key), key).toBe(message);
      expect(
        validateSettings(s).map((i) => i.key),
        `store mirror flags ${key}`,
      ).toContain(key);
    }
  });

  it("asks for whole numbers where the store keeps integers", () => {
    expect(validateAll(withSetting(defaultSettings(), "live.update_tick_ms", 150.5)).get("live.update_tick_ms")).toBe("Enter a whole number.");
    expect(validateAll(withSetting(defaultSettings(), "activity.cpu_cap_percent", 2.5)).has("activity.cpu_cap_percent")).toBe(false);
  });

  it("applies cross-field and app-level rules", () => {
    let s = withSetting(defaultSettings(), "history.thin_after_days", 120);
    expect(validateAll(s).get("history.thin_after_days")).toBe("Must not be longer than “Keep snapshots for” (90 days).");
    s = withSetting(defaultSettings(), "startup.start_minimized_to_tray", true);
    expect(validateAll(s).get("startup.start_minimized_to_tray")).toBe("Needs “Show the tray icon” turned on.");
    s = withSetting(s, "tray.enabled", true);
    expect(validateAll(s).has("startup.start_minimized_to_tray")).toBe(false);
    const threshold = settingDef("tray.low_space_threshold_bytes");
    expect(threshold && validateSetting(threshold, 50 * 1024 ** 2, s)).toBe("Enter at least 100 MB.");
  });

  it("reads, writes and detects modified values without mutating", () => {
    const base = defaultSettings();
    const next = withSetting(base, "appearance.theme", "dark");
    expect(base.appearance.theme).toBe("system");
    expect(getSetting(next, "appearance.theme")).toBe("dark");
    expect(next.scan).toBe(base.scan);
    const theme = settingDef("appearance.theme");
    expect(theme && isModified(next, theme)).toBe(true);
    expect(theme && isModified(base, theme)).toBe(false);
  });
});

describe("settings search", () => {
  const s: Settings = withSetting(defaultSettings(), "live.update_tick_ms", 500);

  it("matches titles, descriptions, keys, categories and keywords", () => {
    const keys = (text: string) => SETTINGS.filter((d) => matchesQuery(d, parseQuery(text), s)).map((d) => d.key);
    expect(keys("update interval")).toEqual(["live.update_tick_ms"]);
    expect(keys("update_tick")).toEqual(["live.update_tick_ms"]);
    expect(keys("recycle bin")).toContain("cleanup.default_method");
    expect(keys("usb")).toEqual(["scan.auto_scan_removable"]);
    expect(keys("tray icon")).toEqual(expect.arrayContaining(["tray.enabled", "tray.low_space_threshold_bytes"]));
    expect(keys("zzz nothing")).toEqual([]);
  });

  it("filters to modified settings with @modified", () => {
    const q = parseQuery("@modified");
    expect(q).toEqual({ words: [], modifiedOnly: true });
    expect(SETTINGS.filter((d) => matchesQuery(d, q, s)).map((d) => d.key)).toEqual(["live.update_tick_ms"]);
  });

  it("splits text into highlight runs", () => {
    expect(highlightRuns("Update interval", ["inter"])).toEqual([
      { text: "Update ", hit: false },
      { text: "inter", hit: true },
      { text: "val", hit: false },
    ]);
    expect(highlightRuns("abc", [])).toEqual([{ text: "abc", hit: false }]);
  });
});
