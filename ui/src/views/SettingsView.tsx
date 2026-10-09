/**
 * Settings (SPEC §19): every persisted setting with its control, validated
 * as you type (mirror of the store's rules) and saved through
 * `settings_save` (whose issues win); rules tooling (built-in rules
 * read-only, user rules folder, reload, path tester); data clearing;
 * export/import; helper service mode; About (version, licenses, updates).
 */
import { useId, useMemo, useState, type ReactNode } from "react";
import { ConfirmDialog, LoadState, SafetyBadge, Unavailable, ViewFrame, useCapability, useLoad } from "../components/feature";
import { ExplanationView } from "../components/Explanation";
import { useFeatures } from "../features";
import { errorMessage, getAppInfo } from "../lib/backend";
import { formatCount } from "../lib/format";
import { categoryInfo } from "../lib/palette";
import { changedKeys, validateSettings, type ClearableData, type Explanation, type Settings, type SettingsIssue } from "../lib/settings";
import { useApp } from "../store/app";
import { applyAppearance } from "../store/prefs";
import { useSettings } from "../store/settings";

const GB = 2 ** 30;
const MB = 2 ** 20;

// -----------------------------------------------------------------------------
// Field primitives
// -----------------------------------------------------------------------------

type Section = keyof Settings;

interface FieldCtx {
  draft: Settings;
  issues: readonly SettingsIssue[];
  set: <S extends Section, K extends keyof Settings[S]>(section: S, key: K, value: Settings[S][K]) => void;
}

function issueFor(ctx: FieldCtx, key: string): string | null {
  return ctx.issues.find((i) => i.key === key)?.message ?? null;
}

function Toggle<S extends Section>({ ctx, section, field, label, hint }: { ctx: FieldCtx; section: S; field: keyof Settings[S] & string; label: string; hint?: string }) {
  const id = useId();
  const value = ctx.draft[section][field] as boolean;
  return (
    <div className="field field--toggle">
      <input
        id={id}
        type="checkbox"
        role="switch"
        checked={value}
        aria-describedby={hint ? `${id}-hint` : undefined}
        onChange={(e) => {
          ctx.set(section, field, e.target.checked as Settings[S][typeof field]);
        }}
      />
      <label htmlFor={id}>{label}</label>
      {hint && (
        <p id={`${id}-hint`} className="field__hint">
          {hint}
        </p>
      )}
    </div>
  );
}

function NumberField<S extends Section>({
  ctx,
  section,
  field,
  label,
  unit,
  scale = 1,
  step = 1,
  hint,
}: {
  ctx: FieldCtx;
  section: S;
  field: keyof Settings[S] & string;
  label: string;
  unit?: string;
  /** Stored value = shown value × scale (e.g. GB → bytes). */
  scale?: number;
  step?: number;
  hint?: string;
}) {
  const id = useId();
  const key = `${section}.${field}`;
  const err = issueFor(ctx, key);
  const stored = ctx.draft[section][field] as number;
  const [text, setText] = useState(String(stored / scale));
  const [prevStored, setPrevStored] = useState(stored);
  // Revert and import change the value from outside; re-derive the text then,
  // but never while the user's own (possibly half-typed) text already matches.
  if (!Object.is(prevStored, stored)) {
    setPrevStored(stored);
    if (Number.isFinite(stored) && Number(text) * scale !== stored) setText(String(stored / scale));
  }
  return (
    <div className="field">
      <label htmlFor={id}>{label}</label>
      <span className="field__input">
        <input
          id={id}
          type="number"
          className="input input--num"
          step={step}
          value={text}
          aria-invalid={err !== null}
          aria-describedby={[err ? `${id}-err` : "", hint ? `${id}-hint` : ""].filter(Boolean).join(" ") || undefined}
          onChange={(e) => {
            setText(e.target.value);
            const n = e.target.value.trim() === "" ? Number.NaN : Number(e.target.value);
            ctx.set(section, field, (Number.isFinite(n) ? Math.round(n * scale) : Number.NaN) as Settings[S][typeof field]);
          }}
        />
        {unit && <span className="field__unit">{unit}</span>}
      </span>
      {hint && (
        <p id={`${id}-hint`} className="field__hint">
          {hint}
        </p>
      )}
      {err && (
        <p id={`${id}-err`} className="field__error" role="alert">
          {err}
        </p>
      )}
    </div>
  );
}

function Choice<S extends Section>({
  ctx,
  section,
  field,
  label,
  options,
}: {
  ctx: FieldCtx;
  section: S;
  field: keyof Settings[S] & string;
  label: string;
  options: readonly [string, string][];
}) {
  const name = useId();
  const value = ctx.draft[section][field] as string;
  return (
    <fieldset className="field field--choice">
      <legend>{label}</legend>
      {options.map(([v, text]) => (
        <label key={v}>
          <input
            type="radio"
            name={name}
            checked={value === v}
            onChange={() => {
              ctx.set(section, field, v as Settings[S][typeof field]);
            }}
          />
          {text}
        </label>
      ))}
    </fieldset>
  );
}

function Group({ id, title, children }: { id: string; title: string; children: ReactNode }) {
  return (
    <section id={`set-${id}`} className="settings__group" aria-labelledby={`set-${id}-h`}>
      <h2 id={`set-${id}-h`} className="section-title">
        {title}
      </h2>
      {children}
    </section>
  );
}

const GROUPS: readonly [string, string][] = [
  ["scan", "Scanning"],
  ["live", "Live updates"],
  ["activity", "Activity tracking"],
  ["helper", "Helper"],
  ["cleanup", "Cleanup"],
  ["history", "History"],
  ["appearance", "Appearance"],
  ["rules", "Rules"],
  ["data", "Data"],
  ["startup", "Startup and tray"],
  ["about", "About"],
];

// -----------------------------------------------------------------------------
// View
// -----------------------------------------------------------------------------

/** Settings view. */
export function SettingsView() {
  const features = useFeatures();
  const has = useCapability("settings_load", "settings_save");
  const [load, reload] = useLoad(() => features.settings.loadSettings(), [features], has);
  return (
    <ViewFrame title="Settings">
      <nav className="settings__toc" aria-label="Settings sections">
        <ul>
          {GROUPS.map(([id, t]) => (
            <li key={id}>
              <a href={`#set-${id}`}>{t}</a>
            </li>
          ))}
        </ul>
      </nav>
      {!has ? (
        <>
          <Unavailable feature="Saving settings" command="settings_load">
            Theme, units and patterns still apply for this session from the top bar.
          </Unavailable>
          <AboutGroup />
        </>
      ) : (
        <LoadState load={load} feature="Settings" command="settings_load" onRetry={reload}>
          {(s) => <SettingsForm initial={s} />}
        </LoadState>
      )}
    </ViewFrame>
  );
}

/** Props for {@link SettingsForm}. */
export interface SettingsFormProps {
  initial: Settings;
}

/** The editable settings form with save / revert. */
export function SettingsForm({ initial }: SettingsFormProps) {
  const features = useFeatures();
  const [saved, setSaved] = useState(initial);
  const [draft, setDraft] = useState(initial);
  const [serverIssues, setServerIssues] = useState<SettingsIssue[]>([]);
  const [status, setStatus] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const localIssues = useMemo(() => validateSettings(draft), [draft]);
  const issues = serverIssues.length > 0 ? serverIssues : localIssues;
  const dirty = changedKeys(saved, draft);
  const ctx: FieldCtx = {
    draft,
    issues,
    set(section, key, value) {
      setServerIssues([]);
      setDraft((d) => ({ ...d, [section]: { ...d[section], [key]: value } }));
    },
  };

  const save = () => {
    setSaving(true);
    features.settings.saveSettings(draft).then(
      (r) => {
        setSaving(false);
        if (r.issues.length > 0) {
          setServerIssues(r.issues);
          setStatus("Not saved: fix the highlighted settings.");
          return;
        }
        setSaved(r.settings);
        setDraft(r.settings);
        applyAppearance(r.settings);
        setStatus("Settings saved.");
      },
      (e: unknown) => {
        setSaving(false);
        setStatus(`Not saved: ${errorMessage(e)}`);
      },
    );
  };

  const imported = (s: Settings, problems: SettingsIssue[]) => {
    if (problems.length > 0) {
      setServerIssues(problems);
      setStatus("Import refused: the file has invalid settings. Nothing was changed.");
      return;
    }
    setSaved(s);
    setDraft(s);
    applyAppearance(s);
    setStatus("Settings imported.");
  };

  return (
    <form
      className="settings"
      onSubmit={(e) => {
        e.preventDefault();
        if (issues.length === 0 && dirty.length > 0) save();
      }}
    >
      <div className="settings__bar" role="region" aria-label="Save settings">
        <span role="status">{status ?? (dirty.length > 0 ? `${dirty.length} unsaved ${dirty.length === 1 ? "change" : "changes"}` : "All changes saved")}</span>
        {issues.length > 0 && <span className="warn-text">{formatCount(issues.length)} to fix</span>}
        <button
          type="button"
          className="btn"
          disabled={dirty.length === 0}
          onClick={() => {
            setDraft(saved);
            setServerIssues([]);
            setStatus(null);
          }}
        >
          Revert
        </button>
        <button type="submit" className="btn btn--primary" disabled={dirty.length === 0 || issues.length > 0 || saving}>
          {saving ? "Saving…" : "Save"}
        </button>
      </div>

      <Group id="scan" title="Scanning">
        <Choice
          ctx={ctx}
          section="scan"
          field="default_size_mode"
          label="Default size"
          options={[
            ["allocated", "Size on disk (allocated)"],
            ["logical", "File size (logical)"],
          ]}
        />
        <GlobsField ctx={ctx} />
        <Toggle ctx={ctx} section="scan" field="include_network_drives" label="Include network drives" />
        <Toggle ctx={ctx} section="scan" field="auto_scan_on_launch" label="Scan the system drive when Strata starts" />
        <Toggle ctx={ctx} section="scan" field="auto_scan_removable" label="Scan removable drives when plugged in" />
        <NumberField ctx={ctx} section="scan" field="walker_concurrency" label="Fallback scanner threads" hint="0 picks automatically. Lower it for slow network drives." />
        <Toggle ctx={ctx} section="scan" field="show_follow_policy" label="Show the link policy notice" hint="Strata never follows junctions, symbolic links or mount points while scanning, so nothing is counted twice." />
      </Group>

      <Group id="live" title="Live updates">
        <Toggle ctx={ctx} section="live" field="usn_enabled" label="Keep the map current with the change journal" />
        <NumberField ctx={ctx} section="live" field="update_tick_ms" label="Update interval" unit="ms" step={100} />
        <Toggle ctx={ctx} section="live" field="auto_rescan_on_journal_loss" label="Rescan automatically when changes were missed" />
      </Group>

      <Group id="activity" title="Activity tracking (advanced)">
        <Toggle ctx={ctx} section="activity" field="enabled" label="Track which programs write to disk" hint="Off by default. Uses ETW through the helper; data stays on this PC." />
        <NumberField ctx={ctx} section="activity" field="retention_days" label="Keep activity for" unit="days" />
        <NumberField ctx={ctx} section="activity" field="cpu_cap_percent" label="CPU cap" unit="%" step={0.1} />
      </Group>

      <Group id="helper" title="Helper">
        <Choice
          ctx={ctx}
          section="helper"
          field="mode"
          label="How the elevated helper runs"
          options={[
            ["on_demand", "On demand (UAC prompt when needed)"],
            ["service", "As a Windows service (no prompts)"],
          ]}
        />
        <HelperService />
      </Group>

      <Group id="cleanup" title="Cleanup">
        <Choice
          ctx={ctx}
          section="cleanup"
          field="default_method"
          label="Default method"
          options={[
            ["recycle_bin", "Move to Recycle Bin"],
            ["permanent", "Delete permanently"],
          ]}
        />
        <NumberField ctx={ctx} section="cleanup" field="large_delete_confirm_bytes" label="Ask again for permanent deletes over" unit="GB" scale={GB} step={0.5} />
        <NumberField ctx={ctx} section="cleanup" field="stale_node_modules_days" label="node_modules count as stale after" unit="days" />
        <NumberField ctx={ctx} section="cleanup" field="stale_installers_days" label="Installers count as old after" unit="days" />
        <NumberField ctx={ctx} section="cleanup" field="duplicates_min_bytes" label="Ignore duplicates smaller than" unit="MB" scale={MB} step={0.5} />
      </Group>

      <Group id="history" title="History">
        <NumberField ctx={ctx} section="history" field="snapshot_interval_hours" label="Snapshot every" unit="hours" />
        <NumberField ctx={ctx} section="history" field="retention_days" label="Keep snapshots for" unit="days" />
        <NumberField ctx={ctx} section="history" field="thin_after_days" label="Keep one per week after" unit="days" />
        <NumberField ctx={ctx} section="history" field="min_dir_bytes" label="Record folders larger than" unit="MB" scale={MB} />
      </Group>

      <Group id="appearance" title="Appearance">
        <Choice
          ctx={ctx}
          section="appearance"
          field="theme"
          label="Theme"
          options={[
            ["system", "Follow Windows"],
            ["light", "Light"],
            ["dark", "Dark"],
          ]}
        />
        <Choice
          ctx={ctx}
          section="appearance"
          field="color_mode"
          label="Default colors"
          options={[
            ["category", "Category"],
            ["file_type", "File type"],
            ["age", "Age"],
            ["app", "Owning app"],
            ["safety", "Safety tier"],
          ]}
        />
        <Choice
          ctx={ctx}
          section="appearance"
          field="treemap_style"
          label="Treemap style"
          options={[
            ["flat", "Flat"],
            ["cushion", "Cushion"],
          ]}
        />
        <Choice
          ctx={ctx}
          section="appearance"
          field="units"
          label="Units"
          options={[
            ["binary", "Binary, shown as KB/MB/GB (like Explorer)"],
            ["decimal", "SI (1 kB = 1000 bytes)"],
          ]}
        />
        <Toggle ctx={ctx} section="appearance" field="compact_density" label="Compact rows" />
        <PatternsToggle />
      </Group>

      <Group id="rules" title="Rules">
        <Toggle ctx={ctx} section="rules" field="user_rules_enabled" label="Load my own rule packs" />
        <RulesDirField ctx={ctx} />
        <RulesTools />
      </Group>

      <Group id="data" title="Data">
        <DataTools onImported={imported} />
        <Toggle ctx={ctx} section="privacy" field="crash_reports_opt_in" label="Send crash reports" hint="Off by default. Never includes paths, file names or scan data." />
      </Group>

      <Group id="startup" title="Startup and tray">
        <Toggle ctx={ctx} section="startup" field="launch_at_login" label="Start with Windows" />
        <Toggle ctx={ctx} section="startup" field="start_minimized_to_tray" label="Start minimized to the tray" />
        <Toggle ctx={ctx} section="tray" field="enabled" label="Show a tray icon (free space at a glance, quick scan)" />
        <Toggle ctx={ctx} section="tray" field="low_space_notification" label="Notify when free space drops low" />
        <NumberField ctx={ctx} section="tray" field="low_space_threshold_bytes" label="Low space means under" unit="GB" scale={GB} />
      </Group>

      <AboutGroup ctx={ctx} />
    </form>
  );
}

// -----------------------------------------------------------------------------
// Composite fields
// -----------------------------------------------------------------------------

function GlobsField({ ctx }: { ctx: FieldCtx }) {
  const id = useId();
  const err = issueFor(ctx, "scan.exclude_globs");
  const [text, setText] = useState(ctx.draft.scan.exclude_globs.join("\n"));
  return (
    <div className="field">
      <label htmlFor={id}>Exclude from scans (one pattern per line)</label>
      <textarea
        id={id}
        className="input textarea"
        rows={3}
        value={text}
        aria-invalid={err !== null}
        aria-describedby={err ? `${id}-err` : `${id}-hint`}
        onChange={(e) => {
          setText(e.target.value);
          ctx.set(
            "scan",
            "exclude_globs",
            e.target.value.split(/\r?\n/).filter((l, i, a) => !(l === "" && i === a.length - 1)),
          );
        }}
      />
      <p id={`${id}-hint`} className="field__hint">
        Example: <code>D:\Backups\**</code>
      </p>
      {err && (
        <p id={`${id}-err`} className="field__error" role="alert">
          {err}
        </p>
      )}
    </div>
  );
}

function RulesDirField({ ctx }: { ctx: FieldCtx }) {
  const id = useId();
  const err = issueFor(ctx, "rules.user_rules_dir");
  return (
    <div className="field">
      <label htmlFor={id}>User rules folder</label>
      <input
        id={id}
        type="text"
        className="input"
        value={ctx.draft.rules.user_rules_dir ?? ""}
        placeholder="Default (in your app data folder)"
        aria-invalid={err !== null}
        onChange={(e) => {
          ctx.set("rules", "user_rules_dir", e.target.value === "" ? null : e.target.value);
        }}
      />
      {err && (
        <p className="field__error" role="alert">
          {err}
        </p>
      )}
    </div>
  );
}

function PatternsToggle() {
  const id = useId();
  const patterns = useSettings((s) => s.patterns);
  const setPatterns = useSettings((s) => s.setPatterns);
  return (
    <div className="field field--toggle">
      <input
        id={id}
        type="checkbox"
        role="switch"
        checked={patterns}
        onChange={(e) => {
          setPatterns(e.target.checked);
        }}
      />
      <label htmlFor={id}>Category patterns for color-blind use (applies now)</label>
    </div>
  );
}

function HelperService() {
  const features = useFeatures();
  const has = useCapability("helper_service_status");
  const canChange = useCapability("helper_service_install", "helper_service_uninstall");
  const [load, reload] = useLoad(() => features.settings.fetchHelperService(), [features], has);
  const [busy, setBusy] = useState(false);
  if (!has) return <p className="detail__muted">Installing the service isn’t available in this build.</p>;
  return (
    <LoadState load={load} feature="Helper service" command="helper_service_status" onRetry={reload}>
      {(s) => (
        <div className="toolbar">
          <span>{s.installed ? (s.running ? "Service installed and running." : "Service installed, not running.") : "Service not installed."}</span>
          <button
            type="button"
            className="btn"
            disabled={!canChange || busy}
            onClick={() => {
              setBusy(true);
              (s.installed ? features.settings.uninstallHelperService() : features.settings.installHelperService()).then(
                () => {
                  setBusy(false);
                  reload();
                },
                (e: unknown) => {
                  setBusy(false);
                  useApp.getState().notify(errorMessage(e));
                },
              );
            }}
          >
            {s.installed ? "Uninstall service (admin)…" : "Install service (admin)…"}
          </button>
        </div>
      )}
    </LoadState>
  );
}

function RulesTools() {
  const features = useFeatures();
  const canList = useCapability("rules_list");
  const canOpen = useCapability("rules_open_folder");
  const canReload = useCapability("rules_reload");
  const canExplain = useCapability("rules_explain");
  const [showRules, setShowRules] = useState(false);
  const [filter, setFilter] = useState("");
  const [rules] = useLoad(() => features.settings.fetchRules(), [features], canList && showRules);
  const [reloadMsg, setReloadMsg] = useState<string | null>(null);
  const [path, setPath] = useState("");
  const [explain, setExplain] = useState<{ result: Explanation } | { error: string } | null>(null);
  const testerId = useId();
  return (
    <>
      <div className="toolbar">
        <button
          type="button"
          className="btn"
          disabled={!canOpen}
          onClick={() => {
            void features.settings.openRulesFolder().catch((e: unknown) => {
              useApp.getState().notify(errorMessage(e));
            });
          }}
        >
          Open user rules folder
        </button>
        <button
          type="button"
          className="btn"
          disabled={!canReload}
          onClick={() => {
            features.settings.reloadRules().then(
              (r) => {
                setReloadMsg(
                  `Loaded ${formatCount(r.builtin)} built-in and ${formatCount(r.user)} user rules.${r.problems.length > 0 ? ` Problems: ${r.problems.map((p) => `${p.file}: ${p.message}`).join("; ")}` : ""}`,
                );
              },
              (e: unknown) => {
                setReloadMsg(errorMessage(e));
              },
            );
          }}
        >
          Reload rules
        </button>
        <button
          type="button"
          className="btn"
          disabled={!canList}
          aria-expanded={showRules}
          onClick={() => {
            setShowRules(!showRules);
          }}
        >
          {showRules ? "Hide built-in rules" : "View built-in rules"}
        </button>
      </div>
      {reloadMsg && <p role="status">{reloadMsg}</p>}
      {showRules && (
        <LoadState load={rules} feature="Rules" command="rules_list">
          {(list) => {
            const q = filter.toLowerCase();
            const shown = list.filter((r) => q === "" || r.id.toLowerCase().includes(q) || r.name.toLowerCase().includes(q));
            return (
              <>
                <label>
                  Filter rules{" "}
                  <input
                    type="search"
                    className="input"
                    value={filter}
                    onChange={(e) => {
                      setFilter(e.target.value);
                    }}
                  />
                </label>
                <div className="scroll-box" tabIndex={0} role="region" aria-label="Rules (read-only)">
                  <table className="table">
                    <caption className="visually-hidden">Rules, read-only</caption>
                    <thead>
                      <tr>
                        <th scope="col">Rule</th>
                        <th scope="col">Category</th>
                        <th scope="col">Safety</th>
                        <th scope="col">Source</th>
                      </tr>
                    </thead>
                    <tbody>
                      {shown.map((r) => (
                        <tr key={`${r.source}-${r.id}`}>
                          <th scope="row">
                            <div className="cell-name">{r.name}</div>
                            <div className="cell-path">{r.id}</div>
                            <div className="detail__muted">{r.explain}</div>
                          </th>
                          <td>{categoryInfo(r.category).label}</td>
                          <td>
                            <SafetyBadge tier={r.safety} />
                          </td>
                          <td>
                            {r.source === "builtin" ? "Built-in" : "Yours"}
                            {r.overriddenBy && <div className="detail__muted">overridden by {r.overriddenBy}</div>}
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </>
            );
          }}
        </LoadState>
      )}
      <div className="field">
        <label htmlFor={testerId}>Why is this classified as…? Test a path</label>
        <span className="field__input">
          <input
            id={testerId}
            type="text"
            className="input input--wide"
            value={path}
            placeholder="C:\Users\me\AppData\Local\Temp"
            onChange={(e) => {
              setPath(e.target.value);
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                document.getElementById(`${testerId}-go`)?.click();
              }
            }}
          />
          <button
            id={`${testerId}-go`}
            type="button"
            className="btn"
            disabled={!canExplain || path.trim() === ""}
            onClick={() => {
              features.settings.explainPath(path.trim()).then(
                (result) => {
                  setExplain({ result });
                },
                (e: unknown) => {
                  setExplain({ error: errorMessage(e) });
                },
              );
            }}
          >
            Explain
          </button>
        </span>
      </div>
      {explain && ("error" in explain ? <p className="warn-text">{explain.error}</p> : <ExplanationView e={explain.result} />)}
    </>
  );
}

const CLEAR_TEXT: Readonly<Record<ClearableData, [string, string]>> = {
  history: ["Clear history", "Deletes every snapshot. Usage charts and “what changed” start over. Your files are not touched."],
  activity: ["Clear activity data", "Deletes all recorded program activity. Your files are not touched."],
  caches: ["Clear Strata’s caches", "Deletes the scan index cache and duplicate hashes. The next launch rescans. Your files are not touched."],
};

function DataTools({ onImported }: { onImported: (s: Settings, issues: SettingsIssue[]) => void }) {
  const features = useFeatures();
  const canClear = useCapability("data_clear");
  const canExport = useCapability("settings_export");
  const canImport = useCapability("settings_import");
  const [confirm, setConfirm] = useState<ClearableData | null>(null);
  const importId = useId();
  return (
    <>
      <div className="toolbar">
        {(Object.keys(CLEAR_TEXT) as ClearableData[]).map((k) => (
          <button
            key={k}
            type="button"
            className="btn"
            disabled={!canClear}
            onClick={() => {
              setConfirm(k);
            }}
          >
            {CLEAR_TEXT[k][0]}…
          </button>
        ))}
      </div>
      <div className="toolbar">
        <button
          type="button"
          className="btn"
          disabled={!canExport}
          onClick={() => {
            features.settings.exportSettings().then(
              (json) => {
                const url = URL.createObjectURL(new Blob([json], { type: "application/json" }));
                const a = document.createElement("a");
                a.href = url;
                a.download = "strata-settings.json";
                a.click();
                URL.revokeObjectURL(url);
              },
              (e: unknown) => {
                useApp.getState().notify(errorMessage(e));
              },
            );
          }}
        >
          Export settings
        </button>
        <label htmlFor={importId} className={`btn${canImport ? "" : " btn--disabled"}`}>
          Import settings…
        </label>
        <input
          id={importId}
          type="file"
          accept=".json,application/json"
          className="visually-hidden"
          disabled={!canImport}
          onChange={(e) => {
            const file = e.target.files?.[0];
            e.target.value = "";
            if (!file) return;
            void file
              .text()
              .then((text) => features.settings.importSettings(text))
              .then(
                (r) => {
                  onImported(r.settings, r.issues);
                },
                (err: unknown) => {
                  useApp.getState().notify(`Import failed: ${errorMessage(err)}`);
                },
              );
          }}
        />
      </div>
      {confirm && (
        <ConfirmDialog
          title={`${CLEAR_TEXT[confirm][0]}?`}
          confirmLabel={CLEAR_TEXT[confirm][0]}
          danger
          onCancel={() => {
            setConfirm(null);
          }}
          onConfirm={() => {
            const what = confirm;
            setConfirm(null);
            features.settings.clearData(what).then(
              () => {
                useApp.getState().notify(`${CLEAR_TEXT[what][0]}: done.`);
              },
              (e: unknown) => {
                useApp.getState().notify(errorMessage(e));
              },
            );
          }}
        >
          <p>{CLEAR_TEXT[confirm][1]}</p>
        </ConfirmDialog>
      )}
    </>
  );
}

function AboutGroup({ ctx }: { ctx?: FieldCtx }) {
  const features = useFeatures();
  const canLicenses = useCapability("about_licenses");
  const canUpdate = useCapability("updates_check");
  const [info] = useLoad(() => getAppInfo(), []);
  const [showLicenses, setShowLicenses] = useState(false);
  const [licenses] = useLoad(() => features.settings.fetchLicenses(), [features], canLicenses && showLicenses);
  const [update, setUpdate] = useState<string | null>(null);
  return (
    <Group id="about" title="About">
      <p>
        Strata {info.state === "ready" ? info.data.version : ""} · MIT licensed · no telemetry unless you opt in.
      </p>
      {ctx && (
        <>
          <Choice
            ctx={ctx}
            section="updates"
            field="channel"
            label="Update channel"
            options={[
              ["stable", "Stable"],
              ["beta", "Beta"],
            ]}
          />
          <Toggle ctx={ctx} section="updates" field="auto_download" label="Download updates in the background (applied on restart)" />
        </>
      )}
      <div className="toolbar">
        <button
          type="button"
          className="btn"
          disabled={!canUpdate}
          onClick={() => {
            features.settings.checkForUpdates().then(
              (u) => {
                setUpdate(u.available && u.latest ? `Version ${u.latest} is available on the ${u.channel} channel.` : `You’re up to date (${u.current}).`);
              },
              (e: unknown) => {
                setUpdate(errorMessage(e));
              },
            );
          }}
        >
          Check for updates
        </button>
        <button
          type="button"
          className="btn"
          disabled={!canLicenses}
          aria-expanded={showLicenses}
          onClick={() => {
            setShowLicenses(!showLicenses);
          }}
        >
          Third-party licenses
        </button>
      </div>
      {update && <p role="status">{update}</p>}
      {showLicenses && (
        <LoadState load={licenses} feature="Licenses" command="about_licenses">
          {(list) => (
            <div className="scroll-box" tabIndex={0} role="region" aria-label="Third-party licenses">
              <table className="table">
                <caption className="visually-hidden">Third-party components</caption>
                <thead>
                  <tr>
                    <th scope="col">Component</th>
                    <th scope="col">Version</th>
                    <th scope="col">License</th>
                  </tr>
                </thead>
                <tbody>
                  {list.map((l) => (
                    <tr key={`${l.ecosystem}-${l.name}-${l.version}`}>
                      <th scope="row">{l.name}</th>
                      <td>{l.version}</td>
                      <td>{l.license}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </LoadState>
      )}
    </Group>
  );
}
