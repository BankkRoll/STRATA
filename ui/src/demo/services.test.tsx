import { render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { ENTRY_ACTIONS } from "../lib/commands";
import { decodeFrame, ViewKind } from "../lib/layout/frame";
import { NO_FILTERS } from "../lib/types";
import { ServicesContext } from "../services";
import { fixtureBytes } from "../test/fixtures";
import { resetStores } from "../test/services";
import { FEATURE_VIEWS, FeatureRouter } from "../views/featureViews";
import { DEMO_CAPABILITIES, DEMO_UNAVAILABLE_REASON, demoServices } from "./services";

/** Commands the Explore area reads; nothing else may be in the demo set. */
const EXPLORE = ["list_volumes", "layout_open", "layout_request", "layout_close", "entry_info", "entry_detail", "list_children", "search_open", "search_query", "search_close"];

describe("demo services", () => {
  afterEach(resetStores);
  const services = () => demoServices(fixtureBytes("tree.bin"));

  it("enable only Explore", () => {
    expect([...DEMO_CAPABILITIES].sort()).toEqual([...EXPLORE].sort());
    expect(services().capabilities()).toBe(DEMO_CAPABILITIES);
  });

  it.each(FEATURE_VIEWS.filter((v) => v.id !== "settings" && !v.needsVolume).map((v) => v.id))("show %s as available in the desktop app", async (view) => {
    render(
      <ServicesContext value={services()}>
        <FeatureRouter view={view} />
      </ServicesContext>,
    );
    expect((await screen.findAllByText(DEMO_UNAVAILABLE_REASON)).length).toBeGreaterThan(0);
  });

  it("disable every entry action that needs the engine, with the demo reason", () => {
    const s = services();
    for (const a of ENTRY_ACTIONS.filter((x) => x.requires)) {
      expect(s.bus.availability({ type: a.type, target: { volumeId: "fixture", ids: [1] } })).toEqual({ enabled: false, reason: DEMO_UNAVAILABLE_REASON });
    }
    expect(s.bus.availability({ type: "showInList", target: { volumeId: "fixture", ids: [1] } })).toEqual({ enabled: true });
  });

  it("refuse scans instead of faking them", async () => {
    await expect(services().volumes.startScan("fixture", "auto")).rejects.toMatchObject({ reason: DEMO_UNAVAILABLE_REASON });
  });

  it("lay out any folder at the requested size", async () => {
    const stream = services().createLayoutStream();
    const frame = new Promise<ReturnType<typeof decodeFrame>>((resolve) => stream.subscribe(resolve));
    stream.request({ volumeId: "fixture", root: 21, view: "treemap", width: 1500, height: 700, dpr: 1, transform: { scale: 1, tx: 0, ty: 0 }, sizeMode: "allocated", style: "cushion", filters: NO_FILTERS, animate: false });
    const f = await frame;
    expect([f.view, f.root, f.width, f.height]).toEqual([ViewKind.Treemap, 21, 1500, 700]);
    expect(f.nodes.geom(0, 2)).toBe(1500);
  });
});
