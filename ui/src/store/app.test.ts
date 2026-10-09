// @vitest-environment node
import { beforeEach, describe, expect, it } from "vitest";
import { INITIAL_APP_STATE, isVisualView, selectRoot, useApp } from "./app";

describe("app store", () => {
  beforeEach(() => {
    useApp.setState({ ...INITIAL_APP_STATE });
  });

  it("opens a volume on the last visual view", () => {
    useApp.getState().setView("sunburst");
    useApp.getState().setView("home");
    useApp.getState().openVolume("vol", 5);
    const s = useApp.getState();
    expect(s.view).toBe("sunburst");
    expect(s.path).toEqual([5]);
    expect(selectRoot(s)).toBe(5);
  });

  it("drills, goes up selecting the folder it left, and jumps", () => {
    const a = useApp.getState();
    a.openVolume("vol", 1);
    a.drillTo([2, 3]);
    expect(useApp.getState().path).toEqual([1, 2, 3]);
    expect(useApp.getState().primary).toBe(3);
    expect(useApp.getState().goUp()).toBe(true);
    expect(useApp.getState().path).toEqual([1, 2]);
    expect(useApp.getState().selection).toEqual([3]);
    a.drillTo([4]);
    a.jumpTo(0);
    expect(useApp.getState().path).toEqual([1]);
    expect(useApp.getState().primary).toBe(2);
    expect(useApp.getState().goUp()).toBe(false);
    a.jumpTo(5);
    expect(useApp.getState().path).toEqual([1]);
  });

  it("selects, toggles and clears", () => {
    const a = useApp.getState();
    a.select([1, 2]);
    expect(useApp.getState().primary).toBe(2);
    a.toggleSelect(3);
    expect(useApp.getState().selection).toEqual([1, 2, 3]);
    a.toggleSelect(3);
    expect(useApp.getState()).toMatchObject({ selection: [1, 2], primary: 2 });
    a.clearSelection();
    expect(useApp.getState()).toMatchObject({ selection: [], primary: null });
  });

  it("keeps root, selection and modes when switching views", () => {
    const a = useApp.getState();
    a.openVolume("vol", 1);
    a.drillTo([9]);
    a.select([11]);
    a.setSizeMode("logical");
    a.setColorMode("age");
    a.setView("bubbles");
    a.setView("icicle");
    expect(useApp.getState()).toMatchObject({ path: [1, 9], selection: [11], sizeMode: "logical", colorMode: "age", view: "icicle" });
  });

  it("excludes entries and drops them from the selection", () => {
    const a = useApp.getState();
    a.select([1, 2], 2);
    a.exclude([2]);
    a.exclude([2, 3]);
    expect(useApp.getState().filters.excluded).toEqual([2, 3]);
    expect(useApp.getState()).toMatchObject({ selection: [1], primary: null });
  });

  it("toggles panes and records status", () => {
    const a = useApp.getState();
    a.togglePane("list");
    expect(useApp.getState().panes.list).toBe(false);
    a.setPane("list", true);
    a.notify("hi");
    expect(useApp.getState()).toMatchObject({ panes: { list: true }, status: "hi" });
    expect(isVisualView("home")).toBe(false);
    expect(isVisualView("mindmap")).toBe(true);
  });
});
