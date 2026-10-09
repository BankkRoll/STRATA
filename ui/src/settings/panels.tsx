/**
 * The non-setting parts of the Settings page: rules tooling and the path
 * tester, helper and elevation status, update checks, clearing Strata's
 * data, reporting an issue, the privacy statement and About.
 *
 * Every action is gated on `app_capabilities`; a command missing from this
 * build shows a disabled control with the reason instead of pretending.
 */
import { isTauri } from "@tauri-apps/api/core";
import { useId, useMemo, useState, type ReactNode } from "react";
import { ConfirmDialog, LoadState, SafetyBadge, useCapability, useLoad, useUnavailableReason } from "../components/feature";
import { Icon, type IconName } from "../components/icons";
import { useFeatures } from "../features";
import { call, errorMessage, getAppInfo } from "../lib/backend";
import { formatCount } from "../lib/format";
import { categoryInfo } from "../lib/palette";
import type { ClearableData, Explanation, RuleInfo, Settings } from "../lib/settings";
import { useServices } from "../services";
import { AppMark } from "../shell/TitleBar";
import { useVolumes } from "../store/volumes";

/** Project links shown in About. */
export const LINKS = {
  repo: "https://github.com/BankkRoll/STRATA",
  docs: "https://github.com/BankkRoll/STRATA/tree/main/docs",
  privacy: "https://github.com/BankkRoll/STRATA/blob/main/docs/PRIVACY.md",
  license: "https://github.com/BankkRoll/STRATA/blob/main/LICENSE",
} as const;

// -----------------------------------------------------------------------------
// Building blocks
// -----------------------------------------------------------------------------

/** Why a backend command is not usable, for disabled controls. */
export function missingReason(command: string): string {
  return `Not available in this build: the engine doesn’t provide ${command} yet.`;
}

/**
 * {@link missingReason}, or the services' own reason when they give one
 * (the website demo).
 *
 * @returns Reason lookup by command name.
 */
export function useMissingReason(): (command: string) => string {
  const reason = useUnavailableReason();
  return (command) => reason ?? missingReason(command);
}

/** Props for {@link CapButton}. */
interface CapButtonProps {
  /** Backend commands the action needs. */
  commands: string[];
  icon?: IconName;
  primary?: boolean;
  danger?: boolean;
  busy?: boolean;
  onClick: () => void;
  children: ReactNode;
}

/** A button that is disabled, with the reason as its tooltip, when its command is missing. */
function CapButton({ commands, icon, primary, danger, busy, onClick, children }: CapButtonProps) {
  const ok = useCapability(...commands);
  const rid = useId();
  const whyMissing = useMissingReason();
  const reason = ok ? undefined : whyMissing(commands.join(", "));
  return (
    <>
      <button
        type="button"
        className={`btn btn--sm${primary ? " btn--primary" : ""}${danger ? " btn--danger-quiet" : ""}`}
        aria-disabled={!ok || busy ? true : undefined}
        aria-describedby={reason ? rid : undefined}
        data-tip={reason}
        onClick={() => {
          if (ok && !busy) onClick();
        }}
      >
        {icon && <Icon name={icon} size={14} />}
        {children}
      </button>
      {reason && (
        <span id={rid} className="visually-hidden">
          {reason}
        </span>
      )}
    </>
  );
}

/** A titled block inside a category. */
export function Panel({ title, description, children, id }: { id?: string; title: string; description?: ReactNode; children: ReactNode }) {
  const hid = useId();
  return (
    <section className="spanel" aria-labelledby={hid} id={id}>
      <h3 className="spanel__title" id={hid}>
        {title}
      </h3>
      {description && <p className="spanel__desc">{description}</p>}
      <div className="spanel__body">{children}</div>
    </section>
  );
}

function Result({ tone, children }: { tone: "ok" | "error" | "info"; children: ReactNode }) {
  return (
    <p className={`sresult sresult--${tone}`} role={tone === "error" ? "alert" : "status"}>
      <Icon name={tone === "ok" ? "check" : tone === "error" ? "warning" : "info"} size={14} />
      <span>{children}</span>
    </p>
  );
}

// -----------------------------------------------------------------------------
// Rules
// -----------------------------------------------------------------------------

function RulePacks() {
  const features = useFeatures();
  const can = useCapability("rules_list");
  const whyMissing = useMissingReason();
  const [load, reload] = useLoad(() => features.settings.fetchRules(), [features], can);
  if (!can) return <p className="smuted">{whyMissing("rules_list")}</p>;
  return (
    <LoadState load={load} feature="Rules" command="rules_list" onRetry={reload}>
      {(rules) => <PackTable rules={rules} />}
    </LoadState>
  );
}

function PackTable({ rules }: { rules: RuleInfo[] }) {
  const packs = useMemo(() => {
    const m = new Map<string, { pack: string; source: RuleInfo["source"]; count: number; overridden: number }>();
    for (const r of rules) {
      const k = `${r.source}:${r.pack}`;
      const p = m.get(k) ?? { pack: r.pack, source: r.source, count: 0, overridden: 0 };
      p.count++;
      if (r.overriddenBy) p.overridden++;
      m.set(k, p);
    }
    return [...m.values()].sort((a, b) => (a.source === b.source ? a.pack.localeCompare(b.pack) : a.source === "builtin" ? -1 : 1));
  }, [rules]);
  const builtin = rules.filter((r) => r.source === "builtin").length;
  const user = rules.length - builtin;
  return (
    <>
      <p className="smuted">
        {formatCount(builtin)} built-in rules in {formatCount(packs.filter((p) => p.source === "builtin").length)} packs, {formatCount(user)} of your own. Built-in packs are read-only.
      </p>
      <div className="stable-wrap" tabIndex={0} role="region" aria-label="Rule pack list">
        <table className="stable">
          <thead>
            <tr>
              <th scope="col">Pack</th>
              <th scope="col">Source</th>
              <th scope="col" className="num">
                Rules
              </th>
              <th scope="col" className="num">
                Overridden
              </th>
            </tr>
          </thead>
          <tbody>
            {packs.map((p) => (
              <tr key={`${p.source}:${p.pack}`}>
                <th scope="row">
                  <code>{p.pack}</code>
                </th>
                <td>{p.source === "builtin" ? "Built-in, read-only" : "Yours"}</td>
                <td className="num">{formatCount(p.count)}</td>
                <td className="num">{p.overridden > 0 ? formatCount(p.overridden) : "—"}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  );
}

function RuleActions() {
  const features = useFeatures();
  const [msg, setMsg] = useState<{ tone: "ok" | "error"; text: string; problems?: { file: string; message: string }[] } | null>(null);
  const [busy, setBusy] = useState(false);
  return (
    <>
      <div className="sactions">
        <CapButton
          commands={["rules_open_folder"]}
          icon="folderOpen"
          onClick={() => {
            features.settings.openRulesFolder().catch((e: unknown) => {
              setMsg({ tone: "error", text: errorMessage(e) });
            });
          }}
        >
          Open user rules folder
        </CapButton>
        <CapButton
          commands={["rules_reload"]}
          icon="refresh"
          busy={busy}
          onClick={() => {
            setBusy(true);
            features.settings
              .reloadRules()
              .then(
                (r) => {
                  setMsg({
                    tone: r.problems.length > 0 ? "error" : "ok",
                    text: `Loaded ${formatCount(r.builtin)} built-in and ${formatCount(r.user)} user rules${r.problems.length > 0 ? `, with ${formatCount(r.problems.length)} ${r.problems.length === 1 ? "problem" : "problems"}` : ""}.`,
                    problems: r.problems,
                  });
                },
                (e: unknown) => {
                  setMsg({ tone: "error", text: errorMessage(e) });
                },
              )
              .finally(() => {
                setBusy(false);
              });
          }}
        >
          {busy ? "Reloading…" : "Reload rules"}
        </CapButton>
      </div>
      {msg && (
        <Result tone={msg.tone}>
          {msg.text}
          {msg.problems && msg.problems.length > 0 && (
            <ul className="sproblems">
              {msg.problems.map((p, i) => (
                <li key={i}>
                  <code>{p.file}</code>: {p.message}
                </li>
              ))}
            </ul>
          )}
        </Result>
      )}
    </>
  );
}

/** Shows a classifier explanation: result, matched rule, precedence and the path walk. */
export function ExplainResult({ e }: { e: Explanation }) {
  const rule = e.rule;
  return (
    <div className="explain-card" aria-live="polite">
      <dl className="explain-card__grid">
        <dt>Classified as</dt>
        <dd>
          <strong>{categoryInfo(e.result.category).label}</strong> <SafetyBadge tier={e.result.safety} />
          {e.result.regenerable && <span className="sbadge-pill">Regenerable</span>}
        </dd>
        <dt>Matched rule</dt>
        <dd>
          {rule ? (
            <>
              {rule.name} <code>{rule.id}</code>
              <span className="smuted">
                {" "}
                · pack <code>{rule.pack}</code> · {rule.source === "builtin" ? "built-in" : "yours"}
                {rule.overriddenBy && ` · overridden by ${rule.overriddenBy}`}
              </span>
            </>
          ) : e.result.ruleId ? (
            <code>{e.result.ruleId}</code>
          ) : (
            "No rule matched, so it counts as your data."
          )}
        </dd>
        {rule && (
          <>
            <dt>Explanation</dt>
            <dd>{rule.explain}</dd>
          </>
        )}
        {e.originPath && (
          <>
            <dt>Inherited from</dt>
            <dd>
              <code>{e.originPath}</code>
            </dd>
          </>
        )}
      </dl>
      {e.trace.length > 0 && (
        <div className="explain-card__section">
          <h4>Precedence</h4>
          <p className="smuted">Candidate rules in evaluation order; the first line decided.</p>
          <ol className="explain-card__trace">
            {e.trace.map((t, i) => (
              <li key={i} className={i === 0 ? "is-winner" : undefined}>
                {t}
              </li>
            ))}
          </ol>
        </div>
      )}
      {e.steps.length > 0 && (
        <div className="explain-card__section">
          <h4>Along the path</h4>
          <div className="stable-wrap" tabIndex={0} role="region" aria-label="Classification along the path">
            <table className="stable">
              <thead>
                <tr>
                  <th scope="col">Folder</th>
                  <th scope="col">Rule</th>
                  <th scope="col">Category</th>
                  <th scope="col">Safety</th>
                </tr>
              </thead>
              <tbody>
                {e.steps.map((s, i) => (
                  <tr key={i}>
                    <th scope="row">
                      <code>{s.path}</code>
                    </th>
                    <td>{s.ruleId ? <code>{s.ruleId}</code> : "—"}</td>
                    <td>{categoryInfo(s.category).label}</td>
                    <td>
                      <SafetyBadge tier={s.safety} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </div>
  );
}

function PathTester() {
  const features = useFeatures();
  const can = useCapability("rules_explain");
  const whyMissing = useMissingReason();
  const id = useId();
  const [path, setPath] = useState("");
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<{ ok: Explanation } | { error: string } | null>(null);
  const run = () => {
    const p = path.trim();
    if (!can || p === "") return;
    setBusy(true);
    features.settings
      .explainPath(p)
      .then(
        (ok) => {
          setResult({ ok });
        },
        (e: unknown) => {
          setResult({ error: errorMessage(e) });
        },
      )
      .finally(() => {
        setBusy(false);
      });
  };
  return (
    <>
      <form
        className="tester"
        onSubmit={(e) => {
          e.preventDefault();
          run();
        }}
      >
        <label htmlFor={id} className="visually-hidden">
          Why is this classified as…? Test a path
        </label>
        <input
          id={id}
          type="text"
          className="field-text field-text--mono"
          placeholder="C:\Users\me\AppData\Local\Temp"
          spellCheck={false}
          value={path}
          onChange={(e) => {
            setPath(e.target.value);
          }}
        />
        <button type="submit" className="btn btn--sm btn--primary" aria-disabled={!can || path.trim() === "" || busy ? true : undefined} data-tip={can ? undefined : whyMissing("rules_explain")}>
          {busy ? "Explaining…" : "Explain"}
        </button>
      </form>
      {!can && <p className="smuted">{whyMissing("rules_explain")}</p>}
      {result && ("error" in result ? <Result tone="error">{result.error}</Result> : <ExplainResult e={result.ok} />)}
    </>
  );
}

/** Rules: packs, folder, reload and the path tester. */
export function RulesPanels() {
  return (
    <>
      <Panel title="Rule packs" description="Rules decide each item’s category and safety tier. Your packs load after the built-in ones.">
        <RuleActions />
        <RulePacks />
      </Panel>
      <Panel title="Why is this classified as…?" description="Test a path to see which rule matched, which rules it beat, and the reason. Only metadata is read.">
        <PathTester />
      </Panel>
    </>
  );
}

// -----------------------------------------------------------------------------
// Helper and elevation
// -----------------------------------------------------------------------------

/** Elevation state and the helper service. */
export function HelperPanels() {
  const services = useServices();
  const features = useFeatures();
  const helper = useVolumes((s) => s.helper);
  const canStatus = useCapability("helper_service_status");
  const whyMissing = useMissingReason();
  const [svc, reloadSvc] = useLoad(() => features.settings.fetchHelperService(), [features], canStatus);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{ tone: "ok" | "error"; text: string } | null>(null);
  const act = (p: Promise<unknown>, done: string) => {
    setBusy(true);
    p.then(
      () => {
        setMsg({ tone: "ok", text: done });
        reloadSvc();
      },
      (e: unknown) => {
        setMsg({ tone: "error", text: errorMessage(e) });
      },
    ).finally(() => {
      setBusy(false);
    });
  };
  return (
    <>
      <Panel title="Status">
        <dl className="kvlist">
          <dt>Scan mode</dt>
          <dd>
            {helper === null ? (
              <span className="smuted">Unknown in this build</span>
            ) : helper.elevated ? (
              <span className="state-pill state-pill--ok">
                <Icon name="shield" size={12} />
                Fast scan: the helper reads the file table directly
              </span>
            ) : (
              <span className="state-pill">Standard scan: some system folders are hidden</span>
            )}
          </dd>
          <dt>Helper service</dt>
          <dd>
            {!canStatus ? (
              <span className="smuted">{whyMissing("helper_service_status")}</span>
            ) : svc.state === "ready" ? (
              <span className={svc.data.running ? "state-pill state-pill--ok" : "state-pill"}>{svc.data.installed ? (svc.data.running ? "Installed and running" : "Installed, not running") : "Not installed"}</span>
            ) : svc.state === "loading" ? (
              <span className="smuted">Checking…</span>
            ) : (
              <span className="smuted">{svc.message}</span>
            )}
          </dd>
        </dl>
        <div className="sactions">
          {helper && !helper.elevated && (
            <CapButton
              commands={["helper_elevate"]}
              icon="shield"
              primary
              busy={busy}
              onClick={() => {
                setBusy(true);
                services.volumes
                  .elevate()
                  .then(
                    (h) => {
                      useVolumes.getState().setHelper(h);
                      setMsg({ tone: "ok", text: "Fast scan is on." });
                    },
                    (e: unknown) => {
                      setMsg({ tone: "error", text: `Fast scan not enabled: ${errorMessage(e)}` });
                    },
                  )
                  .finally(() => {
                    setBusy(false);
                  });
              }}
            >
              Enable fast scan now
            </CapButton>
          )}
          {svc.state === "ready" && svc.data.installed ? (
            <CapButton
              commands={["helper_service_uninstall"]}
              icon="shield"
              busy={busy}
              onClick={() => {
                act(features.settings.uninstallHelperService(), "The helper service was removed.");
              }}
            >
              Uninstall service…
            </CapButton>
          ) : (
            <CapButton
              commands={["helper_service_install"]}
              icon="shield"
              busy={busy}
              onClick={() => {
                act(features.settings.installHelperService(), "The helper service is installed.");
              }}
            >
              Install service…
            </CapButton>
          )}
        </div>
        <p className="smuted">Installing or removing the service asks for administrator permission. The service only reads file system metadata; it never changes files on its own.</p>
        {msg && <Result tone={msg.tone}>{msg.text}</Result>}
      </Panel>
    </>
  );
}

// -----------------------------------------------------------------------------
// Updates and About
// -----------------------------------------------------------------------------

/** "Check for updates" with its result. */
export function UpdateCheck() {
  const features = useFeatures();
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{ tone: "ok" | "info" | "error"; text: string } | null>(null);
  return (
    <>
      <div className="sactions">
        <CapButton
          commands={["updates_check"]}
          icon="refresh"
          busy={busy}
          onClick={() => {
            setBusy(true);
            features.settings
              .checkForUpdates()
              .then(
                (u) => {
                  setMsg(
                    u.available && u.latest
                      ? { tone: "info", text: `Strata ${u.latest} is available on the ${u.channel} channel. It installs when you exit Strata.` }
                      : { tone: "ok", text: `Strata ${u.current} is up to date on the ${u.channel} channel.` },
                  );
                },
                (e: unknown) => {
                  setMsg({ tone: "error", text: `Couldn’t check for updates: ${errorMessage(e)}` });
                },
              )
              .finally(() => {
                setBusy(false);
              });
          }}
        >
          {busy ? "Checking…" : "Check for updates"}
        </CapButton>
      </div>
      {msg && <Result tone={msg.tone}>{msg.text}</Result>}
    </>
  );
}

function Licenses() {
  const features = useFeatures();
  const can = useCapability("about_licenses");
  const [open, setOpen] = useState(false);
  const [filter, setFilter] = useState("");
  const [load, reload] = useLoad(() => features.settings.fetchLicenses(), [features], can && open);
  return (
    <>
      <div className="sactions">
        <CapButton
          commands={["about_licenses"]}
          icon="file"
          onClick={() => {
            setOpen(!open);
          }}
        >
          {open ? "Hide third-party licenses" : "Third-party licenses"}
        </CapButton>
      </div>
      {open && (
        <LoadState load={load} feature="Licenses" command="about_licenses" onRetry={reload}>
          {(list) => {
            const q = filter.trim().toLowerCase();
            const shown = list.filter((l) => q === "" || l.name.toLowerCase().includes(q) || l.license.toLowerCase().includes(q));
            const cargo = list.filter((l) => l.ecosystem === "cargo").length;
            return (
              <div className="licenses">
                <div className="licenses__bar">
                  <input
                    type="search"
                    className="field-text"
                    aria-label="Filter licenses"
                    placeholder="Filter by name or license"
                    value={filter}
                    onChange={(e) => {
                      setFilter(e.target.value);
                    }}
                  />
                  <span className="smuted">
                    {formatCount(list.length)} components: {formatCount(cargo)} Rust crates, {formatCount(list.length - cargo)} npm packages
                  </span>
                </div>
                <div className="stable-wrap stable-wrap--tall" tabIndex={0} role="region" aria-label="Third-party licenses">
                  <table className="stable">
                    <thead>
                      <tr>
                        <th scope="col">Component</th>
                        <th scope="col">Version</th>
                        <th scope="col">License</th>
                        <th scope="col">Source</th>
                      </tr>
                    </thead>
                    <tbody>
                      {shown.map((l) => (
                        <tr key={`${l.ecosystem}:${l.name}:${l.version}`}>
                          <th scope="row">{l.name}</th>
                          <td className="num">{l.version}</td>
                          <td>
                            <code>{l.license}</code>
                          </td>
                          <td>{l.ecosystem === "cargo" ? "crates.io" : "npm"}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </div>
            );
          }}
        </LoadState>
      )}
    </>
  );
}

function CopyLink({ href, label }: { href: string; label: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <li className="links__item">
      <a
        href={href}
        target="_blank"
        rel="noreferrer noopener"
        className="links__a"
        onClick={(e) => {
          // NOTE: the WebView cannot open new windows; inside the app the
          // backend opens allow-listed pages in the default browser.
          if (!isTauri()) return;
          e.preventDefault();
          void call<null>("open_url", { url: href }).catch((err: unknown) => {
            console.error("open_url failed", err);
          });
        }}
      >
        <Icon name="external" size={14} />
        {label}
      </a>
      <button
        type="button"
        className="icon-btn icon-btn--sm"
        aria-label={`Copy link to ${label}`}
        data-tip={copied ? "Copied" : "Copy link"}
        onClick={() => {
          void navigator.clipboard.writeText(href).then(() => {
            setCopied(true);
          });
        }}
      >
        <Icon name={copied ? "check" : "copy"} size={14} />
      </button>
    </li>
  );
}

/** About: version, build, channel, updates, licenses and links. */
export function AboutPanels({ settings }: { settings: Settings | null }) {
  const [info] = useLoad(() => getAppInfo(), []);
  return (
    <>
      <div className="about">
        <AppMark size={40} />
        <div>
          <p className="about__name">Strata</p>
          <p className="smuted">
            Version {info.state === "ready" ? info.data.version : "…"}
            {info.state === "ready" && info.data.windowsBuild > 0 && ` · Windows build ${info.data.windowsBuild}`}
            {settings && ` · ${settings.updates.channel === "beta" ? "Beta" : "Stable"} channel`}
          </p>
          <p className="smuted">Disk-space intelligence for Windows. Free and open source under the MIT License.</p>
        </div>
      </div>
      <Panel title="Updates">
        <UpdateCheck />
      </Panel>
      <Panel title="Licenses">
        <Licenses />
      </Panel>
      <Panel title="Links">
        <ul className="links">
          <CopyLink href={LINKS.repo} label="Source code on GitHub" />
          <CopyLink href={LINKS.docs} label="Documentation" />
          <CopyLink href={LINKS.privacy} label="Privacy" />
          <CopyLink href={LINKS.license} label="MIT License" />
        </ul>
      </Panel>
    </>
  );
}

// -----------------------------------------------------------------------------
// Data and privacy
// -----------------------------------------------------------------------------

const CLEAR: Readonly<Record<ClearableData, { title: string; text: string; icon: IconName }>> = {
  history: { title: "Clear history", text: "Deletes every snapshot. Usage charts and “what changed” start over.", icon: "history" },
  activity: { title: "Clear activity", text: "Deletes all recorded program activity.", icon: "pulse" },
  caches: { title: "Clear caches", text: "Deletes the saved scan indexes and duplicate hashes. The next launch rescans.", icon: "drive" },
};

/** Clearing Strata's data, reporting an issue and the privacy statement. */
export function DataPanels() {
  const features = useFeatures();
  const [confirm, setConfirm] = useState<ClearableData | null>(null);
  const [msg, setMsg] = useState<{ tone: "ok" | "error"; text: string } | null>(null);
  const [reportMsg, setReportMsg] = useState<{ tone: "ok" | "error"; text: string } | null>(null);
  return (
    <>
      <Panel title="Clear Strata’s data" description="These remove only what Strata recorded. Your files are never touched.">
        <ul className="clear-list">
          {(Object.keys(CLEAR) as ClearableData[]).map((k) => (
            <li key={k} className="clear-list__item">
              <Icon name={CLEAR[k].icon} size={16} />
              <div>
                <p className="clear-list__title">{CLEAR[k].title}</p>
                <p className="smuted">{CLEAR[k].text}</p>
              </div>
              <CapButton
                commands={["data_clear"]}
                danger
                onClick={() => {
                  setConfirm(k);
                }}
              >
                {CLEAR[k].title}…
              </CapButton>
            </li>
          ))}
        </ul>
        {msg && <Result tone={msg.tone}>{msg.text}</Result>}
      </Panel>
      <Panel title="Report an issue" description="Opens a new GitHub issue in your browser, filled in with Strata’s version, Windows build and architecture. You review it before anything is sent, and it never includes paths or file names.">
        <div className="sactions">
          <CapButton
            commands={["report_issue"]}
            icon="bug"
            onClick={() => {
              call<null>("report_issue").then(
                () => {
                  setReportMsg({ tone: "ok", text: "The issue form opened in your browser." });
                },
                (e: unknown) => {
                  setReportMsg({ tone: "error", text: errorMessage(e) });
                },
              );
            }}
          >
            Report an issue…
          </CapButton>
        </div>
        {reportMsg && <Result tone={reportMsg.tone}>{reportMsg.text}</Result>}
      </Panel>
      <Panel title="No telemetry">
        <div className="statement">
          <Icon name="shield" size={16} />
          <p>
            Strata has no telemetry, analytics or crash reporting. Scans, paths and file names never leave this PC. The only network requests are update checks and
            downloads from GitHub Releases, and the issue form you choose to open.
          </p>
        </div>
      </Panel>
      {confirm && (
        <ConfirmDialog
          title={`${CLEAR[confirm].title}?`}
          confirmLabel={CLEAR[confirm].title}
          danger
          onCancel={() => {
            setConfirm(null);
          }}
          onConfirm={() => {
            const what = confirm;
            setConfirm(null);
            features.settings.clearData(what).then(
              () => {
                setMsg({ tone: "ok", text: `${CLEAR[what].title}: done.` });
              },
              (e: unknown) => {
                setMsg({ tone: "error", text: errorMessage(e) });
              },
            );
          }}
        >
          <p>{CLEAR[confirm].text} This can’t be undone. Your files are not touched.</p>
        </ConfirmDialog>
      )}
    </>
  );
}
