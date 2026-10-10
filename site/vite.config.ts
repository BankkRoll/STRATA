/**
 * Build for the project website (GitHub Pages, served under `/STRATA/`).
 *
 * The page itself is static HTML with a little vanilla TypeScript. The demo
 * is the app: `ui/vite.demo.config.ts` builds the real app shell over sample
 * data, and {@link strataDemo} builds it into `dist/demo/` (served at
 * `/STRATA/demo/` in development), where the page embeds it in an iframe.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join, resolve } from "node:path";
import { build, createServer, defineConfig, type Plugin } from "vite";
import { renderBenchmarks } from "./build/benchmarks.ts";
import { loadReleases, renderReleases, type Release } from "./build/releases.ts";

const benchmarks = fileURLToPath(new URL("./data/benchmarks.json", import.meta.url));
const demoConfig = fileURLToPath(new URL("../ui/vite.demo.config.ts", import.meta.url));
const partials = fileURLToPath(new URL("./partials/", import.meta.url));

/**
 * Shared page chrome and release data: `<!--site:header-->` and
 * `<!--site:footer-->` come from `partials/` with `{{base}}` filled in, and
 * `<!--rel:*-->` from the GitHub releases API, fetched once per build.
 */
function siteHtml(): Plugin {
  let base = "/";
  let releases: Promise<Release[] | null> | undefined;
  return {
    name: "strata-site",
    configResolved(config) {
      base = config.base;
    },
    async transformIndexHtml(html, ctx) {
      const partial = (name: string) => readFileSync(join(partials, `${name}.html`), "utf8").replaceAll("{{base}}", base).trimEnd();
      const page = ctx.path.includes("/releases/") ? "releases" : "home";
      const header = partial("header").replace(`data-page="${page}"`, 'aria-current="page"');
      releases ??= loadReleases();
      const rel = renderReleases(await releases, base);
      return html
        .replace("<!--site:header-->", header.replaceAll("\n", "\n    "))
        .replace("<!--site:footer-->", partial("footer").replaceAll("\n", "\n    "))
        .replace(/<!--rel:(\w+)-->/g, (m, key: string) => (key in rel ? rel[key as keyof typeof rel] : m));
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
      const parts = renderBenchmarks(benchmarks);
      return html.replace(/<!--bench:(\w+)-->/g, (m, key: string) => (key in parts ? parts[key as keyof typeof parts] : m));
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
  plugins: [siteHtml(), benchmarkHtml(), strataDemo()],
  build: {
    target: "es2022",
    sourcemap: false,
    assetsInlineLimit: 0,
    rollupOptions: {
      input: {
        main: fileURLToPath(new URL("./index.html", import.meta.url)),
        releases: fileURLToPath(new URL("./releases/index.html", import.meta.url)),
      },
    },
  },
});
