/**
 * Services for the website demo: the app's fixture services over the sample
 * tree, with layouts computed in the page and a capability set that lists
 * only what the demo really does (browsing the sample volume).
 *
 * Everything else (insights, cleanup, history, activity, settings, scans,
 * file actions) is missing from the capabilities, so the app shows its own
 * unavailable states with {@link DEMO_UNAVAILABLE_REASON}. Nothing is faked.
 */
import { decodeTree, fixtureServices, type FixtureData } from "../dev/fixtureServices";
import { BackendUnavailableError } from "../lib/backend";
import { CommandBus, ENTRY_ACTIONS, type Availability, type EntryCommand } from "../lib/commands";
import type { EntryDetail } from "../lib/detail";
import { storeBusDeps, type Services } from "../services";
import { DemoLayoutStream } from "./stream";

/** Why everything but browsing is unavailable in the demo. */
export const DEMO_UNAVAILABLE_REASON = "Available in the desktop app. This demo shows sample data only.";

/**
 * Backend commands the demo services answer: volume list, layouts, entry
 * info and details, folder listings and name search. Every Explore surface
 * works with these; no other area's command is here.
 */
export const DEMO_CAPABILITIES: ReadonlySet<string> = new Set([
  "list_volumes",
  "layout_open",
  "layout_request",
  "layout_close",
  "entry_info",
  "entry_detail",
  "list_children",
  "search_open",
  "search_query",
  "search_close",
]);

/** Names shown for the sample volume and its root folder. */
export const SAMPLE_LABELS = { volume: "Sample volume", root: "Sample volume" } as const;

/** Entry actions whose command is missing get the demo's reason instead of "not in this build". */
class DemoBus extends CommandBus {
  override availability(cmd: EntryCommand): Availability {
    const a = super.availability(cmd);
    if (a.enabled) return a;
    const info = ENTRY_ACTIONS.find((x) => x.type === cmd.type);
    const missing = info?.requires != null && !DEMO_CAPABILITIES.has(info.requires);
    const applies = cmd.target.ids.length > 0 && !(info?.single && cmd.target.ids.length > 1);
    return missing && applies ? { enabled: false, reason: DEMO_UNAVAILABLE_REASON } : a;
  }
}

/** The fixture detail with the wording that only makes sense in the dev harness replaced. */
function sampleDetail(d: EntryDetail): EntryDetail {
  return {
    ...d,
    classification: d.classification && {
      ...d.classification,
      ruleId: "sample",
      ruleName: "Sample data",
      explain: "Categories, apps and safety tiers in the sample are generated. The desktop app matches every path against its rule packs.",
    },
    attribution: d.attribution ? { ...d.attribution, evidence: ["Sample data"] } : null,
    safety: d.safety ? { ...d.safety, why: "Generated for the sample." } : null,
    // Size history comes from snapshots, which the demo does not have.
    history: null,
  };
}

/**
 * Builds the demo services over the decoded sample tree.
 *
 * @param tree - `tree.bin` from the layout fixtures.
 * @returns Services for the real app shell.
 */
export function demoServices(tree: ArrayBuffer): Services {
  const decoded = decodeTree(tree);
  const data: FixtureData = { large: false, ...decoded, frames: new Map(), drillRoot: -1, labels: SAMPLE_LABELS };
  const base = fixtureServices(data);
  const unavailable = (command: string) => Promise.reject(new BackendUnavailableError(command, DEMO_UNAVAILABLE_REASON));
  return {
    ...base,
    createLayoutStream: () => new DemoLayoutStream(decoded),
    fetchDetail: async (volumeId, id) => sampleDetail(await base.fetchDetail(volumeId, id)),
    volumes: {
      ...base.volumes,
      sinceLastScan: () => Promise.resolve(null),
      startScan: () => unavailable("scan_start"),
      cancelScan: () => unavailable("scan_cancel"),
      elevate: () => unavailable("helper_elevate"),
    },
    capabilities: () => DEMO_CAPABILITIES,
    unavailableReason: DEMO_UNAVAILABLE_REASON,
    bus: new DemoBus(storeBusDeps(() => DEMO_CAPABILITIES)),
  };
}
