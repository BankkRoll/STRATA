import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { App } from "../App";
import type { VolumeInfo } from "../lib/volumes";
import type { Services } from "../services";
import { useApp } from "../store/app";
import { resetStores, testServices } from "../test/services";
import { SHORTCUTS, commandFor, keycaps, matchesCombo, parseCombo } from "./keymap";
import { DEFAULT_LAYOUT, LAYOUT_STORAGE_KEY, readLayout, sanitizeLayout, useLayout } from "./layout";
import { normalizePath, resolvePath, splitPath, suggestPaths } from "./pathResolve";
import { cycleRegion } from "./regions";
import { useTabs } from "./tabs";

const key = (k: string, mods: Partial<Record<"ctrlKey" | "altKey" | "shiftKey", boolean>> = {}, code = "") => ({
  key: k,
  code,
  ctrlKey: false,
  altKey: false,
  shiftKey: false,
  metaKey: false,
  ...mods,
});

const flush = () =>
  act(async () => {
    await new Promise((r) => setTimeout(r, 0));
  });

beforeEach(() => {
  resetStores();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

async function fixtureVolume(services: Services): Promise<VolumeInfo> {
  const [v] = await services.volumes.list();
  return v as VolumeInfo;
}

// -----------------------------------------------------------------------------
// Keymap
// -----------------------------------------------------------------------------

describe("keymap", () => {
  it("parses combinations, including keys that are themselves symbols", () => {
    expect(parseCombo("Ctrl+Shift+PageUp")).toEqual({ ctrl: true, alt: false, shift: true, key: "pageup" });
    expect(parseCombo("Ctrl+,")).toEqual({ ctrl: true, alt: false, shift: false, key: "," });
    expect(parseCombo("+")).toEqual({ ctrl: false, alt: false, shift: false, key: "+" });
    expect(parseCombo("Esc").key).toBe("escape");
    expect(keycaps("Alt+ArrowUp")).toEqual(["Alt", "↑"]);
  });

  it("matches events, treating shifted symbols and physical digits sensibly", () => {
    expect(matchesCombo(key("?", { shiftKey: true }), parseCombo("?"))).toBe(true);
    expect(matchesCombo(key("K", { ctrlKey: true, shiftKey: true }), parseCombo("Ctrl+K"))).toBe(false);
    expect(matchesCombo(key("&", { altKey: true }, "Digit1"), parseCombo("Alt+1"))).toBe(true);
  });

  it("maps keys to shell commands and respects text fields", () => {
    expect(commandFor(key("k", { ctrlKey: true }), false)).toBe("palette");
    expect(commandFor(key("p", { ctrlKey: true }), true)).toBe("palette");
    expect(commandFor(key(",", { ctrlKey: true }), false)).toBe("settings");
    expect(commandFor(key("?", { shiftKey: true }), false)).toBe("cheatSheet");
    expect(commandFor(key("?", { shiftKey: true }), true)).toBeNull();
    expect(commandFor(key("b", { ctrlKey: true }), false)).toBe("toggleSidebar");
    expect(commandFor(key("b", { ctrlKey: true, altKey: true }), false)).toBe("toggleInspector");
    expect(commandFor(key("2", { ctrlKey: true }, "Digit2"), false)).toBe("view:sunburst");
    expect(commandFor(key("2", { altKey: true }, "Digit2"), false)).toBe("area:insights");
    expect(commandFor(key("PageUp", { ctrlKey: true, shiftKey: true }), false)).toBe("moveTabLeft");
    expect(commandFor(key("F6"), true)).toBe("nextRegion");
    expect(commandFor(key("x"), false)).toBeNull();
  });

  it("never binds one combination to two commands", () => {
    const seen = new Map<string, string>();
    for (const s of SHORTCUTS) {
      if (!s.command) continue;
      for (const k of s.keys) {
        const c = JSON.stringify(parseCombo(k));
        expect(seen.get(c) ?? s.command, k).toBe(s.command);
        seen.set(c, s.command);
      }
    }
  });
});

// -----------------------------------------------------------------------------
// Layout persistence
// -----------------------------------------------------------------------------

describe("layout", () => {
  it("clamps and repairs persisted values field by field", () => {
    const l = sanitizeLayout({ sidebarWidth: 5000, inspectorWidth: "wide", split: "sideways", sidebarOpen: false, collapsed: { a: true, b: 3 }, savedFilters: [{ id: "x" }] });
    expect(l.sidebarWidth).toBe(420);
    expect(l.inspectorWidth).toBe(DEFAULT_LAYOUT.inspectorWidth);
    expect(l.split).toBe("split");
    expect(l.sidebarOpen).toBe(false);
    expect(l.collapsed).toEqual({ a: true });
    expect(l.savedFilters).toEqual([]);
    window.localStorage.setItem(LAYOUT_STORAGE_KEY, "{not json");
    expect(readLayout()).toEqual({ ...DEFAULT_LAYOUT });
  });

  it("remembers the sidebar and pane state across launches", async () => {
    useApp.getState().openVolume("fixture", 0);
    render(<App services={testServices()} />);
    expect(screen.getByRole("navigation", { name: "Explore sidebar" })).toBeTruthy();
    fireEvent.keyDown(window, key("b", { ctrlKey: true }));
    expect(screen.queryByRole("navigation", { name: "Explore sidebar" })).toBeNull();
    fireEvent.keyDown(window, key("j", { ctrlKey: true }));
    expect(useApp.getState().panes.list).toBe(false);
    await waitFor(() => {
      const saved = JSON.parse(window.localStorage.getItem(LAYOUT_STORAGE_KEY) ?? "{}") as { sidebarOpen?: boolean; listOpen?: boolean };
      expect(saved).toMatchObject({ sidebarOpen: false, listOpen: false });
    });
  });
});

// -----------------------------------------------------------------------------
// Shell keyboard and regions
// -----------------------------------------------------------------------------

describe("shell keyboard", () => {
  it("opens the shortcut sheet with ? and closes it with Escape", async () => {
    render(<App services={testServices()} />);
    fireEvent.keyDown(window, key("?", { shiftKey: true }));
    const sheet = await screen.findByRole("dialog", { name: "Keyboard shortcuts" });
    expect(within(sheet).getByText("Show or hide the sidebar")).toBeTruthy();
    fireEvent.change(within(sheet).getByRole("searchbox", { name: "Filter shortcuts" }), { target: { value: "tab" } });
    expect(within(sheet).queryByText("Show or hide the sidebar")).toBeNull();
    expect(within(sheet).getByText("Close the tab")).toBeTruthy();
    fireEvent.keyDown(within(sheet).getByRole("searchbox"), { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("cycles focus through the regions with F6 and Shift+F6", () => {
    useApp.getState().openVolume("fixture", 0);
    render(<App services={testServices()} />);
    const regions = [...document.querySelectorAll<HTMLElement>("[data-region]")].map((r) => r.dataset.region);
    expect(regions).toEqual(["titlebar", "activitybar", "sidebar", "workspace", "inspector", "statusbar"]);
    expect(cycleRegion(1)).toBe("titlebar");
    expect(document.activeElement?.textContent).toMatch(/Search files or run a command/);
    fireEvent.keyDown(window, key("F6"));
    expect(document.activeElement?.closest("[data-region]")?.getAttribute("data-region")).toBe("activitybar");
    expect(document.activeElement?.getAttribute("aria-current")).toBe("page");
    fireEvent.keyDown(window, key("F6", { shiftKey: true }));
    expect(document.activeElement?.closest("[data-region]")?.getAttribute("data-region")).toBe("titlebar");
  });

  it("switches areas from the keyboard and remembers each area's view", async () => {
    render(<App services={testServices()} />);
    fireEvent.keyDown(window, key("2", { altKey: true }, "Digit2"));
    expect(useApp.getState().view).toBe("recommendations");
    fireEvent.click(screen.getByRole("button", { name: "Duplicates" }));
    fireEvent.keyDown(window, key("1", { altKey: true }, "Digit1"));
    expect(useApp.getState().view).toBe("home");
    fireEvent.keyDown(window, key("2", { altKey: true }, "Digit2"));
    expect(useApp.getState().view).toBe("duplicates");
    fireEvent.keyDown(window, key(",", { ctrlKey: true }));
    expect(useApp.getState().view).toBe("settings");
    await flush();
  });

  it("toggles the sidebar when the open area's icon is clicked again", () => {
    render(<App services={testServices()} />);
    const areas = screen.getByRole("navigation", { name: "Areas" });
    const explore = within(areas).getByRole("button", { name: "Explore" });
    expect(explore.getAttribute("aria-expanded")).toBe("true");
    fireEvent.click(explore);
    expect(useLayout.getState().sidebarOpen).toBe(false);
    expect(explore.getAttribute("aria-expanded")).toBe("false");
  });
});

// -----------------------------------------------------------------------------
// Tabs
// -----------------------------------------------------------------------------

describe("tabs", () => {
  it("opens a tab per location and supports keyboard open, cycle, move and close", async () => {
    const services = testServices();
    useApp.getState().openVolume("fixture", 0);
    render(<App services={services} />);
    const tablist = screen.getByRole("tablist", { name: "Open locations" });
    await waitFor(() => {
      expect(within(tablist).getAllByRole("tab").map((t) => t.textContent)).toEqual(["Volumes", "Synthetic fixture"]);
    });
    act(() => {
      useApp.getState().drillTo([1]);
    });
    fireEvent.keyDown(window, key("t", { ctrlKey: true }));
    expect(useTabs.getState().tabs).toHaveLength(2);
    expect(useTabs.getState().tabs[1]?.path).toEqual([0, 1]);
    act(() => {
      useApp.getState().drillTo([7]);
    });
    expect(useTabs.getState().tabs[1]?.path).toEqual([0, 1, 7]);
    expect(useTabs.getState().tabs[0]?.path).toEqual([0, 1]);

    fireEvent.keyDown(window, key("Tab", { ctrlKey: true }));
    expect(useApp.getState().path).toEqual([0, 1]);
    fireEvent.keyDown(window, key("PageDown", { ctrlKey: true, shiftKey: true }));
    expect(useTabs.getState().tabs.map((t) => t.path.length)).toEqual([3, 2]);

    const selected = within(tablist).getAllByRole("tab").find((t) => t.getAttribute("aria-selected") === "true") as HTMLElement;
    fireEvent.keyDown(selected, { key: "ArrowLeft" });
    expect(useApp.getState().path).toEqual([0, 1, 7]);

    fireEvent.keyDown(window, key("w", { ctrlKey: true }));
    expect(useTabs.getState().tabs).toHaveLength(1);
    expect(useApp.getState().path).toEqual([0, 1]);
    fireEvent.keyDown(window, key("w", { ctrlKey: true }));
    expect(useTabs.getState().tabs).toHaveLength(0);
    expect(useApp.getState()).toMatchObject({ view: "home", volumeId: null });
    await flush();
  });

  it("returns to the volumes page from its tab without closing others", async () => {
    useApp.getState().openVolume("fixture", 0);
    render(<App services={testServices()} />);
    fireEvent.click(screen.getByRole("tab", { name: "Volumes" }));
    expect(useApp.getState().view).toBe("home");
    expect(useTabs.getState().tabs).toHaveLength(1);
    fireEvent.click(await screen.findByRole("tab", { name: "Synthetic fixture" }));
    expect(useApp.getState().view).toBe("treemap");
  });
});

// -----------------------------------------------------------------------------
// Path bar
// -----------------------------------------------------------------------------

describe("path bar", () => {
  it("normalizes and splits typed paths", async () => {
    const services = testServices();
    const v = await fixtureVolume(services);
    const c = { volumes: [{ ...v, mountPoints: ["D:\\"] }], current: { volumeId: "fixture", path: [0, 1] } };
    expect(normalizePath(' "d:/Media//x/" ')).toBe("d:\\Media\\x");
    expect(splitPath("D:\\folder #21\\x", c)).toMatchObject({ base: [0], segments: ["folder #21", "x"] });
    expect(splitPath("..\\y", c)).toMatchObject({ base: [0, 1], segments: ["..", "y"] });
    expect(splitPath("Q:\\nowhere", c)).toBeNull();
  });

  it("resolves folders through the index and explains failures", async () => {
    const services = testServices();
    const v = await fixtureVolume(services);
    const ctx = { volumes: [v], rows: services.rows, current: null };
    const page = await services.rows.fetchChildren({ volumeId: "fixture", parent: 0, sort: { key: "size", desc: true }, sizeMode: "allocated", offset: 0, limit: 50, filters: useApp.getState().filters });
    const dir = page.rows.find((r) => r.isDir);
    const file = page.rows.find((r) => !r.isDir);
    if (!dir || !file) throw new Error("fixture needs a folder and a file at the root");
    expect(await resolvePath(`Synthetic fixture\\${dir.name.toUpperCase()}`, ctx)).toEqual({ volumeId: "fixture", path: [0, dir.id] });
    expect(await resolvePath(`Synthetic fixture\\${file.name}`, ctx)).toEqual({ error: `“${file.name}” is a file, not a folder.` });
    expect(await resolvePath("Synthetic fixture\\missing", ctx)).toEqual({ error: "No folder named “missing” in “Synthetic fixture”." });
    const suggestions = await suggestPaths(`Synthetic fixture\\${dir.name.slice(0, 4)}`, ctx);
    expect(suggestions.length).toBeGreaterThan(0);
    expect(suggestions.every((s) => s.startsWith("Synthetic fixture\\fold"))).toBe(true);
  });

  it("edits the path with Ctrl+L and navigates on Enter", async () => {
    const services = testServices();
    useApp.getState().openVolume("fixture", 0);
    render(<App services={services} />);
    const page = await services.rows.fetchChildren({ volumeId: "fixture", parent: 0, sort: { key: "size", desc: true }, sizeMode: "allocated", offset: 0, limit: 50, filters: useApp.getState().filters });
    const dir = page.rows.find((r) => r.isDir);
    if (!dir) throw new Error("fixture needs a folder");
    fireEvent.keyDown(window, key("l", { ctrlKey: true }));
    const input = await screen.findByRole("combobox", { name: "Path" });
    expect((input as HTMLInputElement).value).toBe("Synthetic fixture");
    fireEvent.change(input, { target: { value: `Synthetic fixture\\${dir.name}` } });
    fireEvent.keyDown(input, { key: "Enter" });
    await waitFor(() => {
      expect(useApp.getState().path).toEqual([0, dir.id]);
    });
    expect(screen.queryByRole("combobox", { name: "Path" })).toBeNull();

    fireEvent.keyDown(window, key("l", { ctrlKey: true }));
    const again = await screen.findByRole("combobox", { name: "Path" });
    fireEvent.change(again, { target: { value: "Synthetic fixture\\nope" } });
    fireEvent.keyDown(again, { key: "Enter" });
    expect((await screen.findByRole("alert")).textContent).toBe("No folder named “nope” in “Synthetic fixture”.");
    fireEvent.keyDown(again, { key: "Escape" });
    expect(screen.queryByRole("combobox", { name: "Path" })).toBeNull();
  });
});

// -----------------------------------------------------------------------------
// Status bar, states and responsive layout
// -----------------------------------------------------------------------------

describe("status bar and states", () => {
  it("summarizes the selection and the volume's free space", async () => {
    useApp.getState().openVolume("fixture", 0);
    render(<App services={testServices()} />);
    const status = screen.getByRole("contentinfo", { name: "Status" });
    expect(await within(status).findByText(/free$/)).toBeTruthy();
    expect(within(status).getByText("Standard scan")).toBeTruthy();
    expect(within(status).getByText("Live")).toBeTruthy();
    act(() => {
      useApp.getState().select([1, 2], 2);
    });
    expect(await within(status).findByText(/^2 selected · \d/)).toBeTruthy();
  });

  it("keeps a designed state when fast scan is declined", async () => {
    const services = testServices();
    services.volumes = { ...services.volumes, elevate: () => Promise.reject(new Error("The operation was canceled by the user.")) };
    render(<App services={services} />);
    const status = screen.getByRole("contentinfo", { name: "Status" });
    fireEvent.click(await within(status).findByRole("button", { name: "Standard scan" }));
    expect(await within(status).findByRole("button", { name: "Standard scan · fast scan declined" })).toBeTruthy();
    expect(within(status).getByRole("status").textContent).toBe("Fast scan not enabled: The operation was canceled by the user.");
  });

  it("shows scan progress with an ETA and a workspace notice while scanning", async () => {
    const services = testServices();
    const v = await fixtureVolume(services);
    const scanning: VolumeInfo = { ...v, scan: { ...v.scan, state: "scanning", progress: { entries: 12_000, bytes: 1e9, fraction: 0.25, etaSecs: 150 } } };
    services.volumes = { ...services.volumes, list: () => Promise.resolve([scanning]) };
    useApp.getState().openVolume("fixture", 0);
    render(<App services={services} />);
    const bar = await screen.findByRole("progressbar", { name: "Scanning Synthetic fixture" });
    expect(bar.getAttribute("aria-valuenow")).toBe("25");
    expect(bar.parentElement?.textContent).toMatch(/25% · 12,000 items · about 3 min left/);
    expect(screen.getByText(/Sizes grow as 12,000 items so far are counted/)).toBeTruthy();
  });

  it("collapses the sidebar to the rail and overlays the inspector in narrow windows", async () => {
    vi.stubGlobal("matchMedia", (q: string) => ({ matches: q.includes("max-width"), addEventListener: () => undefined, removeEventListener: () => undefined }));
    useApp.getState().openVolume("fixture", 0);
    render(<App services={testServices()} />);
    expect(screen.queryByRole("navigation", { name: "Explore sidebar" })).toBeNull();
    expect(screen.queryByRole("complementary", { name: "Details" })).toBeNull();
    fireEvent.keyDown(window, key("b", { ctrlKey: true }));
    const overlay = screen.getByRole("navigation", { name: "Explore sidebar" });
    expect(overlay.className).toContain("sidebar--overlay");
    fireEvent.keyDown(within(overlay).getAllByRole("button")[0] as HTMLElement, { key: "Escape" });
    expect(screen.queryByRole("navigation", { name: "Explore sidebar" })).toBeNull();
    act(() => {
      useApp.getState().select([1], 1);
    });
    const details = await screen.findByRole("complementary", { name: "Details" });
    expect(details.closest(".inspector--overlay")).not.toBeNull();
    fireEvent.click(within(details).getByRole("button", { name: "Close details" }));
    expect(useApp.getState().panes.detail).toBe(false);
  });

  it("shows the first-run state when nothing is scanned yet", async () => {
    const services = testServices();
    const v = await fixtureVolume(services);
    const fresh: VolumeInfo = { ...v, mountPoints: ["X:\\"], categoryBytes: null, scan: { state: "never", progress: null, lastScanMs: null, scanner: null, rootId: null } };
    services.volumes = { ...services.volumes, list: () => Promise.resolve([fresh]) };
    render(<App services={services} />);
    const sidebar = screen.getByRole("navigation", { name: "Explore sidebar" });
    expect(await within(sidebar).findByRole("button", { name: /Synthetic fixture \(X:\), Not scanned, .* Press to scan/ })).toBeTruthy();
    expect(screen.getByRole("contentinfo", { name: "Status" }).textContent).toMatch(/No volume open/);
  });

  it("hides unscanned partitions without a drive letter until asked", async () => {
    const services = testServices();
    const v = await fixtureVolume(services);
    const never = { state: "never", progress: null, lastScanMs: null, scanner: null, rootId: null } as const;
    const drive: VolumeInfo = { ...v, id: "drive", mountPoints: ["X:\\"], categoryBytes: null, scan: never };
    const efi: VolumeInfo = { ...v, id: "efi", label: "", mountPoints: [], categoryBytes: null, scan: never };
    services.volumes = { ...services.volumes, list: () => Promise.resolve([drive, efi]) };
    render(<App services={services} />);
    const sidebar = screen.getByRole("navigation", { name: "Explore sidebar" });
    await within(sidebar).findByRole("button", { name: /Synthetic fixture \(X:\)/ });
    expect(within(sidebar).queryByText("Unnamed partition")).toBeNull();
    expect(screen.queryByText(/efi/)).toBeNull();
    fireEvent.click(await screen.findByRole("button", { name: "Show 1 system partition without a drive letter" }));
    expect(await screen.findByRole("heading", { name: "Unnamed partition" })).toBeTruthy();
  });
});
