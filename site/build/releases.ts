/**
 * Build-time rendering of release downloads and notes from the public GitHub
 * REST API. `vite.config.ts` calls {@link loadReleases} once per build and
 * splices {@link renderReleases} fragments into `<!--rel:*-->` placeholders,
 * so download links, sizes, checksums and notes ship as static markup.
 *
 * Responsibilities:
 * - Fetch published releases (drafts never appear in the public API).
 * - Read the latest release's `SHA256SUMS.txt` for the checksum list.
 * - Render the landing-page download block and the releases page list.
 * - Fall back to plain links to GitHub Releases when the API is unreachable,
 *   so a build never fails and visitors can always download.
 */

const REPO = "BankkRoll/STRATA";
const API = `https://api.github.com/repos/${REPO}/releases?per_page=50`;
/** Always resolves to the newest published release on GitHub. */
export const LATEST_URL = `https://github.com/${REPO}/releases/latest`;
const ALL_URL = `https://github.com/${REPO}/releases`;

/** Installer architectures, in display order. */
const ARCHES = [
  { id: "x64", label: "x64", desc: "Most Windows PCs, with Intel or AMD processors." },
  { id: "arm64", label: "ARM64", desc: "Windows on ARM, such as Snapdragon laptops." },
] as const;

type ArchId = (typeof ARCHES)[number]["id"];

/** One downloadable file of a release. */
export interface ReleaseAsset {
  name: string;
  /** Size in bytes. */
  size: number;
  /** Direct download URL on GitHub. */
  url: string;
}

/** A published release, reduced to what the site shows. */
export interface Release {
  /** Git tag, e.g. `v0.1.0`. */
  tag: string;
  /** Version without the leading `v`. */
  version: string;
  /** ISO publish date. */
  date: string;
  prerelease: boolean;
  /** Release page on GitHub. */
  url: string;
  /** Release notes as Markdown. */
  notes: string;
  assets: ReleaseAsset[];
  /** File name to SHA-256, from `SHA256SUMS.txt`; filled for the latest release only. */
  sums: Map<string, string>;
}

interface ApiAsset {
  name: string;
  size: number;
  browser_download_url: string;
}

interface ApiRelease {
  tag_name: string;
  published_at: string | null;
  prerelease: boolean;
  draft: boolean;
  html_url: string;
  body: string | null;
  assets: ApiAsset[];
}

/**
 * Fetches published releases, newest first.
 *
 * SECURITY: `GITHUB_TOKEN` (set by the Pages workflow to lift the anonymous
 * rate limit) is sent only to api.github.com, never to download hosts.
 *
 * @returns The releases, or `null` when GitHub could not be reached.
 */
export async function loadReleases(): Promise<Release[] | null> {
  const headers: Record<string, string> = { Accept: "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28" };
  if (process.env.GITHUB_TOKEN) headers.Authorization = `Bearer ${process.env.GITHUB_TOKEN}`;
  let raw: ApiRelease[];
  try {
    const res = await fetch(API, { headers, signal: AbortSignal.timeout(15_000) });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    raw = (await res.json()) as ApiRelease[];
  } catch (e) {
    console.warn(`releases: GitHub API unavailable, falling back to links (${String(e)})`);
    return null;
  }
  const releases: Release[] = raw
    // NOTE: only version tags; `updater-beta` is the beta update channel, not a release.
    .filter((r) => !r.draft && r.published_at && /^v\d/.test(r.tag_name))
    .map((r) => ({
      tag: r.tag_name,
      version: r.tag_name.replace(/^v/, ""),
      date: r.published_at ?? "",
      prerelease: r.prerelease,
      url: r.html_url,
      notes: r.body ?? "",
      assets: r.assets.map((a) => ({ name: a.name, size: a.size, url: a.browser_download_url })),
      sums: new Map(),
    }));
  const latest = releases.find((r) => !r.prerelease);
  const sumsFile = latest?.assets.find((a) => a.name === "SHA256SUMS.txt");
  if (latest && sumsFile) {
    try {
      const res = await fetch(sumsFile.url, { signal: AbortSignal.timeout(15_000) });
      if (res.ok) {
        for (const line of (await res.text()).split(/\r?\n/)) {
          const m = /^([0-9a-f]{64})\s+\*?(.+)$/i.exec(line.trim());
          if (m?.[1] && m[2]) latest.sums.set(m[2], m[1].toLowerCase());
        }
      }
    } catch {
      // NOTE: checksums are optional; the block is left out without them.
    }
  }
  return releases;
}

// -----------------------------------------------------------------------------
// Formatting
// -----------------------------------------------------------------------------

const esc = (s: string): string => s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

const date = (iso: string): string =>
  new Date(iso).toLocaleDateString("en-US", { year: "numeric", month: "long", day: "numeric", timeZone: "UTC" });

const size = (bytes: number): string =>
  bytes >= 1024 * 1024 ? `${(bytes / (1024 * 1024)).toFixed(1)} MB` : `${Math.max(1, Math.round(bytes / 1024))} KB`;

const installer = (r: Release, arch: ArchId): ReleaseAsset | undefined =>
  r.assets.find((a) => a.name.toLowerCase().endsWith(`_${arch}-setup.exe`));

/** Inline Markdown: code spans, bold and http(s) links, on escaped text. */
function inline(text: string): string {
  return esc(text)
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g, '<a class="link" href="$2">$1</a>');
}

/**
 * Renders the subset of Markdown the changelog uses: `###` headings, `-`
 * bullets with indented continuation lines, and paragraphs.
 */
function markdown(md: string): string {
  const out: string[] = [];
  let list: string[] | null = null;
  let para: string[] = [];
  const flushPara = () => {
    if (para.length) out.push(`<p>${inline(para.join(" "))}</p>`);
    para = [];
  };
  const flushList = () => {
    if (list) out.push(`<ul>${list.map((li) => `<li>${inline(li)}</li>`).join("")}</ul>`);
    list = null;
  };
  for (const line of md.split(/\r?\n/)) {
    const heading = /^#{1,6}\s+(.*)$/.exec(line);
    const bullet = /^[-*]\s+(.*)$/.exec(line);
    if (heading) {
      flushPara();
      flushList();
      out.push(`<h3>${inline(heading[1] ?? "")}</h3>`);
    } else if (bullet) {
      flushPara();
      (list ??= []).push(bullet[1] ?? "");
    } else if (list && /^\s+\S/.test(line)) {
      list[list.length - 1] += ` ${line.trim()}`;
    } else if (line.trim() === "") {
      flushPara();
      flushList();
    } else {
      flushList();
      para.push(line.trim());
    }
  }
  flushPara();
  flushList();
  return out.join("");
}

const GITHUB_ICON =
  '<svg viewBox="0 0 16 16" aria-hidden="true"><path fill="currentColor" d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27s1.36.09 2 .27c1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0 0 16 8c0-4.42-3.58-8-8-8Z" /></svg>';

const DOWNLOAD_ICON =
  '<svg viewBox="0 0 20 20" aria-hidden="true"><path d="M10 3.5v9m0 0-3.5-3.5M10 12.5l3.5-3.5M4.5 16h11" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" /></svg>';

const githubButton = (href: string, label: string): string =>
  `<a class="btn btn--quiet btn--sm" href="${esc(href)}">${GITHUB_ICON}<span>${esc(label)}</span></a>`;

// -----------------------------------------------------------------------------
// Fragments
// -----------------------------------------------------------------------------

/** HTML fragments keyed by placeholder name (`<!--rel:KEY-->`). */
export interface ReleaseHtml {
  /** Landing-page download block. */
  download: string;
  /** Releases page list. */
  list: string;
}

/**
 * The landing-page download block: one card per installer, the version line,
 * links to the release notes and GitHub, and the checksums.
 *
 * `data-*` hooks let the page script mark the visitor's architecture and swap
 * in a newer release published after this build.
 */
function downloadBlock(latest: Release | undefined, base: string): string {
  const cards = ARCHES.map((a) => {
    const file = latest && installer(latest, a.id);
    const href = file?.url ?? LATEST_URL;
    const meta = file ? `${file.name} · ${size(file.size)}` : "From GitHub Releases";
    return `<article class="dl-card" data-arch="${a.id}">
  <div class="dl-card__head"><h3 class="dl-card__arch">${a.label}</h3><span class="dl-card__tag" data-recommend hidden>This PC</span></div>
  <p class="dl-card__desc">${a.desc}</p>
  <a class="btn btn--primary btn--lg" href="${esc(href)}" data-asset="${a.id}">${DOWNLOAD_ICON}Download for ${a.label}</a>
  <p class="meta" data-asset-meta="${a.id}">${esc(meta)}</p>
</article>`;
  }).join("");
  const version = latest
    ? `<p class="meta" data-release-line>Version ${esc(latest.version)} · ${date(latest.date)} · Windows 10 / 11</p>`
    : `<p class="meta" data-release-line>Windows 10 / 11</p>`;
  const sums =
    latest && latest.sums.size > 0
      ? `<details class="disclosure dl__sums" data-sums>
  <summary>SHA-256 checksums<span class="disclosure__icon" aria-hidden="true"></span></summary>
  <dl class="sums">${[...latest.sums]
    .filter(([name]) => /\.exe$/i.test(name))
    .map(([name, hash]) => `<dt>${esc(name)}</dt><dd><code>${hash}</code></dd>`)
    .join("")}</dl>
</details>`
      : "";
  return `<div class="dl" data-release data-tag="${esc(latest?.tag ?? "")}">
  <div class="dl__grid">${cards}</div>
  <div class="dl__foot">
    ${version}
    <div class="dl__links">
      <a class="link" href="${base}releases/">Release notes and earlier versions</a>
      ${githubButton(LATEST_URL, "Release on GitHub")}
    </div>
  </div>
  <p class="dl__note">The installers aren’t code-signed yet, so Windows SmartScreen may ask you to confirm. Installed copies update themselves.</p>
  ${sums}
</div>`;
}

/** Installers first, in {@link ARCHES} order, then every other file. */
const rank = (name: string): number => {
  const i = ARCHES.findIndex((a) => name.toLowerCase().endsWith(`_${a.id}-setup.exe`));
  return i < 0 ? ARCHES.length : i;
};

/** One release on the releases page: notes beside its files. */
function releaseEntry(r: Release, isLatest: boolean): string {
  const files = r.assets
    .filter((a) => !a.name.endsWith(".sig") && a.name !== "latest.json")
    .sort((a, b) => rank(a.name) - rank(b.name) || a.name.localeCompare(b.name))
    .map((a) => `<li><a class="link" href="${esc(a.url)}">${esc(a.name)}</a><span class="meta">${size(a.size)}</span></li>`)
    .join("");
  const badge = isLatest ? '<span class="rel__badge">Latest</span>' : r.prerelease ? '<span class="rel__badge rel__badge--pre">Pre-release</span>' : "";
  return `<article class="rel" id="${esc(r.tag)}">
  <header class="rel__head">
    <h2 class="rel__title"><a href="#${esc(r.tag)}">${esc(r.version)}</a></h2>${badge}
    <p class="meta">${date(r.date)}</p>
  </header>
  <div class="rel__body">
    <div class="rel__notes prose">${markdown(r.notes) || "<p>No release notes.</p>"}</div>
    <aside class="rel__files" aria-label="Downloads for ${esc(r.version)}">
      <h3 class="rel__sub">Downloads</h3>
      ${files ? `<ul class="files">${files}</ul>` : ""}
      ${githubButton(r.url, "View on GitHub")}
    </aside>
  </div>
</article>`;
}

/**
 * Renders every `<!--rel:*-->` fragment.
 *
 * @param releases - Result of {@link loadReleases}; `null` renders link-only fallbacks.
 * @param base - The site's base path, e.g. `/STRATA/`.
 * @returns HTML fragments keyed by placeholder name.
 */
export function renderReleases(releases: Release[] | null, base: string): ReleaseHtml {
  const latest = releases?.find((r) => !r.prerelease);
  const list =
    releases && releases.length > 0
      ? releases.map((r) => releaseEntry(r, r === latest)).join("")
      : `<p class="rel__empty">Release notes are on GitHub. ${githubButton(ALL_URL, "All releases on GitHub")}</p>`;
  return { download: downloadBlock(latest, base), list };
}
