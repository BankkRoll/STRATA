/**
 * Settings: a searchable page of every persisted setting, driven by the
 * settings registry (`settings/registry.ts`), plus rules tooling, helper and
 * elevation, updates, data and privacy, and About.
 *
 * - Changes save automatically once valid (`settings_save`); invalid values
 *   stay local with their message and are never sent. The backend validates
 *   again and its issues win.
 * - Every setting shows its default, a modified marker and a reset button.
 * - Search matches titles, descriptions, keys, categories and keywords, and
 *   highlights the matches; `@modified` lists changed settings.
 * - The JSON view shows the effective settings read-only, with copy, export
 *   and import (`settings_export` / `settings_import`).
 */
import { useEffect, useId, useMemo, useRef, useState, type ReactNode } from "react";
import { LoadState, useCapability, useLoad, useUnavailableReason } from "../components/feature";
import { Icon } from "../components/icons";
import { useFeatures } from "../features";
import { errorMessage } from "../lib/backend";
import { formatCount } from "../lib/format";
import type { Settings, SettingsIssue } from "../lib/settings";
import { Highlight, SettingRow } from "../settings/controls";
import { AboutPanels, DataPanels, HelperPanels, Panel, RulesPanels, UpdateCheck, missingReason } from "../settings/panels";
import {
  SETTINGS,
  SETTINGS_CATEGORIES,
  UNLISTED_KEYS,
  getSetting,
  isModified,
  matchesQuery,
  parseQuery,
  sameValue,
  validateAll,
  withSetting,
  type CategoryId,
  type SettingDef,
  type SettingKey,
  type SettingsQuery,
} from "../settings/registry";
import { useLayout } from "../shell/layout";
import { applyAppearance } from "../store/prefs";
import "../settings/settings.css";

const SAVE_DELAY_MS = 500;

/** Search terms for the non-setting panels of each category. */
const PANEL_TERMS: Partial<Record<CategoryId, string>> = {
  rules: "rule packs built-in read-only reload open folder why is this classified test path explain precedence",
  helper: "status elevation fast scan standard scan service install uninstall administrator uac",
  updates: "check for updates version",
  privacy: "clear history activity caches data report an issue bug github telemetry privacy",
  about: "version licenses third-party github documentation links mit",
};

// -----------------------------------------------------------------------------
// Save state
// -----------------------------------------------------------------------------

type SaveState = { kind: "idle" } | { kind: "saving" } | { kind: "saved" } | { kind: "failed"; message: string };

interface Editor {
  draft: Settings;
  /** Last settings the store confirmed. */
  saved: Settings;
  /** Messages by key: the backend's for unchanged values, else the registry's. */
  issues: Map<SettingKey, string>;
  save: SaveState;
  /** Bumps when values change from outside the editors (reset, import), remounting them. */
  revision: number;
  set: (key: SettingKey, value: unknown) => void;
  reset: (key: SettingKey) => void;
  replace: (s: Settings) => void;
}

/**
 * Draft, validation and debounced auto-save.
 *
 * @param initial - Settings as loaded.
 * @param canSave - `settings_save` exists.
 */
function useSettingsEditor(initial: Settings | null, canSave: boolean): Editor | null {
  const features = useFeatures();
  const [saved, setSaved] = useState(initial);
  const [draft, setDraft] = useState(initial);
  const [serverIssues, setServerIssues] = useState<SettingsIssue[]>([]);
  const [save, setSave] = useState<SaveState>({ kind: "idle" });
  const [revision, setRevision] = useState(0);
  const latest = useRef(draft);
  useEffect(() => {
    latest.current = draft;
  }, [draft]);

  const local = useMemo(() => (draft ? validateAll(draft) : new Map<SettingKey, string>()), [draft]);
  const dirty = draft !== null && saved !== null && !sameValue(draft, saved);

  useEffect(() => {
    if (!draft || !dirty || local.size > 0 || !canSave || serverIssues.length > 0) return;
    const t = setTimeout(() => {
      setSave({ kind: "saving" });
      features.settings.saveSettings(draft).then(
        (r) => {
          if (r.issues.length > 0) {
            setServerIssues(r.issues);
            setSave({ kind: "failed", message: `${formatCount(r.issues.length)} ${r.issues.length === 1 ? "setting was" : "settings were"} refused` });
            return;
          }
          setSaved(r.settings);
          // The store may normalize values; adopt them unless the user kept typing.
          if (sameValue(latest.current, draft) && !sameValue(r.settings, draft)) {
            setDraft(r.settings);
            setRevision((n) => n + 1);
          }
          applyAppearance(r.settings);
          setSave({ kind: "saved" });
        },
        (e: unknown) => {
          setSave({ kind: "failed", message: errorMessage(e) });
        },
      );
    }, SAVE_DELAY_MS);
    return () => {
      clearTimeout(t);
    };
  }, [draft, dirty, local, canSave, serverIssues, features]);

  if (!draft || !saved) return null;
  const issues = new Map(local);
  for (const i of serverIssues) if (!issues.has(i.key as SettingKey)) issues.set(i.key as SettingKey, i.message);
  return {
    draft,
    saved,
    issues,
    save,
    revision,
    set(key, value) {
      setServerIssues([]);
      setDraft((d) => (d ? withSetting(d, key, value) : d));
    },
    reset(key) {
      const def = SETTINGS.find((s) => s.key === key);
      if (!def) return;
      setServerIssues([]);
      setDraft((d) => (d ? withSetting(d, key, def.default) : d));
      setRevision((n) => n + 1);
    },
    replace(s) {
      setServerIssues([]);
      setSaved(s);
      setDraft(s);
      setRevision((n) => n + 1);
      applyAppearance(s);
    },
  };
}

// -----------------------------------------------------------------------------
// JSON view
// -----------------------------------------------------------------------------

function effectiveJson(s: Settings): string {
  const out: Record<string, Record<string, unknown>> = {};
  for (const [section, fields] of Object.entries(s as unknown as Record<string, Record<string, unknown>>)) {
    for (const [field, v] of Object.entries(fields)) {
      if (UNLISTED_KEYS.includes(`${section}.${field}`)) continue;
      (out[section] ??= {})[field] = v;
    }
  }
  return JSON.stringify(out, null, 2);
}

/** Tints JSON tokens: keys, strings, numbers and literals. */
function JsonLine({ line }: { line: string }) {
  const parts: ReactNode[] = [];
  const re = /("(?:[^"\\]|\\.)*")(\s*:)?|\b(-?\d+(?:\.\d+)?(?:e[+-]?\d+)?)\b|\b(true|false|null)\b/gi;
  let last = 0;
  let m: RegExpExecArray | null;
  let k = 0;
  while ((m = re.exec(line)) !== null) {
    if (m.index > last) parts.push(line.slice(last, m.index));
    if (m[1] !== undefined) {
      parts.push(
        <span key={k++} className={m[2] ? "j-key" : "j-str"}>
          {m[1]}
        </span>,
      );
      if (m[2]) parts.push(m[2]);
    } else if (m[3] !== undefined) {
      parts.push(
        <span key={k++} className="j-num">
          {m[3]}
        </span>,
      );
    } else {
      parts.push(
        <span key={k++} className="j-lit">
          {m[4]}
        </span>,
      );
    }
    last = re.lastIndex;
  }
  parts.push(line.slice(last));
  return <>{parts}</>;
}

function JsonView({ settings, unsaved, onImported }: { settings: Settings; unsaved: boolean; onImported: (s: Settings) => void }) {
  const features = useFeatures();
  const canExport = useCapability("settings_export");
  const canImport = useCapability("settings_import");
  const fileId = useId();
  const [msg, setMsg] = useState<{ tone: "ok" | "error"; text: string; issues?: SettingsIssue[] } | null>(null);
  const json = effectiveJson(settings);
  const lines = json.split("\n");
  return (
    <div className="json">
      <div className="json__bar">
        <span className="smuted">
          Effective settings, read-only{unsaved ? "; changes still being saved are not shown" : ""}. Edit them on the Settings tab, or import a file.
        </span>
        <button
          type="button"
          className="btn btn--sm"
          onClick={() => {
            navigator.clipboard.writeText(json).then(
              () => {
                setMsg({ tone: "ok", text: "Copied to the clipboard." });
              },
              (e: unknown) => {
                setMsg({ tone: "error", text: errorMessage(e) });
              },
            );
          }}
        >
          <Icon name="copy" size={14} />
          Copy
        </button>
        <button
          type="button"
          className="btn btn--sm"
          aria-disabled={!canExport || undefined}
          data-tip={canExport ? "Save a file you can import on another PC" : missingReason("settings_export")}
          onClick={() => {
            if (!canExport) return;
            features.settings.exportSettings().then(
              (text) => {
                const url = URL.createObjectURL(new Blob([text], { type: "application/json" }));
                const a = document.createElement("a");
                a.href = url;
                a.download = "strata-settings.json";
                a.click();
                URL.revokeObjectURL(url);
                setMsg({ tone: "ok", text: "Exported strata-settings.json." });
              },
              (e: unknown) => {
                setMsg({ tone: "error", text: errorMessage(e) });
              },
            );
          }}
        >
          <Icon name="download" size={14} />
          Export…
        </button>
        <label htmlFor={fileId} className="btn btn--sm" aria-disabled={!canImport || undefined} data-tip={canImport ? "Replace these settings from an exported file" : missingReason("settings_import")}>
          <Icon name="upload" size={14} />
          Import…
        </label>
        <input
          id={fileId}
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
                  if (r.issues.length > 0) {
                    setMsg({ tone: "error", text: "Import refused: the file has invalid settings. Nothing was changed.", issues: r.issues });
                    return;
                  }
                  onImported(r.settings);
                  setMsg({ tone: "ok", text: "Settings imported." });
                },
                (err: unknown) => {
                  setMsg({ tone: "error", text: `Import failed: ${errorMessage(err)}` });
                },
              );
          }}
        />
      </div>
      {msg && (
        <p className={`sresult sresult--${msg.tone}`} role={msg.tone === "error" ? "alert" : "status"}>
          <Icon name={msg.tone === "ok" ? "check" : "warning"} size={14} />
          <span>
            {msg.text}
            {msg.issues && (
              <ul className="sproblems">
                {msg.issues.map((i) => (
                  <li key={i.key}>
                    <code>{i.key}</code>: {i.message}
                  </li>
                ))}
              </ul>
            )}
          </span>
        </p>
      )}
      <pre className="json__code" tabIndex={0} aria-label="Effective settings as JSON">
        {lines.map((l, i) => (
          <span key={i} className="json__line">
            <span className="json__ln" aria-hidden="true">
              {i + 1}
            </span>
            <JsonLine line={l} />
            {"\n"}
          </span>
        ))}
      </pre>
    </div>
  );
}

// -----------------------------------------------------------------------------
// Page
// -----------------------------------------------------------------------------

function categoryPanels(id: CategoryId, settings: Settings | null): ReactNode {
  switch (id) {
    case "rules":
      return <RulesPanels />;
    case "helper":
      return <HelperPanels />;
    case "updates":
      return (
        <Panel title="Check now">
          <UpdateCheck />
        </Panel>
      );
    case "privacy":
      return <DataPanels />;
    case "about":
      return <AboutPanels settings={settings} />;
    default:
      return null;
  }
}

function panelMatches(id: CategoryId, q: SettingsQuery): boolean {
  if (q.modifiedOnly) return false;
  if (q.words.length === 0) return true;
  const cat = SETTINGS_CATEGORIES.find((c) => c.id === id);
  const hay = `${cat?.title ?? ""} ${PANEL_TERMS[id] ?? ""}`.toLowerCase();
  return q.words.every((w) => hay.includes(w));
}

function SaveStatus({ editor, canSave }: { editor: Editor | null; canSave: boolean }) {
  if (!editor) return null;
  const n = editor.issues.size;
  let text: string;
  let tone = "";
  if (!canSave) {
    text = "Changes can’t be saved in this build";
    tone = "warn";
  } else if (n > 0) {
    text = `${formatCount(n)} ${n === 1 ? "setting needs" : "settings need"} attention — not saved`;
    tone = "warn";
  } else if (editor.save.kind === "saving") text = "Saving…";
  else if (editor.save.kind === "failed") {
    text = `Not saved: ${editor.save.message}`;
    tone = "warn";
  } else if (editor.save.kind === "saved") text = "All changes saved";
  else text = "Changes save automatically";
  return (
    <span className={`settings__status${tone ? ` settings__status--${tone}` : ""}`} role="status">
      {tone === "warn" ? <Icon name="warning" size={14} /> : editor.save.kind === "saved" ? <Icon name="check" size={14} /> : null}
      {text}
    </span>
  );
}

/** Props for {@link SettingsPage}. */
export interface SettingsPageProps {
  /** Loaded settings, or `null` when this build has no settings store. */
  initial: Settings | null;
  canSave: boolean;
}

/** The Settings page body. */
export function SettingsPage({ initial, canSave }: SettingsPageProps) {
  const editor = useSettingsEditor(initial, canSave);
  const unavailable = useUnavailableReason();
  const [query, setQuery] = useState("");
  const [mode, setMode] = useState<"ui" | "json">("ui");
  const [active, setActive] = useState<CategoryId>("general");
  const searchRef = useRef<HTMLInputElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  const q = parseQuery(query);
  const searching = q.words.length > 0 || q.modifiedOnly;
  const draft = editor?.draft ?? null;

  const visible = useMemo(() => {
    const out = new Map<CategoryId, { settings: SettingDef[]; panels: boolean }>();
    for (const c of SETTINGS_CATEGORIES) {
      const settings = SETTINGS.filter((d) => d.category === c.id && matchesQuery(d, q, draft));
      const panels = c.id in PANEL_TERMS && panelMatches(c.id, q);
      if (settings.length > 0 || panels || !searching) out.set(c.id, { settings, panels });
    }
    return out;
    // eslint-disable-next-line react-hooks/exhaustive-deps -- `q` is derived from `query`.
  }, [query, draft, searching]);
  const matchCount = [...visible.values()].reduce((a, v) => a + v.settings.length, 0);
  const current: CategoryId | null = visible.has(active) ? active : ([...visible.keys()][0] ?? null);

  // Scrolls only the content pane; scrollIntoView would also move the workspace.
  const scrollTo = (id: CategoryId) => {
    setActive(id);
    const root = contentRef.current;
    const el = document.getElementById(`settings-cat-${id}`);
    if (!root || !el) return;
    root.scrollTop += el.getBoundingClientRect().top - root.getBoundingClientRect().top;
  };

  useEffect(() => {
    const focus = useLayout.getState().settingsFocus;
    if (focus && SETTINGS_CATEGORIES.some((c) => c.id === focus)) {
      useLayout.setState({ settingsFocus: null });
      requestAnimationFrame(() => {
        scrollTo(focus as CategoryId);
      });
    } else {
      searchRef.current?.focus();
    }
  }, []);

  // Scroll spy: the category nearest the top of the content becomes active.
  useEffect(() => {
    const root = contentRef.current;
    if (!root || mode !== "ui") return;
    const onScroll = () => {
      const top = root.getBoundingClientRect().top;
      let current: CategoryId | null = null;
      for (const el of root.querySelectorAll<HTMLElement>("[data-category]")) {
        if (el.getBoundingClientRect().top - top <= 48) current = el.dataset.category as CategoryId;
      }
      if (current) setActive(current);
    };
    root.addEventListener("scroll", onScroll, { passive: true });
    return () => {
      root.removeEventListener("scroll", onScroll);
    };
  }, [mode]);

  return (
    <section className="settings" aria-labelledby="settings-title">
      <header className="settings__head">
        <h1 id="settings-title" className="settings__title">
          Settings
        </h1>
        <div className="settings__search">
          <Icon name="search" size={14} />
          <input
            ref={searchRef}
            type="search"
            aria-label="Search settings"
            aria-describedby="settings-search-hint"
            placeholder="Search settings"
            value={query}
            onChange={(e) => {
              setQuery(e.target.value);
              if (mode === "json") setMode("ui");
            }}
            onKeyDown={(e) => {
              if (e.key === "Escape" && query !== "") {
                e.stopPropagation();
                setQuery("");
              }
            }}
          />
          <button
            type="button"
            className={q.modifiedOnly ? "settings__chip is-on" : "settings__chip"}
            aria-pressed={q.modifiedOnly}
            data-tip="Show only settings you changed"
            onClick={() => {
              setQuery(q.modifiedOnly ? query.replace(/@modified\s*/i, "").trim() : `@modified ${query}`.trim());
            }}
          >
            Modified
          </button>
          <span id="settings-search-hint" className="visually-hidden">
            Matches titles, descriptions and keys. Type @modified to list changed settings.
          </span>
        </div>
        <div className="seg" role="radiogroup" aria-label="Settings view">
          {(["ui", "json"] as const).map((m) => (
            <button
              key={m}
              type="button"
              role="radio"
              aria-checked={mode === m}
              tabIndex={mode === m ? 0 : -1}
              onClick={() => {
                setMode(m);
              }}
            >
              <Icon name={m === "ui" ? "list" : "braces"} size={14} />
              <span className="seg__text">{m === "ui" ? "Settings" : "JSON"}</span>
            </button>
          ))}
        </div>
        <SaveStatus editor={editor} canSave={canSave} />
      </header>
      <div className="settings__layout">
        <nav className="settings__nav" aria-label="Settings categories">
          <ul>
            {SETTINGS_CATEGORIES.map((c) => {
              const v = visible.get(c.id);
              const modified = draft ? SETTINGS.filter((d) => d.category === c.id && isModified(draft, d)).length : 0;
              return (
                <li key={c.id}>
                  <button
                    type="button"
                    className="settings__navitem"
                    aria-current={mode === "ui" && current === c.id ? "true" : undefined}
                    aria-disabled={searching && !v ? true : undefined}
                    onClick={() => {
                      if (mode === "json") setMode("ui");
                      if (v) requestAnimationFrame(() => {
                        scrollTo(c.id);
                      });
                    }}
                  >
                    <span className="settings__navlabel">{c.title}</span>
                    {searching && v && v.settings.length > 0 && <span className="settings__navcount">{v.settings.length}</span>}
                    {!searching && modified > 0 && (
                      <span className="settings__navdot" aria-label={`${modified} modified`} data-tip={`${modified} modified`} />
                    )}
                  </button>
                </li>
              );
            })}
          </ul>
        </nav>
        <div className="settings__content" ref={contentRef} tabIndex={-1}>
          {mode === "json" ? (
            draft && editor ? (
              <JsonView settings={editor.saved} unsaved={!sameValue(editor.saved, editor.draft)} onImported={editor.replace} />
            ) : (
              <p className="smuted settings__empty">{unavailable ? `The JSON view needs the settings store. ${unavailable}` : "The JSON view needs the settings store, which this build doesn’t include."}</p>
            )
          ) : (
            <>
              {searching && (
                <p className="settings__count" role="status">
                  {matchCount === 0 && visible.size === 0
                    ? `No settings match “${query.trim()}”.`
                    : `${formatCount(matchCount)} ${matchCount === 1 ? "setting" : "settings"} found`}
                </p>
              )}
              {!editor && (
                <div className="settings__notice" role="note">
                  <Icon name="info" size={16} />
                  <p>{unavailable ? `Settings aren’t available here. ${unavailable}` : "Saving settings isn’t available in this build. The tools below still work where the engine supports them."}</p>
                </div>
              )}
              {SETTINGS_CATEGORIES.map((c) => {
                const v = visible.get(c.id);
                if (!v) return null;
                const groups = [...new Set(v.settings.map((d) => d.group ?? ""))];
                return (
                  <section key={c.id} id={`settings-cat-${c.id}`} data-category={c.id} className="scat" aria-labelledby={`settings-cat-${c.id}-h`}>
                    <h2 id={`settings-cat-${c.id}-h`} className="scat__title">
                      <Highlight text={c.title} words={q.words} />
                    </h2>
                    <p className="scat__desc">{c.description}</p>
                    {editor &&
                      groups.map((g) => (
                        <div key={g} className="scat__group">
                          {g && <h3 className="scat__subtitle">{g}</h3>}
                          {v.settings
                            .filter((d) => (d.group ?? "") === g)
                            .map((d) => (
                              <SettingRow
                                key={`${d.key}:${editor.revision}`}
                                def={d}
                                value={getSetting(editor.draft, d.key)}
                                error={editor.issues.get(d.key) ?? null}
                                modified={isModified(editor.draft, d)}
                                disabled={!canSave}
                                words={q.words}
                                onChange={(value) => {
                                  editor.set(d.key, value);
                                }}
                                onReset={() => {
                                  editor.reset(d.key);
                                }}
                              />
                            ))}
                        </div>
                      ))}
                    {v.panels && categoryPanels(c.id, draft)}
                  </section>
                );
              })}
            </>
          )}
        </div>
      </div>
    </section>
  );
}

/** Settings view: loads settings, then renders the page. */
export function SettingsView() {
  const features = useFeatures();
  const canLoad = useCapability("settings_load");
  const canSave = useCapability("settings_save");
  const [load, reload] = useLoad(() => features.settings.loadSettings(), [features], canLoad);
  if (!canLoad) return <SettingsPage initial={null} canSave={false} />;
  return (
    <LoadState load={load} feature="Settings" command="settings_load" onRetry={reload}>
      {(s) => <SettingsPage initial={s} canSave={canSave} />}
    </LoadState>
  );
}
