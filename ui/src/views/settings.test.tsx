import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { FeaturesContext } from "../features";
import type { Settings } from "../lib/settings";
import { ServicesContext, type Services } from "../services";
import { useLayout } from "../shell/layout";
import { useSettings } from "../store/settings";
import { defaultSettings, fakeFeatures, resolves, type FeatureOverrides } from "../test/features";
import { resetStores, testServices } from "../test/services";
import { SettingsView } from "./SettingsView";

const GIB = 1024 ** 3;
const SAVE_WAIT = { timeout: 2500 };

function setup(caps: string[], overrides: FeatureOverrides = {}) {
  const services: Services = { ...testServices(), capabilities: () => new Set(caps) };
  render(
    <ServicesContext value={services}>
      <FeaturesContext value={fakeFeatures(overrides)}>
        <SettingsView />
      </FeaturesContext>
    </ServicesContext>,
  );
}

function saving(settings: Settings = defaultSettings()) {
  const saveSettings = vi.fn((s: Settings) => Promise.resolve({ settings: s, issues: [] as { key: string; message: string }[] }));
  return { loadSettings: resolves(settings), saveSettings };
}

const row = (key: string) => document.querySelector<HTMLElement>(`[data-key="${key}"]`) as HTMLElement;

beforeEach(() => {
  resetStores();
  useSettings.setState({ theme: "system", units: "binary", patterns: false });
});

describe("settings page", () => {
  it("shows every category with defined settings, defaults and badges", async () => {
    setup(["settings_load", "settings_save"], { settings: saving() });
    expect(await screen.findByRole("heading", { name: "Settings", level: 1 })).toBeTruthy();
    const nav = screen.getByRole("navigation", { name: "Settings categories" });
    expect(within(nav).getAllByRole("button").map((b) => b.textContent)).toEqual([
      "General",
      "Scanning",
      "Live updates",
      "Cleanup & safety",
      "Duplicates",
      "Activity tracking",
      "Appearance",
      "Rules",
      "Startup & tray",
      "Helper & elevation",
      "Updates",
      "Data & privacy",
      "About",
    ]);
    const tick = row("live.update_tick_ms");
    expect(within(tick).getByText("Default: 1,000 ms")).toBeTruthy();
    expect(within(tick).getByText("Range: 100–60,000 ms")).toBeTruthy();
    expect(within(tick).getByText("live.update_tick_ms")).toBeTruthy();
    expect(within(row("activity.enabled")).getByText("Requires admin")).toBeTruthy();
    expect(within(row("updates.auto_download")).getByText("Applies on restart")).toBeTruthy();
    expect(screen.queryByRole("switch", { name: /crash/i })).toBeNull();
    expect(screen.getByText(/no telemetry, analytics or crash reporting/)).toBeTruthy();
  });

  it("validates inline, never saves invalid values, and auto-saves valid ones", async () => {
    const api = saving();
    setup(["settings_load", "settings_save"], { settings: api });
    const tick = await screen.findByRole("spinbutton", { name: "Update interval" });
    fireEvent.change(tick, { target: { value: "50" } });
    expect(tick.getAttribute("aria-invalid")).toBe("true");
    expect(within(row("live.update_tick_ms")).getByRole("alert").textContent).toBe("Enter a value from 100 to 60,000 ms.");
    expect(screen.getByText(/1 setting needs attention — not saved/)).toBeTruthy();
    await act(async () => {
      await new Promise((r) => setTimeout(r, 700));
    });
    expect(api.saveSettings).not.toHaveBeenCalled();

    fireEvent.change(tick, { target: { value: "500" } });
    fireEvent.change(screen.getByRole("combobox", { name: "Theme" }), { target: { value: "dark" } });
    fireEvent.change(screen.getByRole("spinbutton", { name: "Extra confirmation above" }), { target: { value: "2" } });
    await waitFor(() => {
      expect(api.saveSettings).toHaveBeenCalled();
    }, SAVE_WAIT);
    const sent = api.saveSettings.mock.calls.at(-1)?.[0];
    expect(sent?.live.update_tick_ms).toBe(500);
    expect(sent?.appearance.theme).toBe("dark");
    expect(sent?.cleanup.large_delete_confirm_bytes).toBe(2 * GIB);
    expect(await screen.findByText("All changes saved")).toBeTruthy();
    expect(useSettings.getState().theme).toBe("dark");
  });

  it("converts byte thresholds with the unit select", async () => {
    const api = saving();
    setup(["settings_load", "settings_save"], { settings: api });
    const unit = await screen.findByRole("combobox", { name: "Minimum file size unit" });
    expect((unit as HTMLSelectElement).value).toBe("MB");
    fireEvent.change(screen.getByRole("spinbutton", { name: "Minimum file size" }), { target: { value: "512" } });
    fireEvent.change(unit, { target: { value: "KB" } });
    await waitFor(() => {
      expect(api.saveSettings.mock.calls.at(-1)?.[0].cleanup.duplicates_min_bytes).toBe(512 * 1024);
    }, SAVE_WAIT);
  });

  it("marks modified settings and resets one to its default", async () => {
    const changed = defaultSettings();
    changed.live.update_tick_ms = 250;
    const api = saving(changed);
    setup(["settings_load", "settings_save"], { settings: api });
    await screen.findByRole("spinbutton", { name: "Update interval" });
    const tickRow = row("live.update_tick_ms");
    expect(tickRow.className).toContain("is-modified");
    expect(within(tickRow).getByText("(modified)")).toBeTruthy();
    expect(row("live.usn_enabled").className).not.toContain("is-modified");
    fireEvent.click(within(tickRow).getByRole("button", { name: "Reset Update interval to default" }));
    expect(screen.getByRole<HTMLInputElement>("spinbutton", { name: "Update interval" }).value).toBe("1000");
    expect(row("live.update_tick_ms").className).not.toContain("is-modified");
    await waitFor(() => {
      expect(api.saveSettings.mock.calls.at(-1)?.[0].live.update_tick_ms).toBe(1000);
    }, SAVE_WAIT);
  });

  it("shows the backend's issues when it refuses a save", async () => {
    const saveSettings = vi.fn((s: Settings) => Promise.resolve({ settings: s, issues: [{ key: "history.retention_days", message: "too short for weekly thinning" }] }));
    setup(["settings_load", "settings_save"], { settings: { loadSettings: resolves(defaultSettings()), saveSettings } });
    fireEvent.change(await screen.findByRole("spinbutton", { name: "Keep snapshots for" }), { target: { value: "40" } });
    expect(await screen.findByText("too short for weekly thinning", undefined, SAVE_WAIT)).toBeTruthy();
    expect(screen.getByText(/1 setting needs attention — not saved/)).toBeTruthy();
    expect(within(row("history.retention_days")).getByRole("alert").textContent).toBe("too short for weekly thinning");
  });

  it("searches titles, descriptions and keys, highlights matches and filters @modified", async () => {
    const changed = defaultSettings();
    changed.tray.enabled = true;
    setup(["settings_load", "settings_save"], { settings: saving(changed) });
    const search = await screen.findByRole("searchbox", { name: "Search settings" });
    fireEvent.change(search, { target: { value: "interval" } });
    expect(screen.getByText("2 settings found")).toBeTruthy();
    expect(row("live.update_tick_ms")).toBeTruthy();
    expect(row("history.snapshot_interval_hours")).toBeTruthy();
    expect(row("scan.auto_scan_on_launch")).toBeNull();
    expect([...document.querySelectorAll("mark.hl")].some((m) => m.textContent.toLowerCase() === "interval")).toBe(true);

    fireEvent.change(search, { target: { value: "update_tick" } });
    expect(screen.getByText("1 setting found")).toBeTruthy();

    fireEvent.change(search, { target: { value: "" } });
    fireEvent.click(screen.getByRole("button", { name: "Modified" }));
    expect((search as HTMLInputElement).value).toBe("@modified");
    expect(screen.getByText("1 setting found")).toBeTruthy();
    expect(row("tray.enabled")).toBeTruthy();

    fireEvent.change(search, { target: { value: "qqqq" } });
    expect(screen.getByText("No settings match “qqqq”.")).toBeTruthy();
  });

  it("shows the effective settings as read-only JSON with copy", async () => {
    const writeText = vi.fn(() => Promise.resolve());
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });
    setup(["settings_load", "settings_save"], { settings: saving() });
    fireEvent.click(await screen.findByRole("radio", { name: "JSON" }));
    const code = screen.getByLabelText("Effective settings as JSON");
    expect(code.textContent).toContain('"update_tick_ms": 1000');
    expect(code.textContent).not.toContain("crash_reports_opt_in");
    fireEvent.click(screen.getByRole("button", { name: "Copy" }));
    await waitFor(() => {
      expect(writeText).toHaveBeenCalled();
    });
    const copied = JSON.parse((writeText.mock.calls[0] as unknown as [string])[0]) as { live: { update_tick_ms: number } };
    expect(copied.live.update_tick_ms).toBe(1000);
    expect(await screen.findByText("Copied to the clipboard.")).toBeTruthy();
    const exp = screen.getByRole("button", { name: "Export…" });
    expect(exp.getAttribute("aria-disabled")).toBe("true");
  });

  it("explains a path with the matched rule and precedence", async () => {
    const explainPath = resolves({
      path: "C:\\Users\\me\\AppData\\Local\\Temp",
      result: { category: 7, safety: "safe" as const, ruleId: "windows.temp", regenerable: true },
      rule: { id: "windows.temp", name: "Windows temporary files", pack: "windows", source: "builtin" as const, category: 7, safety: "safe" as const, explain: "Recreated as needed.", action: "delete" as const, app: null, regenerable: true, overriddenBy: null },
      originPath: null,
      steps: [],
      trace: ["windows.temp matched {TEMP}", "user-data.default: fallback"],
    });
    setup(["settings_load", "settings_save", "rules_explain"], { settings: { ...saving(), explainPath } });
    const input = await screen.findByRole("textbox", { name: /Test a path/ });
    fireEvent.change(input, { target: { value: "C:\\Users\\me\\AppData\\Local\\Temp" } });
    fireEvent.click(screen.getByRole("button", { name: "Explain" }));
    expect(await screen.findByText("windows.temp matched {TEMP}")).toBeTruthy();
    expect(screen.getByText("Windows temporary files")).toBeTruthy();
    expect(screen.getByText("Recreated as needed.")).toBeTruthy();
    expect(screen.getByRole("heading", { name: "Precedence" })).toBeTruthy();
    expect(explainPath).toHaveBeenCalledWith("C:\\Users\\me\\AppData\\Local\\Temp");
  });

  it("lists built-in rule packs read-only with counts", async () => {
    const rule = (id: string, pack: string) => ({ id, name: id, pack, source: "builtin" as const, category: 6, safety: "safe" as const, explain: "", action: "delete" as const, app: null, regenerable: true, overriddenBy: null });
    setup(["settings_load", "settings_save", "rules_list"], { settings: { ...saving(), fetchRules: resolves([rule("a", "windows"), rule("b", "windows"), rule("c", "browsers")]) } });
    const table = await screen.findByRole("region", { name: "Rule pack list" });
    const rows = within(table).getAllByRole("row").slice(1).map((r) => r.textContent);
    expect(rows).toEqual(["browsersBuilt-in, read-only1—", "windowsBuilt-in, read-only2—"]);
    expect(screen.getByText(/3 built-in rules in 2 packs/)).toBeTruthy();
  });

  it("disables actions the engine does not provide, with the reason", async () => {
    setup(["settings_load", "settings_save"], { settings: saving() });
    const report = await screen.findByRole("button", { name: "Report an issue…" });
    expect(report.getAttribute("aria-disabled")).toBe("true");
    expect(report.getAttribute("data-tip")).toBe("Not available in this build: the engine doesn’t provide report_issue yet.");
    const clear = screen.getByRole("button", { name: "Clear history…" });
    expect(clear.getAttribute("aria-disabled")).toBe("true");
    fireEvent.click(clear);
    expect(screen.queryByRole("dialog")).toBeNull();
    for (const b of screen.getAllByRole("button", { name: "Check for updates" })) expect(b.getAttribute("aria-disabled")).toBe("true");
  });

  it("clears data after confirmation when available", async () => {
    const clearData = vi.fn(() => Promise.resolve(null));
    setup(["settings_load", "settings_save", "data_clear"], { settings: { ...saving(), clearData } });
    fireEvent.click(await screen.findByRole("button", { name: "Clear activity…" }));
    const dialog = screen.getByRole("dialog");
    expect(clearData).not.toHaveBeenCalled();
    fireEvent.click(within(dialog).getByRole("button", { name: "Clear activity" }));
    await waitFor(() => {
      expect(clearData).toHaveBeenCalledWith("activity");
    });
    expect(await screen.findByText("Clear activity: done.")).toBeTruthy();
  });

  it("opens at a deep-linked category", async () => {
    useLayout.setState({ settingsFocus: "activity" });
    setup(["settings_load", "settings_save"], { settings: saving() });
    await screen.findByRole("heading", { name: "Activity tracking", level: 2 });
    await waitFor(() => {
      expect(screen.getByRole("button", { name: "Activity tracking" }).getAttribute("aria-current")).toBe("true");
    });
    expect(useLayout.getState().settingsFocus).toBeNull();
  });
});
