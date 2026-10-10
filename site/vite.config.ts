/**
 * Build for the project website (GitHub Pages, served under `/STRATA/`).
 *
 * The pages are static HTML with a little vanilla TypeScript. Content that
 * lives elsewhere is spliced in at build time:
 * - shared chrome from `partials/` (`<!--site:header-->`, `<!--site:footer-->`);
 * - icons from `build/icons.ts` (`<!--icon:*-->`);
 * - releases and repository stats from the GitHub API (`<!--rel:*-->`,
 *   `<!--gh:*-->`), fetched once per build, with offline fallbacks;
 * - FAQ, rule-pack facts and document tables from the repository
 *   (`<!--data:*-->`, `{{key}}`);
 * - benchmark figures from `data/benchmarks.json` (`<!--bench:*-->`, `{{bench.*}}`).
 *
 * The demo is the app: `ui/vite.demo.config.ts` builds the real app shell over
 * sample data, and {@link strataDemo} builds it into `dist/demo/` (served at
 * `/STRATA/demo/` in development), where the landing page embeds it in an iframe.
 */
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join, resolve } from "node:path";
import { build, createServer, defineConfig, type Plugin } from "vite";
import { benchmarkFacts, renderBenchmarks } from "./build/benchmarks.ts";
import { CONTENT_FILES, RULES_DIR, renderContent } from "./build/content.ts";
import { compactCount, loadRepoStats, REPO_URL, type RepoStats } from "./build/github.ts";
import { inlineIcons } from "./build/icons.ts";
import { loadReleases, renderReleases, type Release } from "./build/releases.ts";

const benchmarks = fileURLToPath(new URL("./data/benchmarks.json", import.meta.url));
const demoConfig = fileURLToPath(new URL("../ui/vite.demo.config.ts", import.meta.url));
const partials = fileURLToPath(new URL("./partials/", import.meta.url));
const page = (path: string) => fileURLToPath(new URL(path, import.meta.url));

/** Site pages: Rollup input name to HTML entry. The name is also the `data-page` key in the header. */
const PAGES = {
  home: page("./index.html"),
  features: page("./features/index.html"),
  security: page("./security/index.html"),
  faq: page("./faq/index.html"),
  releases: page("./releases/index.html"),
} as const;

const pageOf = (path: string): keyof typeof PAGES => {
  const m = /\/(features|security|faq|releases)\//.exec(path);
  return (m?.[1] as keyof typeof PAGES | undefined) ?? "home";
};

/** The GitHub button's star count, or nothing when GitHub was unreachable or there are none yet. */
function starsHtml(stats: RepoStats | null): string {
  if (!stats || stats.stars === 0) return "";
  const label = `${stats.stars.toLocaleString("en-US")} ${stats.stars === 1 ? "star" : "stars"}`;
  return `<span class="gh__stars"><!--icon:star--><span aria-hidden="true">${compactCount(stats.stars)}</span><span class="visually-hidden">, ${label}</span></span>`;
}

/**
 * Shared page chrome and build-time content. Network data is fetched once per
 * build and shared by every page; repository files are re-read per page so the
 * dev server reflects edits.
 */
function siteHtml(): Plugin {
  let base = "/";
  let releases: Promise<Release[] | null> | undefined;
  let stats: Promise<RepoStats | null> | undefined;
  return {
    name: "strata-site",
    configResolved(config) {
      base = config.base;
    },
    configureServer(server) {
      server.watcher.add([...CONTENT_FILES, RULES_DIR, partials]);
      server.watcher.on("change", (file) => {
        if (CONTENT_FILES.includes(file) || file.startsWith(RULES_DIR) || file.startsWith(partials)) server.ws.send({ type: "full-reload" });
      });
    },
    async transformIndexHtml(html, ctx) {
      const name = pageOf(ctx.path);
      const partial = (p: string) => readFileSync(join(partials, `${p}.html`), "utf8").trimEnd();
      const header = partial("header")
        .replaceAll(`data-page="${name}"`, 'aria-current="page"')
        .replace(/ data-page="\w+"/g, "");
      releases ??= loadReleases();
      stats ??= loadRepoStats();
      const rel = renderReleases(await releases, base);
      const text: Record<string, string> = {
        ...renderContent(base),
        ...benchmarkFacts(benchmarks),
        base,
        repo: REPO_URL,
        version: rel.version || "&lt;version&gt;",
        "release.short": rel.version ? `v${rel.version}` : "Releases",
        "release.label": rel.version ? "What’s new" : "Changelog",
      };
      const filled = inlineIcons(
        html
          .replace("<!--site:header-->", header.replaceAll("\n", "\n    "))
          .replace("<!--site:footer-->", partial("footer").replaceAll("\n", "\n    "))
          .replace("<!--gh:stars-->", starsHtml(await stats)),
      );
      // NOTE: fragments may contain icon placeholders, so icons are inlined once more afterwards.
      return inlineIcons(
        filled
          .replace(/<!--rel:(\w+)-->/g, (m, key: string) => (key in rel ? rel[key as keyof typeof rel] : m))
          .replace(/<!--data:([\w.-]+)-->/g, (m, key: string) => text[key] ?? m)
          .replace(/\{\{([\w.-]+)\}\}/g, (_, key: string) => {
            const v = text[key];
            if (v === undefined) throw new Error(`${ctx.path}: unknown placeholder {{${key}}}`);
            return v;
          }),
      );
    },
  };
}

/** Splices the rendered benchmark data into `<!--bench:*-->` placeholders. */
function benchmarkHtml(): Plugin {
  return {
    name: "strata-benchmarks",
    configureServer(server) {
      server.watcher.add(benchmarks);
      server.watcher.on("change", (file) => {
        if (file === benchmarks) server.ws.send({ type: "full-reload" });
      });
    },
    transformIndexHtml(html) {
      if (!html.includes("<!--bench:")) return html;
      const parts = renderBenchmarks(benchmarks);
      return html.replace(/<!--bench:(\w+)-->/g, (m, key: string) => (key in parts ? parts[key as keyof typeof parts] : m));
    },
  };
}

/**
 * Adds a strict Content Security Policy to every built page.
 *
 * SECURITY: scripts are limited to the site's own bundle plus the exact
 * inline snippets present at build time, pinned by SHA-256. The only remote
 * origin is api.github.com, for the live release check. `style-src` allows
 * inline styles because charts position their marks with `style="--x:…"`
 * custom properties. Development builds skip it, since Vite's client needs
 * inline modules and a WebSocket.
 */
function contentSecurityPolicy(): Plugin {
  let isBuild = false;
  return {
    name: "strata-csp",
    configResolved(config) {
      isBuild = config.command === "build";
    },
    transformIndexHtml: {
      order: "post",
      handler(html) {
        if (!isBuild) return html;
        const hashes = [...html.matchAll(/<script(?![^>]*\bsrc=)[^>]*>([\s\S]*?)<\/script>/g)].map(
          (m) => `'sha256-${createHash("sha256").update(m[1] ?? "").digest("base64")}'`,
        );
        const policy = [
          "default-src 'self'",
          `script-src 'self' ${hashes.join(" ")}`.trim(),
          "style-src 'self' 'unsafe-inline'",
          "img-src 'self' data:",
          "font-src 'self'",
          "connect-src 'self' https://api.github.com",
          "frame-src 'self'",
          "object-src 'none'",
          "base-uri 'self'",
          "form-action 'none'",
        ].join("; ");
        return html.replace(/<meta charset="utf-8" \/>/, (m) => `${m}\n    <meta http-equiv="Content-Security-Policy" content="${policy}" />`);
      },
    },
  };
}

/**
 * Builds the app demo into `<outDir>/demo/` after the site build, so one
 * `vite build` produces the whole Pages artifact; in development, serves it
 * from the demo's own Vite server mounted at `<base>demo/`.
 */
function strataDemo(): Plugin {
  let outDir = "";
  let base = "/";
  return {
    name: "strata-demo",
    configResolved(config) {
      outDir = resolve(config.root, config.build.outDir);
      base = config.base;
    },
    async configureServer(server) {
      const demo = await createServer({
        configFile: demoConfig,
        base: `${base}demo/`,
        server: { middlewareMode: true, hmr: false, ws: false },
        appType: "spa",
      });
      server.middlewares.use(demo.middlewares);
      server.httpServer?.once("close", () => {
        void demo.close();
      });
    },
    async closeBundle() {
      await build({ configFile: demoConfig, build: { outDir: join(outDir, "demo"), emptyOutDir: true }, logLevel: "warn" });
    },
  };
}

export default defineConfig({
  base: "/STRATA/",
  plugins: [siteHtml(), benchmarkHtml(), contentSecurityPolicy(), strataDemo()],
  build: {
    target: "es2022",
    sourcemap: false,
    assetsInlineLimit: 0,
    rollupOptions: {
      input: { main: PAGES.home, features: PAGES.features, security: PAGES.security, faq: PAGES.faq, releases: PAGES.releases },
    },
  },
});
