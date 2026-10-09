import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { useApp } from "../store/app";
import { useInsights } from "../store/insights";
import { useQueue } from "../store/queue";
import { queueEntry, resolves } from "../test/features";
import { renderFeature } from "../test/render";
import { resetStores } from "../test/services";
import { ActivityView } from "./ActivityView";
import { AppsView } from "./AppsView";
import { CategoriesView } from "./CategoriesView";
import { FileTypesView } from "./FileTypesView";
import { RecommendationsView } from "./RecommendationsView";

const GIB = 1024 ** 3;

beforeEach(() => {
  resetStores();
  useQueue.setState({ items: [], refused: [] });
});

describe("file types and categories", () => {
  it("opens Largest files filtered by the chosen extension", async () => {
    useApp.getState().openVolume("fixture", 0);
    renderFeature(
      ["insights_file_types"],
      {
        insights: {
          fetchFileTypes: resolves({
            totalBytes: 10 * GIB,
            byExtension: [
              { extension: "mkv", group: "Video", files: 3, bytes: 8 * GIB, mismatched: 0 },
              { extension: "", group: "Other", files: 9, bytes: 2 * GIB, mismatched: 1 },
            ],
            byDetectedType: [{ label: "GGUF model", files: 1, bytes: GIB }],
          }),
        },
      },
      <FileTypesView />,
    );
    expect(await screen.findByText("GGUF model")).toBeTruthy();
    expect(screen.getByRole("list", { name: "Largest file types" }).textContent).toMatch(/80\.0%/);
    fireEvent.click(screen.getByRole("button", { name: /List largest files of type \.mkv/ }));
    expect(useApp.getState().view).toBe("largest");
    expect(useInsights.getState().largest.extensions).toEqual(["mkv"]);
  });

  it("drills from a category into the map filter", async () => {
    useApp.getState().openVolume("fixture", 0);
    renderFeature(
      ["insights_categories"],
      { insights: { fetchCategories: resolves([{ category: 6, bytes: 5 * GIB, files: 40, top: [{ id: 9, name: "npm-cache", path: "C:\\Users\\me\\AppData\\Local\\npm-cache", bytes: 4 * GIB }] }]) } },
      <CategoriesView />,
    );
    fireEvent.click(await screen.findByRole("button", { name: /Show details of Caches/ }));
    fireEvent.click(screen.getByRole("button", { name: "npm-cache" }));
    expect(useApp.getState().primary).toBe(9);
    fireEvent.click(screen.getByRole("button", { name: "Show only this on the map" }));
    expect(useApp.getState().filters.categories).toEqual([6]);
    expect(useApp.getState().view).toBe("treemap");
  });

  it("shows the unavailable state without the command", () => {
    useApp.getState().openVolume("fixture", 0);
    renderFeature([], {}, <CategoriesView />);
    expect(screen.getByText("Categories isn’t available in this build")).toBeTruthy();
  });
});

describe("apps", () => {
  it("shows confidence, evidence and mismatches, and confirms the uninstaller", async () => {
    const prepareTool = resolves({
      promptId: 4,
      title: "Uninstall Example",
      description: "Runs Example's own uninstaller.",
      commandLine: '"C:\\Program Files\\Example\\uninstall.exe"',
      launch: "shell_open" as const,
      capturesOutput: false,
      removedFlags: ["/S"],
      expiresMs: Date.now() + 60_000,
      recycleBin: null,
    });
    const runTool = resolves({ exitCode: null, output: "" });
    renderFeature(
      ["apps_footprint", "tools_prepare", "tools_run", "apps_queue_caches"],
      {
        insights: {
          fetchApps: resolves([
            {
              id: "example",
              name: "Example",
              publisher: "Example Corp",
              version: "1.2",
              source: "registry" as const,
              confidence: "exact" as const,
              evidence: ["InstallLocation matches"],
              totalBytes: 6 * GIB,
              locations: [{ kind: "cache" as const, path: "C:\\Users\\me\\AppData\\Local\\Example\\Cache", volumeId: "fixture", entryId: null, bytes: 5 * GIB, safety: "safe" as const }],
              registryEstimateBytes: GIB / 4,
              mismatch: true,
              canUninstall: true,
              cacheBytes: 5 * GIB,
              running: false,
            },
          ]),
        },
        tools: { prepareTool, runTool },
      },
      <AppsView />,
    );
    fireEvent.click(await screen.findByRole("button", { name: /^Example/ }));
    expect(screen.getByText("Exact confidence")).toBeTruthy();
    expect(screen.getByText("InstallLocation matches")).toBeTruthy();
    expect(screen.getByText(/registry estimate is far off/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Uninstall…" }));
    expect(prepareTool).toHaveBeenCalledWith({ kind: "uninstall", appId: "example" });
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText(/Silent flags removed/).textContent).toMatch(/\/S/);
    fireEvent.click(within(dialog).getByRole("button", { name: "Run" }));
    await waitFor(() => {
      expect(runTool).toHaveBeenCalledWith(4, expect.any(Function));
    });
  });
});

describe("recommendations", () => {
  it("explains, previews and queues with deselected items left out", async () => {
    const queueRecommendation = resolves({ added: [queueEntry(1)], refused: [] });
    renderFeature(
      ["recommendations_list", "recommendations_preview", "recommendations_queue"],
      {
        insights: {
          fetchRecommendations: resolves([
            {
              id: "caches",
              kind: "caches",
              title: "Caches you can safely clear",
              summary: "in 2 apps",
              explain: "Apps rebuild these on demand.",
              bytes: 3 * GIB,
              items: 2,
              safety: "safe" as const,
              action: { kind: "queue" as const },
            },
          ]),
          previewRecommendation: resolves({
            total: 2,
            items: [
              { volumeId: "fixture", entryId: 11, path: "C:\\Users\\me\\AppData\\Local\\A\\Cache", bytes: 2 * GIB, safety: "safe" as const, explain: "" },
              { volumeId: "fixture", entryId: 12, path: "C:\\Users\\me\\AppData\\Local\\B\\Cache", bytes: GIB, safety: "safe" as const, explain: "" },
            ],
          }),
          queueRecommendation,
        },
      },
      <RecommendationsView />,
    );
    expect(await screen.findByText(/Up to/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Why & preview" }));
    expect(screen.getByText("Apps rebuild these on demand.")).toBeTruthy();
    fireEvent.click(await screen.findByRole("checkbox", { name: /B\\Cache/ }));
    fireEvent.click(screen.getByRole("button", { name: "Add to cleanup queue" }));
    await waitFor(() => {
      expect(queueRecommendation).toHaveBeenCalledWith("caches", [12]);
    });
    expect(useApp.getState().status).toBe("1 item added to the cleanup queue.");
  });
});

describe("activity", () => {
  const status = { enabled: false, running: false, needsHelper: false, throttled: false, cpuPercent: null, sinceMs: null, retentionDays: 30 };

  it("explains the opt-in before enabling", async () => {
    const setActivityEnabled = resolves({ ...status, enabled: true });
    renderFeature(["activity_status", "activity_set_enabled"], { activity: { fetchActivityStatus: resolves(status), setActivityEnabled } }, <ActivityView />);
    expect(await screen.findByText(/never transmitted/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Turn on activity tracking" }));
    await waitFor(() => {
      expect(setActivityEnabled).toHaveBeenCalledWith(true);
    });
  });

  it("lists top writers and clears data after confirmation", async () => {
    const clearActivity = resolves(null);
    const fetchTopWriters = resolves([
      {
        image: "C:\\Program Files\\Example\\example.exe",
        name: "example.exe",
        bytesWritten: 2 * GIB,
        filesCreated: 10,
        filesDeleted: 2,
        topDirs: [{ path: "C:\\Users\\me\\AppData\\Local\\Example", bytesWritten: 2 * GIB }],
      },
    ]);
    renderFeature(
      ["activity_status", "activity_top", "activity_clear"],
      { activity: { fetchActivityStatus: resolves({ ...status, enabled: true, running: true }), fetchTopWriters, clearActivity } },
      <ActivityView />,
    );
    expect(await screen.findByText("example.exe")).toBeTruthy();
    expect(fetchTopWriters).toHaveBeenCalledWith("hour", 25);
    fireEvent.click(screen.getByRole("radio", { name: "Today" }));
    await waitFor(() => {
      expect(fetchTopWriters).toHaveBeenLastCalledWith("today", 25);
    });
    fireEvent.click(screen.getByRole("button", { name: "Clear activity data" }));
    expect(clearActivity).not.toHaveBeenCalled();
    fireEvent.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Clear data" }));
    await waitFor(() => {
      expect(clearActivity).toHaveBeenCalled();
    });
  });
});
