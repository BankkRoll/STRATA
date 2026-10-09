import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import type { ReactNode } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { TimeChart, Sparkline } from "../components/charts";
import { FeaturesContext, type FeatureServices } from "../features";
import type { CleanupProgress, ExecutionReport, ItemVerdict } from "../lib/cleanup";
import type { EntryDetail } from "../lib/detail";
import type { SnapshotDiff, SnapshotInfo } from "../lib/history";
import { ServicesContext, type Services } from "../services";
import { useApp } from "../store/app";
import { useQueue } from "../store/queue";
import { useSettings } from "../store/settings";
import { dupeGroup, fakeFeatures, plan, queueEntry, resolves, type FeatureOverrides } from "../test/features";
import { resetStores, testServices } from "../test/services";
import { CleanupView } from "./CleanupView";
import { DetailPanel } from "./DetailPanel";
import { DuplicatesView } from "./DuplicatesView";
import { HistoryView } from "./HistoryView";
import { LargestView } from "./LargestView";
import { ToolsView } from "./ToolsView";

const GIB = 1024 ** 3;

function setup(caps: string[], overrides: FeatureOverrides, ui: ReactNode, services: Services = testServices()) {
  const s: Services = { ...services, capabilities: () => new Set(caps) };
  const f: FeatureServices = fakeFeatures(overrides);
  render(
    <ServicesContext value={s}>
      <FeaturesContext value={f}>{ui}</FeaturesContext>
    </ServicesContext>,
  );
  return { services: s, features: f };
}

const flush = () =>
  act(async () => {
    await new Promise((r) => setTimeout(r, 0));
  });

const FLOW = ["cleanup_queue_list", "cleanup_plan", "cleanup_preflight", "cleanup_execute", "cleanup_close_prompt", "cleanup_close_app", "cleanup_history", "cleanup_restore"];

beforeEach(() => {
  resetStores();
  useQueue.setState({ items: null, refused: [] });
  useSettings.setState({ theme: "system", units: "binary", patterns: false });
});

// -----------------------------------------------------------------------------
// Cleanup
// -----------------------------------------------------------------------------

describe("cleanup queue", () => {
  it("shows the designed unavailable state without a queue", () => {
    setup([], {}, <CleanupView />);
    expect(screen.getByText("The cleanup queue isn’t available in this build")).toBeTruthy();
    expect(screen.getByText("cleanup_queue_list")).toBeTruthy();
  });

  it("shows tier totals and why items were not added", () => {
    useQueue.setState({
      items: [queueEntry(1), queueEntry(2, { safety: "careful" })],
      refused: [{ entryId: 9, path: "C:\\Windows", reason: "never_tier", message: "Part of Windows; Strata never deletes it." }],
    });
    setup(FLOW, {}, <CleanupView />);
    const totals = screen.getAllByRole("definition");
    expect(totals[0]?.textContent).toMatch(/1\.00 GB · 1 item/);
    expect(totals[2]?.textContent).toMatch(/2\.00 GB · 1 item/);
    const refused = screen.getByRole("heading", { name: "Not added" }).closest("section") as HTMLElement;
    expect(within(refused).getByText(/never deletes it/)).toBeTruthy();
    expect(screen.getByRole("button", { name: /Review 2 items/ })).toBeTruthy();
  });

  it("requires Careful acknowledgements, refuses never-tier items, then pre-flights, closes politely, executes and retries", async () => {
    const items = [queueEntry(1), queueEntry(2, { safety: "careful", name: "models" }), queueEntry(3, { safety: "never", name: "System32" })];
    useQueue.setState({ items, refused: [] });
    const verdicts: ItemVerdict[] = [
      { id: 1, path: items[0]?.path ?? "", verdict: { status: "ready", recycle: { fit: "fits" } }, holders: [], runningApps: [] },
      {
        id: 2,
        path: items[1]?.path ?? "",
        verdict: { status: "blocked", error: { kind: "locked", message: "The item is in use.", retryable: true, path: null, holders: [] } },
        holders: [{ pid: 4242, startTime: 1, appName: "Example.exe", exePath: null, service: null, kind: "main_window", restartable: false }],
        runningApps: [],
      },
    ];
    const report: ExecutionReport = {
      actionId: 3,
      results: [
        { id: 1, path: items[0]?.path ?? "", outcome: { kind: "recycled", restoreItemId: 11 } },
        { id: 2, path: items[1]?.path ?? "", outcome: { kind: "failed", error: { kind: "locked", message: "Still in use.", retryable: true, path: null, holders: [] } } },
      ],
      summary: { succeeded: 1, failed: 1, skipped: 0, bytes: GIB, cancelled: false },
    };
    const planCleanup = resolves(plan(items));
    const preflightCleanup = vi.fn(() => Promise.resolve(verdicts));
    const prepareClose = resolves({ promptId: 5, message: "Close Example.exe? Unsaved work may be lost.", app: "Example.exe", pid: 4242, expiresMs: Date.now() + 60_000 });
    const closeApp = resolves({ kind: "shut_down" as const });
    const executeCleanup = vi.fn((_planId: number, _d: unknown, onProgress: (p: CleanupProgress) => void) => {
      onProgress({ event: "item_finished", id: 1, removed: true });
      return Promise.resolve(report);
    });
    const retryPlan = resolves(plan([items[1] as (typeof items)[number]], { planId: 8 }));
    setup(FLOW, { cleanup: { planCleanup, preflightCleanup, prepareClose, closeApp, executeCleanup, retryPlan } }, <CleanupView />);

    fireEvent.click(screen.getByRole("button", { name: /Review 3 items/ }));
    await screen.findByRole("heading", { name: "Review" });
    expect(planCleanup).toHaveBeenCalledWith([1, 2, 3]);
    expect(screen.getByText(/Never-tier items are protected/)).toBeTruthy();
    expect(screen.queryByRole("checkbox", { name: /Include System32/ })).toBeNull();

    const check = screen.getByRole<HTMLButtonElement>("button", { name: "Check items" });
    expect(check.disabled).toBe(true);
    expect(screen.getByText(/Confirm you reviewed “models”/)).toBeTruthy();
    fireEvent.click(screen.getByRole("checkbox", { name: /I reviewed this item/ }));
    expect(check.disabled).toBe(false);

    fireEvent.click(screen.getByRole("radio", { name: /Delete permanently \(can’t be undone\)/ }));
    expect(check.disabled).toBe(true);
    fireEvent.click(screen.getByRole("checkbox", { name: /permanently deleted items can’t be restored/ }));
    expect(check.disabled).toBe(false);
    fireEvent.click(screen.getByRole("radio", { name: /Move to Recycle Bin/ }));

    fireEvent.click(check);
    await screen.findByText(/1 ready/);
    const decision = preflightCleanup.mock.calls[0] as unknown as [number, { skip: number[]; acks: { careful: number[] } }];
    expect(decision[1].skip).toEqual([3]);
    expect(decision[1].acks.careful).toEqual([2]);
    expect(screen.getByText(/In use by:/).textContent).toMatch(/Example\.exe \(PID 4242\)/);
    const run = screen.getByRole<HTMLButtonElement>("button", { name: /Move to Recycle Bin \(1\)/ });
    expect(run.disabled).toBe(true);

    fireEvent.click(screen.getByRole("button", { name: "Close app" }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText(/Unsaved work may be lost/)).toBeTruthy();
    expect(closeApp).not.toHaveBeenCalled();
    expect(document.activeElement?.textContent).toBe("Cancel");
    fireEvent.click(within(dialog).getByRole("button", { name: "Close Example.exe" }));
    await waitFor(() => {
      expect(closeApp).toHaveBeenCalledWith(5);
    });
    await waitFor(() => {
      expect(preflightCleanup).toHaveBeenCalledTimes(2);
    });

    fireEvent.click(await screen.findByRole("button", { name: "Skip" }));
    const ready = screen.getByRole<HTMLButtonElement>("button", { name: /Move to Recycle Bin \(1\)/ });
    expect(ready.disabled).toBe(false);
    fireEvent.click(ready);
    const sent = executeCleanup.mock.calls[0]?.[1] as { skip: number[] } | undefined;
    expect(sent?.skip).toEqual([3, 2]);
    await screen.findByRole("heading", { name: "Cleanup finished" });
    expect(screen.getByText("Moved to Recycle Bin")).toBeTruthy();
    expect(screen.getByText(/Failed: Still in use\. \(can retry\)/)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Retry 1 failed" }));
    await screen.findByRole("heading", { name: "Review" });
    expect(retryPlan).toHaveBeenCalledWith(7);
  });

  it("explains a missing Recycle Bin before acting", async () => {
    const items = [queueEntry(1)];
    useQueue.setState({ items, refused: [] });
    const p = plan(items, {
      volumes: [{ mountPoint: "E:\\", recycleBin: { state: "unavailable", reason: "removable_drive" }, items: 1, bytes: GIB }],
      warnings: [{ kind: "cannot_recycle", id: 1, fit: { fit: "unavailable", reason: "removable_drive" } }],
    });
    setup(FLOW, { cleanup: { planCleanup: resolves(p) } }, <CleanupView />);
    fireEvent.click(screen.getByRole("button", { name: /Review 1 item/ }));
    await screen.findByRole("heading", { name: "Review" });
    expect(screen.getByText(/No Recycle Bin\. Removable drives have no Recycle Bin\./)).toBeTruthy();
    expect(screen.getByRole<HTMLButtonElement>("button", { name: "Check items" }).disabled).toBe(true);
    fireEvent.click(screen.getByRole("radio", { name: "Delete permanently" }));
    fireEvent.click(screen.getByRole("checkbox", { name: /permanently deleted items can’t be restored/ }));
    expect(screen.getByRole<HTMLButtonElement>("button", { name: "Check items" }).disabled).toBe(false);
  });

  it("restores recycled items from the undo history", async () => {
    const restoreItems = resolves([{ itemId: 11, ok: true, message: null }]);
    const fetchUndoHistory = resolves([
      {
        actionId: 3,
        kind: "cleanup" as const,
        status: "completed" as const,
        startedMs: Date.UTC(2026, 0, 2),
        finishedMs: Date.UTC(2026, 0, 2),
        itemCount: 1,
        doneCount: 1,
        failedCount: 0,
        bytesDone: GIB,
        items: [{ itemId: 11, path: "C:\\Users\\me\\Downloads\\old.iso", bytes: GIB, method: "recycle" as const, tier: "careful" as const, result: "done" as const, error: null, completedMs: 1, restorable: true, restoredMs: null }],
      },
    ]);
    setup(FLOW, { cleanup: { fetchUndoHistory, restoreItems } }, <CleanupView />);
    fireEvent.click(screen.getByRole("tab", { name: "Undo history" }));
    fireEvent.click(await screen.findByRole("button", { name: "Restore" }));
    await waitFor(() => {
      expect(restoreItems).toHaveBeenCalledWith([11]);
    });
    expect(await screen.findByText("1 restored.")).toBeTruthy();
  });
});

describe("adding to the queue", () => {
  it("reports never-tier refusals from the context-menu command", async () => {
    const services = testServices();
    const s = { ...services, capabilities: () => new Set(["cleanup_queue_add"]) };
    const mod = await import("../lib/backend");
    const call = vi.spyOn(mod, "call");
    call.mockResolvedValueOnce({ added: [], refused: [{ entryId: 4, path: "C:\\Windows", reason: "never_tier", message: "C:\\Windows is protected." }] });
    const { createStoreBus } = await import("../services");
    const bus = createStoreBus(s.capabilities);
    await bus.dispatch({ type: "addToCleanup", target: { volumeId: "fixture", ids: [4] } });
    expect(call).toHaveBeenCalledWith("cleanup_queue_add", { volumeId: "fixture", ids: [4] });
    expect(useApp.getState().status).toBe("Not added: C:\\Windows is protected.");
    call.mockRestore();
  });
});

// -----------------------------------------------------------------------------
// Duplicates
// -----------------------------------------------------------------------------

describe("duplicates", () => {
  it("never selects every copy and queues the guarded selection", async () => {
    const g = dupeGroup(1, 2);
    const queueDupes = resolves({ added: [queueEntry(1)], refused: [] });
    setup(
      ["dupes_status", "dupes_groups", "dupes_queue"],
      {
        dupes: {
          fetchDupeStatus: resolves({ state: "done" as const, phase: null, progress: null, lastRunMs: 1, groups: 1, wastedBytes: g.wastedBytes, message: null }),
          fetchDupeGroups: resolves({ total: 1, groups: [g] }),
          queueDupes,
        },
      },
      <DuplicatesView />,
    );
    const second = await screen.findByRole("checkbox", { name: /copy-1-1/ });
    fireEvent.click(second);
    expect((second as HTMLInputElement).checked).toBe(true);
    const first = screen.getByRole("checkbox", { name: /copy-1-0/ });
    fireEvent.click(first);
    expect((first as HTMLInputElement).checked).toBe(false);
    expect(screen.getByRole("alert").textContent).toMatch(/At least one copy must stay/);
    expect(screen.getByText(/Suggested keep: oldest copy/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Add to cleanup queue" }));
    await waitFor(() => {
      expect(queueDupes).toHaveBeenCalledWith([{ groupId: 1, fileIds: [1] }]);
    });
  });

  it("puts hardlink replacement behind an explicit acknowledgement", async () => {
    const g = dupeGroup(1, 3);
    const replaceWithHardlinks = resolves({ replaced: 2, bytesSaved: g.wastedBytes, failed: [] });
    setup(
      ["dupes_status", "dupes_groups", "dupes_hardlink_prompt", "dupes_hardlink"],
      {
        dupes: {
          fetchDupeStatus: resolves({ state: "done" as const, phase: null, progress: null, lastRunMs: 1, groups: 1, wastedBytes: g.wastedBytes, message: null }),
          fetchDupeGroups: resolves({ total: 1, groups: [g] }),
          prepareHardlinks: resolves({ promptId: 2, message: "Editing one copy changes all of them.", files: 2, bytesSaved: g.wastedBytes, refusedCrossVolume: 0, expiresMs: Date.now() + 60_000 }),
          replaceWithHardlinks,
        },
      },
      <DuplicatesView />,
    );
    fireEvent.click((await screen.findAllByRole("button", { name: "Select all but the suggested keep" }))[0] as HTMLElement);
    fireEvent.click(screen.getByRole("button", { name: "Replace with hardlinks…" }));
    const dialog = await screen.findByRole("dialog");
    const confirm = within(dialog).getByRole<HTMLButtonElement>("button", { name: "Replace with hardlinks" });
    expect(confirm.disabled).toBe(true);
    fireEvent.click(within(dialog).getByRole("checkbox"));
    fireEvent.click(confirm);
    await waitFor(() => {
      expect(replaceWithHardlinks).toHaveBeenCalledWith(2);
    });
  });
});

// -----------------------------------------------------------------------------
// History, charts, tools, largest, detail, routing
// -----------------------------------------------------------------------------

describe("history", () => {
  const snap = (id: number, day: number): SnapshotInfo => ({
    id,
    takenMs: Date.UTC(2026, 0, day),
    totalBytes: 1000 * GIB,
    usedBytes: (400 + day) * GIB,
    freeBytes: (600 - day) * GIB,
    allocatedSum: 0,
    logicalSum: 0,
    files: 0,
    dirs: 0,
    scanner: "mft",
  });

  it("defaults the diff picker to the last two snapshots and orders custom picks", async () => {
    const snaps = [snap(1, 1), snap(2, 5), snap(3, 9)];
    const diff = (fromId: number, toId: number): SnapshotDiff => ({
      from: snaps.find((s) => s.id === fromId) as SnapshotInfo,
      to: snaps.find((s) => s.id === toId) as SnapshotInfo,
      usedDelta: 8 * GIB,
      scannedDelta: 8 * GIB,
      grown: [{ path: "C:\\Users\\me\\models", before: null, after: { allocated: 9 * GIB, logical: 9 * GIB, files: 3 }, delta: 6 * GIB, entryId: 42 }],
      shrunk: [],
      newLarge: [],
      deletedLarge: [],
    });
    const fetchDiff = vi.fn((a: number, b: number) => Promise.resolve(diff(a, b)));
    useApp.getState().openVolume("fixture", 0);
    setup(
      ["history_snapshots", "history_usage", "history_diff"],
      {
        history: {
          fetchSnapshots: resolves(snaps),
          fetchUsage: resolves(snaps.map((s) => ({ snapshotId: s.id, atMs: s.takenMs, totalBytes: s.totalBytes, usedBytes: s.usedBytes, freeBytes: s.freeBytes }))),
          fetchDiff,
        },
      },
      <HistoryView />,
    );
    expect(await screen.findByText("+6.00 GB")).toBeTruthy();
    expect(fetchDiff).toHaveBeenLastCalledWith(2, 3, "allocated", 25);
    expect(screen.getByText(/Used space/, { selector: "p" }).textContent).toMatch(/\+8\.00 GB/);
    fireEvent.change(screen.getByRole("combobox", { name: "From" }), { target: { value: "3" } });
    fireEvent.change(screen.getByRole("combobox", { name: "To" }), { target: { value: "1" } });
    await waitFor(() => {
      expect(fetchDiff).toHaveBeenLastCalledWith(1, 3, "allocated", 25);
    });
    fireEvent.click(screen.getByRole("button", { name: "C:\\Users\\me\\models" }));
    expect(useApp.getState().primary).toBe(42);
  });
});

describe("charts", () => {
  it("renders a time chart with keyboard readout and a data table", () => {
    const points = [0, 1, 2].map((i) => ({ x: Date.UTC(2026, 0, 1 + i), y: (i + 1) * GIB }));
    render(<TimeChart title="Used space" series={[{ name: "Used", color: "red", points }]} />);
    const img = screen.getByRole("img", { name: "Used space" });
    fireEvent.keyDown(img, { key: "Home" });
    expect(screen.getByText(/Used 1\.00 GB/)).toBeTruthy();
    fireEvent.keyDown(img, { key: "ArrowRight" });
    expect(screen.getByText(/Used 2\.00 GB/)).toBeTruthy();
    expect(screen.getAllByRole("row")).toHaveLength(4);
    fireEvent.click(screen.getByRole("button", { name: "Show as table" }));
    expect(screen.getByRole("table").className).toBe("table");
  });

  it("labels a sparkline with first and last values and handles gaps", () => {
    render(
      <Sparkline
        label="Size history of models"
        points={[
          { atMs: Date.UTC(2026, 0, 1), bytes: GIB },
          { atMs: Date.UTC(2026, 0, 2), bytes: null },
          { atMs: Date.UTC(2026, 0, 3), bytes: 3 * GIB },
        ]}
      />,
    );
    expect(screen.getByRole("img").getAttribute("aria-label")).toMatch(/1\.00 GB on .* to 3\.00 GB on /);
    expect(screen.getByText("Below threshold")).toBeTruthy();
  });
});

describe("tools", () => {
  it("shows the exact command and runs only after confirmation", async () => {
    const runTool = vi.fn((_id: number, onOutput: (l: { stream: "stdout"; line: string }) => void) => {
      onOutput({ stream: "stdout", line: "The operation completed successfully." });
      return Promise.resolve({ exitCode: 0, output: "" });
    });
    const prepareTool = resolves({
      promptId: 9,
      title: "Clean up Windows component store",
      description: "Runs DISM component cleanup as administrator.",
      commandLine: "C:\\Windows\\System32\\Dism.exe /Online /Cleanup-Image /StartComponentCleanup",
      launch: "elevated" as const,
      capturesOutput: true,
      removedFlags: [],
      expiresMs: Date.now() + 60_000,
      recycleBin: null,
    });
    setup(["tools_prepare", "tools_run"], { tools: { prepareTool, runTool } }, <ToolsView />);
    fireEvent.click(screen.getByRole("button", { name: "Clean up the component store (DISM)…" }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText(/Dism\.exe \/Online \/Cleanup-Image \/StartComponentCleanup/)).toBeTruthy();
    expect(runTool).not.toHaveBeenCalled();
    fireEvent.click(within(dialog).getByRole("button", { name: "Run as administrator" }));
    expect(await screen.findByText("Finished successfully.")).toBeTruthy();
    expect(screen.getByLabelText(/output/).textContent).toMatch(/completed successfully/);
  });

  it("explains that tools need the engine when unavailable", () => {
    setup([], {}, <ToolsView />);
    expect(screen.getByText("Running Windows tools isn’t available in this build")).toBeTruthy();
    expect(screen.getByRole<HTMLButtonElement>("button", { name: "Empty Recycle Bin…" }).disabled).toBe(true);
  });
});

describe("largest files", () => {
  it("lists entries, selects for details, and refuses to queue never-tier ones", async () => {
    useApp.getState().openVolume("fixture", 0);
    const fetchLargest = resolves({
      matched: 2,
      entries: [
        { id: 5, name: "movie.mkv", path: "D:\\Media\\movie.mkv", bytes: 4 * GIB, modifiedMs: null, category: 10, safety: "careful" as const, app: null, extension: "mkv" },
        { id: 6, name: "pagefile.sys", path: "C:\\pagefile.sys", bytes: 8 * GIB, modifiedMs: null, category: 1, safety: "never" as const, app: null, extension: "sys" },
      ],
    });
    setup(["insights_largest", "cleanup_queue_add"], { insights: { fetchLargest } }, <LargestView />);
    fireEvent.click(await screen.findByRole("button", { name: /movie\.mkv, 4\.00 GB/ }));
    expect(useApp.getState().selection).toEqual([5]);
    const never = screen.getByRole("button", { name: "Add pagefile.sys to cleanup" });
    expect(never.getAttribute("aria-disabled")).toBe("true");
    fireEvent.click(never);
    expect(useApp.getState().status).toMatch(/Protected \(Never tier\)/);
    fireEvent.change(screen.getByRole("textbox", { name: "Extensions" }), { target: { value: ".MKV" } });
    fireEvent.blur(screen.getByRole("textbox", { name: "Extensions" }));
    await waitFor(() => {
      expect(fetchLargest).toHaveBeenLastCalledWith(expect.objectContaining({ filters: expect.objectContaining({ extensions: ["mkv"] }) as unknown }));
    });
  });
});

describe("detail panel additions", () => {
  it("explains the classification and fetches the size history", async () => {
    const services = testServices();
    const base = await services.fetchDetail("fixture", 0);
    const detail: EntryDetail = { ...base, isDir: true, history: null, path: "C:\\Users\\me\\models" };
    const explainPath = resolves({ path: detail.path, result: { category: 4, safety: "careful" as const, ruleId: "ai.models", regenerable: false }, rule: null, originPath: "C:\\Users\\me", steps: [], trace: [] });
    const fetchDirSeries = resolves([
      { atMs: Date.UTC(2026, 0, 1), allocated: GIB, logical: GIB },
      { atMs: Date.UTC(2026, 0, 8), allocated: 2 * GIB, logical: 2 * GIB },
    ]);
    useApp.getState().openVolume("fixture", 0);
    useApp.getState().select([0], 0);
    setup(["rules_explain", "history_dir_series"], { settings: { explainPath }, history: { fetchDirSeries } }, <DetailPanel />, { ...services, fetchDetail: () => Promise.resolve(detail) });
    expect((await screen.findByRole("img", { name: /Size history of/ })).getAttribute("aria-label")).toMatch(/2\.00 GB/);
    fireEvent.click(screen.getByRole("button", { name: /Why is this classified as/ }));
    expect(await screen.findByText(/Inherited from/)).toBeTruthy();
    expect(explainPath).toHaveBeenCalledWith("C:\\Users\\me\\models");
  });
});

describe("routing", () => {
  it("lazy-loads feature views from the activity bar and gates volume views", async () => {
    render(<App services={testServices()} />);
    const areas = screen.getByRole("navigation", { name: "Areas" });
    fireEvent.click(within(areas).getByRole("button", { name: "Insights" }));
    const sidebar = await screen.findByRole("navigation", { name: "Insights sidebar" });
    const largest = within(sidebar).getByRole("button", { name: "Largest files" });
    expect(largest.getAttribute("aria-disabled")).toBe("true");
    fireEvent.click(within(areas).getByRole("button", { name: "Settings" }));
    expect(await screen.findByRole("heading", { name: "Settings", level: 1 })).toBeTruthy();
    expect(screen.getByText(/Saving settings isn’t available in this build/)).toBeTruthy();
    expect(screen.queryByRole("navigation", { name: /sidebar/ })).toBeNull();
    await flush();
  });

  it("shows the queue count in the activity bar", () => {
    useQueue.setState({ items: [queueEntry(1), queueEntry(2)], refused: [] });
    render(<App services={testServices()} />);
    expect(screen.getByRole("button", { name: "Cleanup, 2 queued" })).toBeTruthy();
  });
});
