/**
 * Built-in Windows tool actions.
 *
 * Two-step by design: `tools_prepare` returns the exact command and a
 * description the user reads (`strata_clean::tools::CommandSpec`), and only
 * `tools_run` with that prompt's id executes it. The UI never builds command
 * lines itself; uninstallers are referenced by catalog app id, never by a
 * command string from the UI.
 */
import { call } from "./backend";
import { callWithChannel } from "./bridge";

/** A built-in tool (`strata_clean::tools::ToolAction`, plus emptying the bin). */
export type ToolAction =
  | { kind: "empty_recycle_bin"; drive: string | null }
  | { kind: "disk_cleanup"; drive: string | null }
  | { kind: "storage_sense_settings" }
  | { kind: "dism_component_cleanup" }
  | { kind: "system_protection" }
  | { kind: "hibernation_guidance" }
  | { kind: "uninstall"; appId: string }
  | { kind: "compact_os_status" };

/** How a tool starts (`tools::Launch`). */
export type ToolLaunch = "process" | "shell_open" | "elevated" | "guidance_only";

/** The exact command shown before the user confirms (`tools::CommandSpec`). */
export interface ToolPrompt {
  /** Handle for `tools_run`; void after `expiresMs`. */
  promptId: number;
  title: string;
  description: string;
  /** The exact command line (or URI), shown verbatim. */
  commandLine: string;
  launch: ToolLaunch;
  /** Output is captured and shown when it finishes. */
  capturesOutput: boolean;
  /** Silent-install flags stripped from an uninstall string. */
  removedFlags: string[];
  expiresMs: number;
  /** For `empty_recycle_bin`: the item count and size the consent covers. */
  recycleBin: { items: number; bytes: number } | null;
}

/** A line of captured output. */
export interface ToolOutputLine {
  stream: "stdout" | "stderr";
  line: string;
}

/** Result of running a tool. */
export interface ToolRunResult {
  /** Process exit code, or `null` for Shell launches the app does not wait on. */
  exitCode: number | null;
  /** Complete captured output ("" when not captured). */
  output: string;
}

/** Read-only status the tools panel shows next to each tool. */
export interface ToolsStatus {
  /** Recycle Bin contents per drive. */
  recycleBins: { drive: string; items: number; bytes: number }[];
  /** `compact /compactos:query`. */
  compactOs: "compact" | "not_compact" | "unknown";
  /** Hibernation: whether it is on and the size of `hiberfil.sys`. */
  hibernation: { enabled: boolean | null; hiberfilBytes: number | null };
  /** Shadow-copy storage in use, or `null` when unknown (needs elevation). */
  shadowStorageBytes: number | null;
}

/** Reads tool status (`tools_status`). */
export function fetchToolsStatus(): Promise<ToolsStatus> {
  return call<ToolsStatus>("tools_status");
}

/** Builds the command and consent prompt for an action (`tools_prepare`). Nothing runs. */
export function prepareTool(action: ToolAction): Promise<ToolPrompt> {
  return call<ToolPrompt>("tools_prepare", { action });
}

/**
 * Runs a prepared tool (`tools_run`) after the user confirmed the prompt,
 * streaming captured output.
 *
 * @param promptId - From {@link prepareTool}.
 * @param onOutput - Output lines as they arrive.
 * @returns Exit code and full output.
 */
export function runTool(promptId: number, onOutput: (line: ToolOutputLine) => void): Promise<ToolRunResult> {
  return callWithChannel<ToolRunResult, ToolOutputLine>("tools_run", "onOutput", { promptId }, onOutput);
}
