/**
 * Build-time content drawn from the repository itself, so the site never
 * drifts from the documents and rule packs it describes.
 *
 * Responsibilities:
 * - Rule-pack facts from `rules/*.toml`: rule, pack, category and app counts
 *   and the safety-tier split.
 * - The FAQ page, rendered from `docs/FAQ.md` as a searchable accordion.
 * - Tables and lists lifted from `docs/REQUIREMENTS.md`, `docs/PRIVACY.md` and
 *   `SECURITY.md` for the security page.
 *
 * Everything here is synchronous file reading; a missing or reshaped document
 * fails the build loudly instead of shipping an empty section.
 */
import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join } from "node:path";
import { REPO_URL } from "./github.ts";
import { esc, inline, markdown, parseTable, plain, renderTable, sections, type LinkResolver } from "./markdown.ts";

const ROOT = fileURLToPath(new URL("../../", import.meta.url));
const read = (rel: string): string => readFileSync(join(ROOT, rel), "utf8");

/** Every repository file the build reads, for dev-server reloads. */
export const CONTENT_FILES = ["docs/FAQ.md", "docs/PRIVACY.md", "docs/REQUIREMENTS.md", "SECURITY.md"].map((f) => join(ROOT, f));
/** The rule-pack folder, for dev-server reloads. */
export const RULES_DIR = join(ROOT, "rules");

// -----------------------------------------------------------------------------
// Links
// -----------------------------------------------------------------------------

/**
 * Resolves a link written inside a repository document to its URL on the
 * site: FAQ and privacy anchors stay on the site, everything else points at
 * the file on GitHub.
 *
 * @param from - Repository-relative folder of the source document, e.g. `docs`.
 * @param base - The site's base path.
 */
function resolver(from: string, base: string): LinkResolver {
  return (href) => {
    const [path = "", hash = ""] = href.split("#");
    if (!path) return `#${hash}`;
    const parts = [...(from ? from.split("/") : []), ...path.split("/")];
    const out: string[] = [];
    for (const p of parts) {
      if (p === "..") out.pop();
      else if (p && p !== ".") out.push(p);
    }
    const file = out.join("/");
    const anchor = hash ? `#${hash}` : "";
    if (file === "docs/FAQ.md") return `${base}faq/${anchor}`;
    if (file === "docs/PRIVACY.md") return `${base}security/#privacy`;
    if (file === "SECURITY.md") return `${base}security/#report`;
    if (file === "README.md") return `${REPO_URL}${anchor}`;
    return `${REPO_URL}/blob/main/${file}${anchor}`;
  };
}

const section = (doc: string, md: string, title: string): string => {
  const s = sections(md).find((x) => x.title === title);
  if (!s) throw new Error(`${doc}: section "${title}" not found`);
  return s.body;
};

// -----------------------------------------------------------------------------
// Rule packs
// -----------------------------------------------------------------------------

/** Display names for `strata_core::Category`. */
const CATEGORY_NAMES: Record<string, string> = {
  system: "System",
  apps: "Apps",
  games: "Games",
  ai_models: "AI models",
  dev_build: "Developer files",
  caches: "Caches",
  temp: "Temporary files",
  downloads: "Downloads",
  documents: "Documents",
  media: "Media",
  archives: "Archives",
  cloud: "Cloud files",
  recycle_bin: "Recycle Bin",
  ntfs_metadata: "NTFS metadata",
};

/** Safety tiers, from most to least permissive. */
const TIERS = [
  { id: "safe", label: "Safe", desc: "Regenerable, no user data" },
  { id: "probably", label: "Probably", desc: "Likely junk; review recommended" },
  { id: "careful", label: "Careful", desc: "User data or large re-downloads" },
  { id: "never", label: "Never", desc: "No delete action anywhere" },
] as const;

/** Facts about the built-in rule packs. */
export interface RuleFacts {
  rules: number;
  packs: number;
  /** Distinct categories the rules assign, `unknown` excluded. */
  categories: number;
  /** Distinct owning apps the rules name. */
  apps: number;
  /** Rule count per safety tier id. */
  tiers: Record<string, number>;
  /** Pack id, rule count and description, largest first. */
  packList: { id: string; rules: number; description: string }[];
  /** Category display names, most rules first. */
  categoryNames: string[];
}

/**
 * Reads the built-in rule packs. It reads only the top-level keys it needs
 * with line patterns; `strata-classify` validates the full schema in CI.
 *
 * @returns Counts and lists for the pages.
 */
export function loadRuleFacts(): RuleFacts {
  const cats = new Map<string, number>();
  const apps = new Set<string>();
  const tiers: Record<string, number> = {};
  const packList: RuleFacts["packList"] = [];
  let rules = 0;
  for (const file of readdirSync(RULES_DIR).filter((f) => f.endsWith(".toml"))) {
    const text = readFileSync(join(RULES_DIR, file), "utf8");
    const blocks = text.split(/^\[\[rule\]\]\s*$/m).slice(1);
    const key = (block: string, k: string) => new RegExp(`^${k}\\s*=\\s*"([^"]+)"`, "m").exec(block)?.[1];
    for (const b of blocks) {
      const cat = key(b, "category");
      if (cat && cat !== "unknown") cats.set(cat, (cats.get(cat) ?? 0) + 1);
      const tier = key(b, "safety");
      if (tier) tiers[tier] = (tiers[tier] ?? 0) + 1;
      const app = key(b, "app");
      if (app) apps.add(app);
    }
    rules += blocks.length;
    packList.push({ id: key(text, "pack") ?? file.replace(/\.toml$/, ""), rules: blocks.length, description: key(text, "description") ?? "" });
  }
  if (rules === 0) throw new Error("rules: no built-in rules found");
  packList.sort((a, b) => b.rules - a.rules || a.id.localeCompare(b.id));
  const categoryNames = [...cats].sort((a, b) => b[1] - a[1]).map(([c]) => CATEGORY_NAMES[c] ?? c);
  return { rules, packs: packList.length, categories: cats.size, apps: apps.size, tiers, packList, categoryNames };
}

/** A single stacked bar of the safety-tier split, with a legend. */
function tierBar(f: RuleFacts): string {
  const segs = TIERS.map((t) => {
    const n = f.tiers[t.id] ?? 0;
    return `<span class="tierbar__seg tierbar__seg--${t.id}" style="--w:${((n / f.rules) * 100).toFixed(2)}%"></span>`;
  }).join("");
  const legend = TIERS.map(
    (t) =>
      `<li><span class="tierbar__key tierbar__seg--${t.id}" aria-hidden="true"></span><strong>${t.label}</strong><span class="tierbar__n">${f.tiers[t.id] ?? 0}</span><span class="tierbar__desc">${t.desc}</span></li>`,
  ).join("");
  return `<figure class="tierbar"><div class="tierbar__bar" role="img" aria-label="Safety tiers of the ${f.rules} built-in rules: ${TIERS.map((t) => `${f.tiers[t.id] ?? 0} ${t.label.toLowerCase()}`).join(", ")}">${segs}</div><ul class="tierbar__legend">${legend}</ul></figure>`;
}

function packTable(f: RuleFacts): string {
  const rows = f.packList.map((p) => `<li><code>${esc(p.id)}</code><span class="packs__n">${p.rules}</span><span class="packs__desc">${esc(p.description)}</span></li>`).join("");
  return `<ul class="packs">${rows}</ul>`;
}

// -----------------------------------------------------------------------------
// FAQ
// -----------------------------------------------------------------------------

/**
 * The FAQ as an accordion of native `<details>` elements, one per `##`
 * question, each with the anchor GitHub would give it so links into
 * `docs/FAQ.md` work unchanged on the site. `data-text` holds the plain text
 * the page's filter searches.
 */
function faqList(base: string): { html: string; count: number } {
  const resolve = resolver("docs", base);
  const qs = sections(read("docs/FAQ.md"));
  if (qs.length === 0) throw new Error("docs/FAQ.md: no questions found");
  const html = qs
    .map(
      (q) => `<details class="qa" id="${esc(q.slug)}" data-qa data-text="${esc(`${q.title} ${plain(q.body)}`.toLowerCase())}">
  <summary class="qa__q"><span class="qa__title">${inline(q.title, { resolve })}</span><span class="qa__icon" aria-hidden="true"></span></summary>
  <div class="qa__a prose">${markdown(q.body, { resolve })}<p class="qa__tools"><a class="qa__link" href="#${esc(q.slug)}" data-qa-link>Link to this answer</a></p></div>
</details>`,
    )
    .join("");
  return { html, count: qs.length };
}

// -----------------------------------------------------------------------------
// Requirements, privacy and security documents
// -----------------------------------------------------------------------------

/** System requirements as `[item, requirement]` pairs. */
function requirementRows(): Map<string, string> {
  const md = read("docs/REQUIREMENTS.md");
  const t = parseTable(section("docs/REQUIREMENTS.md", md, "System requirements"));
  if (!t) throw new Error("docs/REQUIREMENTS.md: system requirements table not found");
  return new Map(t.rows.map((r) => [r[0] ?? "", plain(r[1] ?? "")]));
}

/** HTML and text fragments keyed by placeholder name (`<!--data:KEY-->` or `{{KEY}}`). */
export type ContentHtml = Record<string, string>;

/**
 * Renders every repository-derived fragment.
 *
 * @param base - The site's base path, e.g. `/STRATA/`.
 * @returns Fragments keyed by placeholder name.
 */
export function renderContent(base: string): ContentHtml {
  const docs = resolver("docs", base);
  const root = resolver("", base);
  const rules = loadRuleFacts();
  const faq = faqList(base);
  const req = requirementRows();
  const privacy = read("docs/PRIVACY.md");
  const requirements = read("docs/REQUIREMENTS.md");
  const security = read("SECURITY.md");

  const admin = parseTable(section("docs/REQUIREMENTS.md", requirements, "Administrator rights"));
  const network = parseTable(section("docs/PRIVACY.md", privacy, "Network access"));
  const stored = parseTable(section("docs/PRIVACY.md", privacy, "What is stored, and where"));
  if (!admin || !network || !stored) throw new Error("docs: a table the security page shows is missing");
  // NOTE: the network section's prose after the table (Report an issue, no other requests) is kept.
  const networkProse = section("docs/PRIVACY.md", privacy, "Network access").split(/\r?\n/).filter((l) => !l.trim().startsWith("|")).join("\n");

  return {
    "rules.count": String(rules.rules),
    "rules.packs": String(rules.packs),
    "rules.categories": String(rules.categories),
    "rules.apps": String(rules.apps),
    "rules.never": String(rules.tiers.never ?? 0),
    "rules.tierbar": tierBar(rules),
    "rules.packlist": packTable(rules),
    "rules.categorylist": rules.categoryNames.map((c) => `<li>${esc(c)}</li>`).join(""),
    "faq.list": faq.html,
    "faq.count": String(faq.count),
    "req.os": esc(req.get("OS") ?? "Windows 10 22H2 or Windows 11"),
    "req.arch": esc(req.get("Architecture") ?? "x64 or ARM64"),
    "req.admin": renderTable(admin, { resolve: docs }, "table table--wide"),
    "privacy.network": renderTable(network, { resolve: docs }, "table table--wide"),
    "privacy.networkNote": markdown(networkProse, { resolve: docs }),
    "privacy.stored": renderTable(stored, { resolve: docs }, "table table--wide"),
    "security.scope": markdown(section("SECURITY.md", security, "Scope"), { resolve: root }),
    "security.report": markdown(section("SECURITY.md", security, "Reporting a vulnerability"), { resolve: root }),
  };
}
