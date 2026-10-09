import { beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.fn<(cmd: string, args?: Record<string, unknown>) => Promise<unknown>>();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: Record<string, unknown>) => invoke(cmd, args),
  isTauri: () => true,
}));

const { clearData, reportIssue, restartToUpdate } = await import("./settings");

describe("settings bridges", () => {
  beforeEach(() => {
    invoke.mockReset();
    invoke.mockResolvedValue(null);
  });

  it("asks the backend to open the bug form, sending nothing about the user", async () => {
    await reportIssue();
    expect(invoke).toHaveBeenCalledWith("report_issue", undefined);
  });

  it("clears app data by kind and restarts into an update", async () => {
    await clearData("caches");
    expect(invoke).toHaveBeenCalledWith("data_clear", { what: "caches" });
    await restartToUpdate();
    expect(invoke).toHaveBeenLastCalledWith("updates_restart", undefined);
  });
});
