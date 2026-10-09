import { beforeEach, describe, expect, it } from "vitest";
import { applyAddResult, useQueue } from "../store/queue";
import { defaultSettings, dupeGroup, plan, queueEntry } from "../test/features";
import { arrangeApps } from "../views/AppsView";
import { parseExtensions } from "../views/LargestView";
import { byteTicks, formatByteTick, formatDelta, linePath, linearScale, nearestIndex, niceTicks } from "./chart";
import { reviewBlockers, tierTotals, type Decision } from "./cleanup";
import { selectAllButKeep, selectionIsSafe, toggleDupe } from "./dupes";
import { orderPick, type SnapshotInfo } from "./history";
import type { AppFootprint } from "./insights";
import { changedKeys, validateSettings } from "./settings";

const GIB = 1024 ** 3;

function decision(patch: Partial<Decision> = {}): Decision {
  return { method: "recycle_bin", acks: { careful: [], permanent: false, largePermanent: false, permanentInsteadOfRecycle: [] }, skip: [], ...patch };
}

describe("review rules", () => {
  it("requires a tick for every included Careful item", () => {
    const p = plan([queueEntry(1), queueEntry(2, { safety: "careful", name: "models" })]);
    expect(reviewBlockers(p, decision()).map((b) => b.id)).toEqual([2]);
    expect(reviewBlockers(p, decision({ acks: { ...decision().acks, careful: [2] } }))).toEqual([]);
    expect(reviewBlockers(p, decision({ skip: [2] }))).toEqual([]);
  });

  it("ignores never-tier items and blocks an empty selection", () => {
    const p = plan([queueEntry(1, { safety: "never" })]);
    expect(reviewBlockers(p, decision()).map((b) => b.message)).toEqual(["Select at least one item."]);
  });

  it("needs the permanent confirmation, and a second one above the threshold", () => {
    const p = plan([queueEntry(1), queueEntry(20)]);
    const perm = decision({ method: "permanent" });
    expect(reviewBlockers(p, perm).map((b) => b.message)).toEqual(["Confirm permanent deletion.", expect.stringMatching(/one item is above/)]);
    const acked = { ...perm, acks: { ...perm.acks, permanent: true } };
    expect(reviewBlockers(p, acked)).toHaveLength(1);
    expect(reviewBlockers(p, { ...acked, acks: { ...acked.acks, largePermanent: true } })).toEqual([]);
  });

  it("makes the user decide for items the Recycle Bin can't take", () => {
    const p = plan([queueEntry(1)], { warnings: [{ kind: "cannot_recycle", id: 1, fit: { fit: "unavailable", reason: "removable_drive" } }] });
    expect(reviewBlockers(p, decision())[0]?.id).toBe(1);
    const instead = decision({ acks: { ...decision().acks, permanentInsteadOfRecycle: [1] } });
    expect(reviewBlockers(p, instead).map((b) => b.message)).toEqual([expect.stringMatching(/Confirm permanent deletion/)]);
    expect(reviewBlockers(p, { ...instead, acks: { ...instead.acks, permanent: true } })).toEqual([]);
    expect(reviewBlockers(p, decision({ skip: [1] })).map((b) => b.message)).toEqual(["Select at least one item."]);
  });

  it("totals by tier excluding skipped items", () => {
    const t = tierTotals([queueEntry(1), queueEntry(2), queueEntry(3, { safety: "careful" })], new Set([2]));
    expect(t).toEqual([
      { safety: "safe", items: 1, bytes: GIB },
      { safety: "probably", items: 0, bytes: 0 },
      { safety: "careful", items: 1, bytes: 3 * GIB },
      { safety: "never", items: 0, bytes: 0 },
    ]);
  });
});

describe("queue add results", () => {
  beforeEach(() => {
    useQueue.setState({ items: [], refused: [] });
  });

  it("explains never-tier refusals and records them", () => {
    const msg = applyAddResult({
      added: [queueEntry(1)],
      refused: [{ entryId: 5, path: "C:\\Windows", reason: "never_list", message: "C:\\Windows is part of Windows and is never deleted." }],
    });
    expect(msg).toBe("1 item added to the cleanup queue. Not added: C:\\Windows is part of Windows and is never deleted.");
    expect(useQueue.getState().items).toHaveLength(1);
    expect(useQueue.getState().refused[0]?.reason).toBe("never_list");
  });
});

describe("duplicate guardrails", () => {
  it("never lets every copy be selected", () => {
    const g = dupeGroup(1, 3);
    let sel = toggleDupe(g, [], 1);
    expect(sel).toEqual({ ok: true, selected: [1] });
    sel = toggleDupe(g, sel.selected, 2);
    expect(sel.ok).toBe(true);
    const last = toggleDupe(g, sel.selected, 0);
    expect(last.ok).toBe(false);
    expect(last.selected).toEqual([1, 2]);
    expect(toggleDupe(g, [1, 2], 2)).toEqual({ ok: true, selected: [1] });
  });

  it("refuses never-tier copies and keeps the suggestion", () => {
    const g = dupeGroup(2, 3);
    const locked = { ...g, files: g.files.map((f) => (f.fileId === 2 ? { ...f, safety: "never" as const } : f)) };
    expect(toggleDupe(locked, [], 2).ok).toBe(false);
    expect(selectAllButKeep(g)).toEqual([1, 2]);
    expect(selectAllButKeep(locked)).toEqual([1]);
  });

  it("validates a whole selection", () => {
    const g = dupeGroup(3, 2);
    const groups = new Map([[g.id, g]]);
    expect(selectionIsSafe(groups, [{ groupId: 3, fileIds: [1] }])).toBe(true);
    expect(selectionIsSafe(groups, [{ groupId: 3, fileIds: [0, 1] }])).toBe(false);
    expect(selectionIsSafe(groups, [{ groupId: 9, fileIds: [0] }])).toBe(false);
  });
});

describe("settings validation", () => {
  it("accepts the defaults", () => {
    expect(validateSettings(defaultSettings())).toEqual([]);
  });

  it("mirrors the store's ranges and cross-field rules", () => {
    const s = defaultSettings();
    s.live.update_tick_ms = 50;
    s.history.thin_after_days = 200;
    s.scan.exclude_globs = ["ok", "  "];
    s.activity.cpu_cap_percent = Number.NaN;
    s.rules.user_rules_dir = " ";
    expect(validateSettings(s).map((i) => i.key)).toEqual([
      "scan.exclude_globs",
      "live.update_tick_ms",
      "activity.cpu_cap_percent",
      "history.thin_after_days",
      "rules.user_rules_dir",
    ]);
  });

  it("lists changed leaf keys", () => {
    const a = defaultSettings();
    const b = defaultSettings();
    b.appearance.theme = "dark";
    b.scan.exclude_globs = ["x"];
    expect(changedKeys(a, b)).toEqual(["scan.exclude_globs", "appearance.theme"]);
  });
});

describe("history", () => {
  const snap = (id: number, takenMs: number) => ({ id, takenMs }) as SnapshotInfo;
  it("orders a diff pick oldest first", () => {
    const s = [snap(1, 100), snap(2, 50), snap(3, 200)];
    expect(orderPick(s, 3, 2)).toEqual([2, 3]);
    expect(orderPick(s, 1, 1)).toBeNull();
    expect(orderPick(s, 1, 9)).toBeNull();
  });
});

describe("chart formatting", () => {
  it("picks round ticks covering the data", () => {
    expect(niceTicks(0, 97, 4)).toEqual([0, 25, 50, 75, 100]);
    const flat = niceTicks(5, 5);
    expect((flat[0] ?? 9) <= 5 && (flat[flat.length - 1] ?? 0) >= 5).toBe(true);
    expect(niceTicks(0, 0)).toEqual([0, 1]);
  });

  it("puts byte ticks on unit boundaries and labels them compactly", () => {
    const t = byteTicks(0, 900 * GIB, 4);
    expect(t[0]).toBe(0);
    expect((t[t.length - 1] ?? 0) >= 900 * GIB).toBe(true);
    expect(t.map((v) => formatByteTick(v))).toEqual(["0", "250 GB", "500 GB", "750 GB", "1,000 GB"]);
    expect(formatByteTick(1.5 * 1024 * GIB)).toBe("1.5 TB");
    expect(formatByteTick(2 * 1000 ** 3, "si")).toBe("2 GB");
  });

  it("builds paths with gaps and maps a flat domain to the middle", () => {
    const sx = linearScale([0, 10], [0, 100]);
    const sy = linearScale([0, 1], [10, 0]);
    expect(linePath([{ x: 0, y: 0 }, { x: 5, y: null }, { x: 10, y: 1 }], sx, sy)).toBe("M0.0,10.0M100.0,0.0");
    expect(linePath([{ x: 0, y: 0 }, { x: 10, y: 1 }], sx, sy)).toBe("M0.0,10.0L100.0,0.0");
    expect(linearScale([3, 3], [0, 10])(3)).toBe(5);
    expect(nearestIndex([{ x: 0, y: 1 }, { x: 10, y: 1 }], 7)).toBe(1);
  });

  it("formats deltas with explicit signs", () => {
    expect(formatDelta(GIB)).toBe("+1.00 GB");
    expect(formatDelta(-1024 * 1024)).toBe("−1.00 MB");
    expect(formatDelta(0)).toBe("±0 B");
  });
});

describe("insight helpers", () => {
  it("normalizes extension filters", () => {
    expect(parseExtensions(" .MP4, mkv;*.iso  mp4")).toEqual(["mp4", "mkv", "iso"]);
  });

  it("filters and sorts apps", () => {
    const app = (name: string, totalBytes: number, mismatch = false) => ({ id: name, name, publisher: null, totalBytes, mismatch }) as AppFootprint;
    const apps = [app("Beta", 5), app("alpha", 10), app("Gamma", 1, true)];
    expect(arrangeApps(apps, "", "size").map((a) => a.name)).toEqual(["alpha", "Beta", "Gamma"]);
    expect(arrangeApps(apps, "", "name").map((a) => a.name)).toEqual(["alpha", "Beta", "Gamma"]);
    expect(arrangeApps(apps, "", "mismatch").map((a) => a.name)).toEqual(["Gamma"]);
    expect(arrangeApps(apps, "ALP", "size").map((a) => a.name)).toEqual(["alpha"]);
  });
});
