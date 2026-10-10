/**
 * Build-time rendering of release downloads and notes from the public GitHub
 * REST API. `vite.config.ts` calls {@link loadReleases} once per build and
 * splices {@link renderReleases} fragments into `<!--rel:*-->` placeholders,
 * so download links, sizes, checksums and notes ship as static markup.
 *
 * Responsibilities:
 * - Fetch published releases (drafts never appear in the public API).
 * - Read the latest release's `SHA256SUMS.txt` for the checksum list.
 * - Render the hero download button, the "what's new" pill, the landing-page
 *   download block and the releases page.
 * - Fall back to plain links to GitHub Releases when the API is unreachable,
 *   so a build never fails and visitors can always download.
 */
import { githubJson, REPO, REPO_URL } from "./github.ts";
import { ARROW_ICON, DOWNLOAD_ICON, GITHUB_ICON } from "./icons.ts";
import { esc, markdown } from "./markdown.ts";

/** Always resolves to the newest published release on GitHub. */
export const LATEST_URL = `${REPO_URL}/releases/latest`;
const ALL_URL = `${REPO_URL}/releases`;

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

interface ApiRelease {
  tag_name: string;
  published_at: string | null;
  prerelease: boolean;
  draft: boolean;
  html_url: string;
  body: string | null;
  assets: { name: string; size: number; browser_download_url: string }[];
}

/**
 * Fetches published releases, newest first, and the latest release's
 * checksums.
 *
 * SECURITY: the checksum file is fetched from GitHub's download host without
 * credentials; the API token stays with api.github.com (see `github.ts`).
 *
 * @returns The releases, or `null` when GitHub could not be reached.
 */
export async function loadReleases(): Promise<Release[] | null> {
  const raw = await githubJson<ApiRelease[]>(`/repos/${REPO}/releases?per_page=50`);
  if (!Array.isArray(raw)) return null;
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

const date = (iso: string): string =>
  new Date(iso).toLocaleDateString("en-US", { year: "numeric", month: "long", day: "numeric", timeZone: "UTC" });

const size = (bytes: number): string =>
  bytes >= 1024 * 1024 ? `${(bytes / (1024 * 1024)).toFixed(1)} MB` : `${Math.max(1, Math.round(bytes / 1024))} KB`;

const installer = (r: Release, arch: ArchId): ReleaseAsset | undefined =>
  r.assets.find((a) => a.name.toLowerCase().endsWith(`_${arch}-setup.exe`));

const githubButton = (href: string, label: string): string =>
  `<a class="btn btn--quiet btn--sm" href="${esc(href)}">${GITHUB_ICON}<span>${esc(label)}</span></a>`;

/** A checksum with a copy button; the page script wires `data-copy`. */
const hashRow = (name: string, hash: string): string =>
  `<dt>${esc(name)}</dt><dd><code>${hash}</code><button class="copy" type="button" data-copy="${hash}" aria-label="Copy the SHA-256 of ${esc(name)}">Copy</button></dd>`;

const sumsList = (r: Release): string => {
  const rows = [...r.sums].filter(([name]) => /\.exe$/i.test(name));
  return rows.length ? `<dl class="sums">${rows.map(([n, h]) => hashRow(n, h)).join("")}</dl>` : "";
};

// -----------------------------------------------------------------------------
// Fragments
// -----------------------------------------------------------------------------

/** HTML fragments keyed by placeholder name (`<!--rel:KEY-->`). */
export interface ReleaseHtml {
  /** Hero download button, aimed at the visitor's architecture by the page script. */
  hero: string;
  /** "What's new" pill linking to the latest release notes. */
  pill: string;
  /** Landing-page download block. */
  download: string;
  /** Releases page summary of the latest release. */
  latest: string;
  /** Releases page list. */
  list: string;
  /** Latest version without the `v`, or empty when unknown. */
  version: string;
}

/**
 * The hero button. It links to the x64 installer, which most PCs need; the
 * page script switches it to ARM64 when the browser reports an ARM PC.
 * `data-*` carry both targets so the switch needs no request.
 */
function heroBlock(latest: Release | undefined, base: string): string {
  const files = ARCHES.map((a) => ({ arch: a, file: latest && installer(latest, a.id) }));
  const attrs = files
    .map(({ arch, file }) => (file ? ` data-href-${arch.id}="${esc(file.url)}" data-meta-${arch.id}="${esc(`${latest?.version ?? ""} · ${arch.label} · ${size(file.size)}`)}"` : ""))
    .join("");
  const x64 = files[0]?.file;
  const meta = latest && x64 ? `${latest.version} · x64 · ${size(x64.size)}` : "x64 and ARM64";
  return `<div class="hero-dl" data-hero-dl${attrs}>
  <a class="btn btn--primary btn--lg" href="${esc(x64?.url ?? LATEST_URL)}" data-hero-link>${DOWNLOAD_ICON}Download for Windows</a>
  <p class="hero-dl__meta"><span data-hero-meta>${esc(meta)}</span><a class="hero-dl__other" href="${base}#download">Other downloads</a></p>
</div>`;
}

function pillBlock(latest: Release | undefined, base: string): string {
  const href = latest ? `${base}releases/#${esc(latest.tag)}` : `${base}releases/`;
  const text = latest ? `What’s new in ${esc(latest.version)}` : "Read the changelog";
  return `<a class="pill" href="${href}" data-pill><span class="pill__tag">New</span><span data-pill-text>${text}</span>${ARROW_ICON}</a>`;
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
  const list = latest ? sumsList(latest) : "";
  const sums = list
    ? `<details class="disclosure dl__sums" data-sums>
  <summary>SHA-256 checksums<span class="disclosure__icon" aria-hidden="true"></span></summary>
  <div class="disclosure__body">${list}<p class="dl__note">Check a download in PowerShell with <code>Get-FileHash .\\Strata_${esc(latest?.version ?? "")}_x64-setup.exe</code> and compare the hash.</p></div>
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
  <p class="dl__note">The installers aren’t code-signed yet, so Windows SmartScreen may ask you to confirm. Installed copies update themselves, and every update is verified against the project’s signing key.</p>
  ${sums}
</div>`;
}

function latestBlock(latest: Release | undefined): string {
  if (!latest) return `<div class="rel-latest"><p class="rel-latest__line">Installers and notes for every version are on GitHub.</p>${githubButton(ALL_URL, "All releases on GitHub")}</div>`;
  const buttons = ARCHES.map((a) => {
    const file = installer(latest, a.id);
    return file
      ? `<a class="btn btn--quiet btn--sm" href="${esc(file.url)}" data-asset="${a.id}">${DOWNLOAD_ICON}<span>${a.label}</span><span class="btn__meta">${size(file.size)}</span></a>`
      : "";
  }).join("");
  return `<div class="rel-latest">
  <p class="rel-latest__line"><span class="rel__badge">Latest</span><a class="rel-latest__ver" href="#${esc(latest.tag)}">${esc(latest.version)}</a><span class="meta">${date(latest.date)}</span></p>
  <div class="actions">${buttons}</div>
</div>`;
}

/** Installers first, in {@link ARCHES} order, then every other file. */
const rank = (name: string): number => {
  const i = ARCHES.findIndex((a) => name.toLowerCase().endsWith(`_${a.id}-setup.exe`));
  return i < 0 ? ARCHES.length : i;
};

/** One release on the releases page: its version rail, notes and files. */
function releaseEntry(r: Release, isLatest: boolean): string {
  const files = r.assets
    .filter((a) => !a.name.endsWith(".sig") && a.name !== "latest.json")
    .sort((a, b) => rank(a.name) - rank(b.name) || a.name.localeCompare(b.name))
    .map((a) => `<li><a class="link" href="${esc(a.url)}">${esc(a.name)}</a><span class="meta">${size(a.size)}</span></li>`)
    .join("");
  const badge = isLatest ? '<span class="rel__badge">Latest</span>' : r.prerelease ? '<span class="rel__badge rel__badge--pre">Pre-release</span>' : "";
  const sums = sumsList(r);
  return `<article class="rel" id="${esc(r.tag)}" aria-labelledby="${esc(r.tag)}-title">
  <header class="rel__head">
    <h2 class="rel__title" id="${esc(r.tag)}-title"><a href="#${esc(r.tag)}">${esc(r.version)}</a></h2>${badge}
    <p class="meta"><time datetime="${esc(r.date)}">${date(r.date)}</time></p>
  </header>
  <div class="rel__body">
    <div class="rel__notes prose">${markdown(r.notes) || "<p>No release notes.</p>"}</div>
    <aside class="rel__files" aria-label="Downloads for ${esc(r.version)}">
      <h3 class="rel__sub">Downloads</h3>
      ${files ? `<ul class="files">${files}</ul>` : ""}
      ${sums ? `<h3 class="rel__sub">SHA-256</h3>${sums}` : ""}
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
  return {
    hero: heroBlock(latest, base),
    pill: pillBlock(latest, base),
    download: downloadBlock(latest, base),
    latest: latestBlock(latest),
    list,
    version: latest ? esc(latest.version) : "",
  };
}
