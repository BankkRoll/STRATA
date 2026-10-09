/**
 * Built-in Windows tools (SPEC §15.6). Each card shows what the tool does;
 * choosing it asks the backend for the exact command, shows that command
 * verbatim in a confirmation, and only then runs it. Captured output (DISM)
 * streams into a log region. Hibernation is guidance only and never runs.
 */
import { useState } from "react";
import { ConfirmDialog, LoadState, Unavailable, ViewFrame, useCapability, useLoad } from "../components/feature";
import { useFeatures } from "../features";
import { errorMessage } from "../lib/backend";
import { formatBytes, formatCount } from "../lib/format";
import type { ToolAction, ToolPrompt, ToolsStatus } from "../lib/tools";
import { useSettings } from "../store/settings";

interface ToolCard {
  key: string;
  title: string;
  blurb: string;
  action: ToolAction;
  /** Status line from `tools_status`. */
  status?: (s: ToolsStatus, units: "binary" | "si") => string | null;
}

const CARDS: readonly ToolCard[] = [
  {
    key: "bin",
    title: "Empty Recycle Bin",
    blurb: "Permanently removes everything in the Recycle Bin on every drive.",
    action: { kind: "empty_recycle_bin", drive: null },
    status: (s, units) => {
      const items = s.recycleBins.reduce((a, b) => a + b.items, 0);
      const bytes = s.recycleBins.reduce((a, b) => a + b.bytes, 0);
      return items === 0 ? "The Recycle Bin is empty." : `${formatCount(items)} items, ${formatBytes(bytes, { units })}`;
    },
  },
  { key: "cleanmgr", title: "Disk Cleanup", blurb: "Windows’ own cleaner for system files, update leftovers and logs.", action: { kind: "disk_cleanup", drive: null } },
  { key: "sense", title: "Storage Sense", blurb: "Opens Settings, where Windows can clean temporary files automatically.", action: { kind: "storage_sense_settings" } },
  {
    key: "dism",
    title: "Clean up the component store (DISM)",
    blurb: "Removes superseded Windows Update components from WinSxS. Runs as administrator; output is shown.",
    action: { kind: "dism_component_cleanup" },
  },
  {
    key: "protection",
    title: "System Protection",
    blurb: "Limit or delete restore points (shadow copies) per drive.",
    action: { kind: "system_protection" },
    status: (s, units) => (s.shadowStorageBytes === null ? null : `Shadow copies use ${formatBytes(s.shadowStorageBytes, { units })}.`),
  },
  {
    key: "hiber",
    title: "Hibernation file",
    blurb: "hiberfil.sys can be removed by turning hibernation off. Strata only explains how; it never runs this for you.",
    action: { kind: "hibernation_guidance" },
    status: (s, units) =>
      s.hibernation.enabled === null
        ? null
        : s.hibernation.enabled
          ? `Hibernation is on${s.hibernation.hiberfilBytes === null ? "" : `; hiberfil.sys uses ${formatBytes(s.hibernation.hiberfilBytes, { units })}`}.`
          : "Hibernation is off.",
  },
  {
    key: "compact",
    title: "CompactOS status",
    blurb: "Shows whether Windows system files are compressed. Changing it is an advanced, confirmed action in Windows itself.",
    action: { kind: "compact_os_status" },
    status: (s) => (s.compactOs === "unknown" ? null : s.compactOs === "compact" ? "System files are compressed." : "System files are not compressed."),
  },
];

/** Tools panel. */
export function ToolsView() {
  const features = useFeatures();
  const canPrepare = useCapability("tools_prepare", "tools_run");
  const hasStatus = useCapability("tools_status");
  const units = useSettings((s) => s.units);
  const [status] = useLoad(() => features.tools.fetchToolsStatus(), [features], hasStatus);
  const [prompt, setPrompt] = useState<ToolPrompt | null>(null);
  const [output, setOutput] = useState<{ title: string; lines: string[]; done: string | null } | null>(null);
  const [error, setError] = useState<string | null>(null);

  const prepare = (action: ToolAction) => {
    setError(null);
    features.tools.prepareTool(action).then(setPrompt, (e: unknown) => {
      setError(errorMessage(e));
    });
  };

  const run = (p: ToolPrompt) => {
    setPrompt(null);
    if (Date.now() > p.expiresMs) {
      setError("That confirmation expired. Choose the tool again.");
      return;
    }
    const lines: string[] = [];
    setOutput({ title: p.title, lines, done: null });
    features.tools
      .runTool(p.promptId, (l) => {
        lines.push(l.line);
        setOutput({ title: p.title, lines: [...lines], done: null });
      })
      .then(
        (r) => {
          const all = r.output ? r.output.split(/\r?\n/) : lines;
          setOutput({ title: p.title, lines: all, done: r.exitCode === null ? "Started." : r.exitCode === 0 ? "Finished successfully." : `Finished with exit code ${r.exitCode}.` });
        },
        (e: unknown) => {
          setOutput({ title: p.title, lines, done: `Failed: ${errorMessage(e)}` });
        },
      );
  };

  return (
    <ViewFrame title="Windows tools" lead="Windows’ own cleanup tools. You see the exact command before anything runs.">
      {!canPrepare && (
        <Unavailable feature="Running Windows tools" command="tools_prepare">
          The cards below describe each tool; launching them needs the engine.
        </Unavailable>
      )}
      {error && (
        <p className="banner banner--warn" role="alert">
          {error}
        </p>
      )}
      <ul className="cards">
        {CARDS.map((c) => {
          const line = status.state === "ready" && c.status ? c.status(status.data, units) : null;
          return (
            <li key={c.key} className="card">
              <h2 className="card__title">{c.title}</h2>
              <p>{c.blurb}</p>
              {line && <p className="detail__muted">{line}</p>}
              <button
                type="button"
                className="btn"
                disabled={!canPrepare}
                onClick={() => {
                  prepare(c.action);
                }}
              >
                {c.action.kind === "hibernation_guidance" ? "Show how" : c.action.kind === "compact_os_status" ? "Check status" : `${c.title}…`}
              </button>
            </li>
          );
        })}
      </ul>
      <p className="detail__muted">App uninstallers are on the Apps page, next to each app.</p>
      {output && (
        <section className="tool-output" aria-labelledby="out-h">
          <h2 id="out-h">{output.title}</h2>
          <pre className="log" tabIndex={0} aria-label={`${output.title} output`}>
            {output.lines.join("\n")}
          </pre>
          <p role="status">{output.done ?? "Running…"}</p>
        </section>
      )}
      {prompt && <ToolConfirm prompt={prompt} onRun={run} onCancel={() => { setPrompt(null); }} />}
      {hasStatus && status.state === "error" && (
        <LoadState load={status} feature="Tool status" command="tools_status">
          {() => null}
        </LoadState>
      )}
    </ViewFrame>
  );
}

/** Props for {@link ToolConfirm}. */
export interface ToolConfirmProps {
  prompt: ToolPrompt;
  onRun: (p: ToolPrompt) => void;
  onCancel: () => void;
}

/** Shows the exact command and asks before running; guidance-only prompts just close. */
export function ToolConfirm({ prompt, onRun, onCancel }: ToolConfirmProps) {
  const units = useSettings((s) => s.units);
  const guidance = prompt.launch === "guidance_only";
  return (
    <ConfirmDialog
      title={prompt.title}
      confirmLabel={guidance ? "Close" : prompt.launch === "elevated" ? "Run as administrator" : "Run"}
      danger={prompt.recycleBin !== null}
      onCancel={onCancel}
      onConfirm={() => {
        if (guidance) onCancel();
        else onRun(prompt);
      }}
    >
      <p>{prompt.description}</p>
      {prompt.recycleBin && (
        <p>
          This permanently removes {formatCount(prompt.recycleBin.items)} items ({formatBytes(prompt.recycleBin.bytes, { units })}).
        </p>
      )}
      <p className="detail__muted">{guidance ? "Run this yourself in an administrator terminal if you want to:" : "Exact command:"}</p>
      <pre className="command">
        <code>{prompt.commandLine}</code>
      </pre>
      {prompt.removedFlags.length > 0 && <p className="detail__muted">Silent flags removed so you see the uninstaller: {prompt.removedFlags.join(" ")}</p>}
    </ConfirmDialog>
  );
}
