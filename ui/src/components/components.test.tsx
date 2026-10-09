import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { BackendUnavailableError } from "../lib/backend";
import { CommandBus } from "../lib/commands";
import type { VolumeInfo } from "../lib/volumes";
import { ServicesContext, type Services } from "../services";
import { useApp } from "../store/app";
import { hoverStore } from "../store/hover";
import { useVolumes } from "../store/volumes";
import { FakeRenderer, resetStores, testServices } from "../test/services";
import { CapacityBar, Home } from "../views/Home";
import { DetailPanel } from "../views/DetailPanel";
import { ListPane } from "../views/ListPane";
import { ContextMenu } from "./ContextMenu";
import { rankCommands } from "./CommandPalette";
import { buildCommands } from "./paletteCommands";

function withServices(services: Services, ui: React.ReactNode) {
  return render(<ServicesContext value={services}>{ui}</ServicesContext>);
}

const flush = () => act(async () => {
  await new Promise((r) => setTimeout(r, 0));
});

beforeEach(() => {
  resetStores();
  hoverStore.setState({ target: null });
});

describe("Home", () => {
  it("shows the designed first-run state when volume discovery is not in the build", async () => {
    const services = testServices();
    services.volumes = { ...services.volumes, list: () => Promise.reject(new BackendUnavailableError("list_volumes", "nope")) };
    render(<App services={services} />);
    expect(await screen.findByText("See everything on your drives")).toBeTruthy();
    expect(screen.getByText("Drive scanning is not connected in this build yet.")).toBeTruthy();
  });

  it("lists volumes with capacity, badges, state chip and the elevation banner", async () => {
    const services = testServices();
    render(<App services={services} />);
    const card = await screen.findByRole("heading", { name: "Synthetic fixture", level: 2 });
    const li = card.closest("li") as HTMLElement;
    expect(within(li).getByText("NTFS")).toBeTruthy();
    expect(within(li).getByText("Live")).toBeTruthy();
    expect(within(li).getByRole("img").getAttribute("aria-label")).toMatch(/used of .* free/);
    expect(screen.getByText(/Standard scan — some system folders hidden/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Enable fast scan" })).toBeTruthy();
    expect(await screen.findByText(/since/)).toBeTruthy();
    fireEvent.click(within(li).getByRole("button", { name: "Open map" }));
    expect(useApp.getState()).toMatchObject({ volumeId: "fixture", path: [0], view: "treemap" });
  });

  it("explains unaccounted space in the capacity bar", () => {
    const v = { totalBytes: 1000, freeBytes: 200, categoryBytes: { "6": 500 } } as unknown as VolumeInfo;
    render(<CapacityBar v={v} />);
    expect(screen.getByRole("img").getAttribute("aria-label")).toMatch(/Caches .*Unaccounted \/ system reserved/);
  });

  it("disables scanning BitLocker-locked volumes", async () => {
    const services = testServices();
    const vols = await services.volumes.list();
    const locked = { ...vols[0], bitlocker: "locked", scan: { ...vols[0]?.scan, state: "never", rootId: null } } as VolumeInfo;
    services.volumes = { ...services.volumes, list: () => Promise.resolve([locked]) };
    withServices(services, <Home />);
    useVolumes.getState().setVolumes([locked]);
    const btn = await screen.findByRole("button", { name: "Scan" });
    expect((btn as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText("BitLocker locked")).toBeTruthy();
  });
});

describe("App shell", () => {
  it("renders landmarks and keeps context when switching views", async () => {
    const services = testServices();
    useApp.getState().openVolume("fixture", 0);
    render(<App services={services} />);
    expect(screen.getByRole("banner")).toBeTruthy();
    expect(screen.getByRole("navigation", { name: "Views" })).toBeTruthy();
    expect(screen.getByRole("navigation", { name: "Breadcrumb" })).toBeTruthy();
    expect(screen.getByRole("complementary", { name: "Details" })).toBeTruthy();
    expect(screen.getByRole("application")).toBeTruthy();
    act(() => {
      useApp.getState().select([5], 5);
    });
    fireEvent.click(screen.getByRole("button", { name: /Sunburst/ }));
    await waitFor(() => {
      expect(useApp.getState().view).toBe("sunburst");
    });
    expect(useApp.getState().selection).toEqual([5]);
    expect(screen.getByRole("button", { name: /Sunburst/ }).getAttribute("aria-current")).toBe("page");
  });

  it("switches size mode with an accessible radio group", () => {
    render(<App services={testServices()} />);
    const logical = screen.getByRole("radio", { name: "Logical" });
    fireEvent.click(logical);
    expect(useApp.getState().sizeMode).toBe("logical");
    fireEvent.keyDown(logical, { key: "ArrowLeft" });
    expect(useApp.getState().sizeMode).toBe("allocated");
  });

  it("opens the palette with Ctrl+K, filters commands and runs one", async () => {
    render(<App services={testServices()} />);
    fireEvent.keyDown(window, { key: "k", ctrlKey: true });
    const input = await screen.findByRole("combobox");
    fireEvent.change(input, { target: { value: "color age" } });
    const opt = await screen.findByRole("option", { name: /Color by age/ });
    expect(opt.getAttribute("aria-selected")).toBe("true");
    fireEvent.keyDown(input, { key: "Enter" });
    expect(useApp.getState().colorMode).toBe("age");
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("streams file search results into the palette", async () => {
    render(<App services={testServices()} />);
    fireEvent.keyDown(window, { key: "f", ctrlKey: true });
    const input = await screen.findByRole("combobox");
    fireEvent.change(input, { target: { value: "folder #1" } });
    await waitFor(() => {
      expect(screen.getAllByRole("option").some((o) => o.textContent.includes("folder #1"))).toBe(true);
    });
    expect(await screen.findByText(/files match/)).toBeTruthy();
  });
});

describe("palette ranking", () => {
  it("puts enabled exact matches first and keeps disabled commands visible", () => {
    const cmds = buildCommands(testServices(), []);
    const ranked = rankCommands(cmds, "settings");
    expect(ranked[0]?.cmd.id).toBe("settings.open");
    expect(ranked[0]?.cmd.availability.enabled).toBe(false);
    const views = rankCommands(cmds, "treemap");
    expect(views.slice(0, 2).map((v) => v.cmd.title)).toContain("Show Treemap");
  });
});

describe("ListPane", () => {
  function setup() {
    const services = testServices();
    useApp.getState().openVolume("fixture", 0);
    const menu = vi.fn();
    withServices(services, <ListPane volumeId="fixture" root={0} onContextMenu={menu} />);
    return { services, menu };
  }

  it("renders a sortable treegrid of the root's children", async () => {
    setup();
    const grid = await screen.findByRole("treegrid");
    await waitFor(() => {
      expect(within(grid).getAllByRole("row").length).toBeGreaterThan(5);
    });
    const header = within(grid).getByRole("columnheader", { name: /On disk/ });
    expect(header.getAttribute("aria-sort")).toBe("descending");
    const first = within(grid).getAllByRole("row")[1] as HTMLElement;
    expect(first.getAttribute("aria-level")).toBe("1");
    expect(first.getAttribute("aria-posinset")).toBe("1");
    fireEvent.click(within(grid).getByRole("button", { name: "Name" }));
    await waitFor(() => {
      expect(within(grid).getByRole("columnheader", { name: /Name/ }).getAttribute("aria-sort")).toBe("ascending");
    });
  });

  it("navigates with the keyboard, expands folders and type-ahead selects", async () => {
    setup();
    const grid = await screen.findByRole("treegrid");
    await waitFor(() => {
      expect(within(grid).getAllByRole("row").length).toBeGreaterThan(3);
    });
    fireEvent.keyDown(grid, { key: "ArrowDown" });
    const second = useApp.getState().primary;
    expect(second).not.toBeNull();
    fireEvent.keyDown(grid, { key: "Home" });
    const rows = () => within(grid).getAllByRole("row").slice(1);
    const firstDir = rows().findIndex((r) => r.getAttribute("aria-expanded") === "false");
    for (let i = 0; i < firstDir; i++) fireEvent.keyDown(grid, { key: "ArrowDown" });
    fireEvent.keyDown(grid, { key: "ArrowRight" });
    await waitFor(() => {
      expect(rows().some((r) => r.getAttribute("aria-level") === "2")).toBe(true);
    });
    fireEvent.keyDown(grid, { key: "ArrowLeft" });
    await waitFor(() => {
      expect(rows().some((r) => r.getAttribute("aria-level") === "2")).toBe(false);
    });
    fireEvent.keyDown(grid, { key: "f" });
    const picked = useApp.getState().primary;
    expect(rows().find((r) => r.getAttribute("aria-selected") === "true")?.textContent).toMatch(/^.*(file|folder) #/);
    expect(picked).not.toBeNull();
  });

  it("opens the context menu for the row and drills on Enter", async () => {
    const { menu } = setup();
    const grid = await screen.findByRole("treegrid");
    await waitFor(() => {
      expect(within(grid).getAllByRole("row").length).toBeGreaterThan(3);
    });
    const row = within(grid).getAllByRole("row").find((r) => r.getAttribute("aria-expanded") !== null) as HTMLElement;
    fireEvent.contextMenu(row, { clientX: 10, clientY: 20 });
    expect(menu).toHaveBeenCalledWith([useApp.getState().primary], 10, 20);
    fireEvent.click(row);
    const idx = within(grid).getAllByRole("row").indexOf(row) - 1;
    fireEvent.keyDown(grid, { key: "Home" });
    for (let i = 0; i < idx; i++) fireEvent.keyDown(grid, { key: "ArrowDown" });
    fireEvent.keyDown(grid, { key: "Enter" });
    expect(useApp.getState().path.length).toBe(2);
  });

  it("shows a designed error state when rows cannot load", async () => {
    const services = testServices({ rows: { fetchChildren: () => Promise.reject(new BackendUnavailableError("list_children", "Not in this build.")) } });
    withServices(services, <ListPane volumeId="fixture" root={0} onContextMenu={vi.fn()} />);
    expect(await screen.findByText("Can’t list this folder")).toBeTruthy();
    expect(screen.getByText("Not in this build.")).toBeTruthy();
  });
});

describe("DetailPanel", () => {
  it("has empty, loading, ready and error states", async () => {
    let fail = false;
    const base = testServices();
    const services = testServices({ fetchDetail: (v, id) => (fail ? Promise.reject(new Error("index offline")) : base.fetchDetail(v, id)) });
    useApp.getState().openVolume("fixture", 0);
    withServices(services, <DetailPanel />);
    expect(screen.getByText("Nothing selected")).toBeTruthy();
    act(() => {
      useApp.getState().select([1], 1);
    });
    expect(screen.getByRole("status", { name: "Loading details" })).toBeTruthy();
    expect(await screen.findByRole("heading", { name: /#1/, level: 2 })).toBeTruthy();
    for (const title of ["Size", "Dates", "Attributes", "Hardlinks", "Alternate data streams", "What it is", "Owner", "Safety", "Actions"]) {
      expect(screen.getByRole("heading", { name: title, level: 3 })).toBeTruthy();
    }
    expect(screen.getByText(/Last-access updates are off/)).toBeTruthy();
    const open = screen.getByRole("button", { name: "Open" });
    expect(open.getAttribute("aria-disabled")).toBe("true");
    expect(open.getAttribute("title")).toMatch(/not connected/);
    fail = true;
    act(() => {
      useApp.getState().select([2], 2);
    });
    expect(await screen.findByText("index offline")).toBeTruthy();
  });
});

describe("ContextMenu", () => {
  it("lists every action, disabling unavailable ones with a reason", () => {
    const notify = vi.fn();
    const exclude = vi.fn();
    const bus = new CommandBus({
      capabilities: () => new Set(),
      writeClipboard: () => Promise.resolve(),
      ui: { showInList: vi.fn(), explain: vi.fn(), exclude, notify },
    });
    const onClose = vi.fn();
    render(<ContextMenu menu={{ target: { volumeId: "v", ids: [3] }, x: 10, y: 10 }} bus={bus} onClose={onClose} />);
    const items = screen.getAllByRole("menuitem");
    expect(items.map((i) => i.querySelector(".menu__label")?.textContent)).toEqual([
      "Open",
      "Reveal in Explorer",
      "Open terminal here",
      "Copy path",
      "Properties",
      "Show in list",
      "Explain",
      "Exclude from view",
      "Add to cleanup",
    ]);
    const open = screen.getByRole("menuitem", { name: /^Open\b/ });
    expect(open.getAttribute("aria-disabled")).toBe("true");
    fireEvent.click(open);
    expect(onClose).not.toHaveBeenCalled();
    expect(document.activeElement).toBe(items[0]);
    fireEvent.keyDown(items[0] as HTMLElement, { key: "ArrowUp" });
    expect(document.activeElement).toBe(items[items.length - 1]);
    fireEvent.click(screen.getByRole("menuitem", { name: /Exclude from view/ }));
    expect(onClose).toHaveBeenCalled();
    expect(exclude).toHaveBeenCalledWith({ volumeId: "v", ids: [3] });
  });
});

describe("ViewController through VisualPane", () => {
  it("requests a layout, uploads the frame, picks on hover and drills on double-click", async () => {
    const renderers: FakeRenderer[] = [];
    const services = testServices({
      createRenderer: () => {
        const r = new FakeRenderer();
        renderers.push(r);
        return r;
      },
    });
    useApp.getState().openVolume("fixture", 0);
    render(<App services={services} />);
    await flush();
    await waitFor(() => {
      expect(renderers[0]?.frames.length).toBe(1);
    });
    const surface = screen.getByRole("application");
    // jsdom reports an 800×600 box for an 800×600 canvas: client px == frame px.
    const frame = renderers[0]?.frames[0];
    expect(frame?.nodes.count).toBeGreaterThan(100);
    fireEvent.pointerMove(surface, { clientX: 400, clientY: 300 });
    expect(hoverStore.getState().target).not.toBeNull();
    expect(await screen.findByRole("tooltip")).toBeTruthy();
    fireEvent.pointerDown(surface, { clientX: 400, clientY: 300, button: 0, pointerId: 1 });
    fireEvent.pointerUp(surface, { clientX: 400, clientY: 300, button: 0, pointerId: 1 });
    expect(useApp.getState().selection.length).toBe(1);
    await waitFor(() => {
      expect(renderers[0]?.selections.at(-1)?.size).toBe(1);
    });
    fireEvent.doubleClick(surface, { clientX: 400, clientY: 300 });
    const drilled = useApp.getState().path;
    expect(drilled.length).toBeGreaterThan(1);
    fireEvent.keyDown(surface, { key: "Backspace" });
    expect(useApp.getState().path).toEqual(drilled.slice(0, -1));
    act(() => {
      useApp.getState().jumpTo(0);
    });
    fireEvent.keyDown(surface, { key: "ArrowRight" });
    expect(screen.getByText(/level 1$/)).toBeTruthy();
    fireEvent.contextMenu(surface, { clientX: 400, clientY: 300 });
    expect(await screen.findByRole("menu")).toBeTruthy();
  });

  it("shows the no-WebGL state and keeps the list usable", async () => {
    const services = testServices({ createRenderer: () => null });
    useApp.getState().openVolume("fixture", 0);
    render(<App services={services} />);
    expect(await screen.findByText("Graphics acceleration is unavailable")).toBeTruthy();
    expect(await screen.findByRole("treegrid")).toBeTruthy();
  });
});
