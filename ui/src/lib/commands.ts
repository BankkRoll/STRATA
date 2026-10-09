/**
 * Typed command dispatch for entry actions (context menu, keyboard, palette).
 *
 * Every action is a {@link EntryCommand}; the {@link CommandBus} decides
 * availability up front so menus can show unavailable actions *disabled with
 * a reason* rather than hiding them or pretending they worked. Backend
 * actions are available only when `app_capabilities` lists their command.
 */
import { applyAddResult } from "../store/queue";
import { call, errorMessage } from "./backend";
import { addToQueue } from "./cleanup";
import { fetchEntryPath } from "./entries";

/** Actions on entries, as offered by the context menu. */
export type EntryAction =
  | "open"
  | "reveal"
  | "copyPath"
  | "properties"
  | "addToCleanup"
  | "showInList"
  | "explain"
  | "excludeFromView"
  | "openTerminal";

/** What an action applies to. */
export interface EntryTarget {
  volumeId: string;
  /** Entry ids; never aggregate placeholders. */
  ids: number[];
}

/** One dispatched command. */
export interface EntryCommand {
  type: EntryAction;
  target: EntryTarget;
}

/** Whether a command can run now. */
export type Availability = { enabled: true } | { enabled: false; reason: string };

/** Static description of an action. */
export interface EntryActionInfo {
  type: EntryAction;
  label: string;
  /** Keyboard hint shown in menus. */
  shortcut?: string;
  /** Backend command it needs, if any. */
  requires: string | null;
  /** Only meaningful for exactly one entry. */
  single: boolean;
}

/** Context-menu order with separators implied by `group`. */
export const ENTRY_ACTIONS: readonly (EntryActionInfo & { group: number })[] = [
  { type: "open", label: "Open", shortcut: "Ctrl+O", requires: "entry_action", single: false, group: 0 },
  { type: "reveal", label: "Reveal in Explorer", shortcut: "Ctrl+E", requires: "entry_action", single: false, group: 0 },
  { type: "openTerminal", label: "Open terminal here", requires: "entry_action", single: true, group: 0 },
  { type: "copyPath", label: "Copy path", shortcut: "Ctrl+Shift+C", requires: "entry_path", single: false, group: 1 },
  { type: "properties", label: "Properties", shortcut: "Alt+Enter", requires: "entry_action", single: true, group: 1 },
  { type: "showInList", label: "Show in list", requires: null, single: true, group: 2 },
  { type: "explain", label: "Explain", shortcut: "I", requires: null, single: true, group: 2 },
  { type: "excludeFromView", label: "Exclude from view", requires: null, single: false, group: 2 },
  { type: "addToCleanup", label: "Add to cleanup", shortcut: "Del", requires: "cleanup_queue_add", single: false, group: 3 },
];

const INFO = new Map(ENTRY_ACTIONS.map((a) => [a.type, a]));

/** UI-side effects the bus needs; implemented by the app shell. */
export interface UiActions {
  showInList(target: EntryTarget): void;
  explain(target: EntryTarget): void;
  exclude(target: EntryTarget): void;
  /** Reports a result or failure to the user (status bar / live region). */
  notify(message: string): void;
}

/** Dependencies of the bus. */
export interface CommandBusDeps {
  /** Backend command names this build implements. */
  capabilities: () => ReadonlySet<string>;
  ui: UiActions;
  /** Clipboard writer (injected so tests and non-secure contexts work). */
  writeClipboard: (text: string) => Promise<void>;
}

/** Routes entry commands to the backend or the UI. */
export class CommandBus {
  /** @param deps - Capability lookup, UI effects and clipboard. */
  constructor(private readonly deps: CommandBusDeps) {}

  /**
   * Whether `cmd` can run, with a user-facing reason when it cannot.
   *
   * @param cmd - The command.
   * @returns Availability.
   */
  availability(cmd: EntryCommand): Availability {
    const info = INFO.get(cmd.type);
    if (!info) return { enabled: false, reason: "Unknown action." };
    if (cmd.target.ids.length === 0) return { enabled: false, reason: "Nothing selected." };
    if (info.single && cmd.target.ids.length > 1) return { enabled: false, reason: "Works on one item at a time." };
    if (info.requires && !this.deps.capabilities().has(info.requires)) {
      return { enabled: false, reason: unavailableReason(cmd.type) };
    }
    return { enabled: true };
  }

  /**
   * Runs a command. Unavailable commands are rejected, never simulated.
   *
   * @param cmd - The command.
   */
  async dispatch(cmd: EntryCommand): Promise<void> {
    const a = this.availability(cmd);
    if (!a.enabled) {
      this.deps.ui.notify(a.reason);
      return;
    }
    const { target } = cmd;
    try {
      switch (cmd.type) {
        case "showInList":
          this.deps.ui.showInList(target);
          return;
        case "explain":
          this.deps.ui.explain(target);
          return;
        case "excludeFromView":
          this.deps.ui.exclude(target);
          return;
        case "copyPath": {
          const paths = await Promise.all(target.ids.map((id) => fetchEntryPath(target.volumeId, id)));
          await this.deps.writeClipboard(paths.join("\r\n"));
          this.deps.ui.notify(paths.length === 1 ? "Path copied." : `${paths.length} paths copied.`);
          return;
        }
        case "addToCleanup":
          // Never-tier refusals come back with reasons; reportAdd shows them.
          this.deps.ui.notify(applyAddResult(await addToQueue(target.volumeId, target.ids)));
          return;
        case "open":
        case "reveal":
        case "properties":
        case "openTerminal":
          await call<null>("entry_action", { action: cmd.type, volumeId: target.volumeId, ids: target.ids });
          return;
      }
    } catch (err) {
      this.deps.ui.notify(`${INFO.get(cmd.type)?.label ?? cmd.type} failed: ${errorMessage(err)}`);
    }
  }
}

function unavailableReason(type: EntryAction): string {
  switch (type) {
    case "addToCleanup":
      return "The cleanup queue is not part of this build yet.";
    case "copyPath":
      return "Path lookup needs the index, which is not connected yet.";
    default:
      return "Shell integration is not connected in this build yet.";
  }
}
