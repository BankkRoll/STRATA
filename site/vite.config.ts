/**
 * Build for the project website (GitHub Pages, served under `/STRATA/`).
 *
 * The page itself is static HTML with a little vanilla TypeScript. The demo
 * is the app: `ui/vite.demo.config.ts` builds the real app shell over sample
 * data, and {@link strataDemo} builds it into `dist/demo/` (served at
 * `/STRATA/demo/` in development), where the page embeds it in an iframe.
 */
import { fileURLToPath } from "node:url";
import { join, resolve } from "node:path";
import { build, createServer, defineConfig, type Plugin } from "vite";
import { renderBenchmarks } from "./build/benchmarks.ts";

const benchmarks = fileURLToPath(new URL("./data/benchmarks.json", import.meta.url));
const demoConfig = fileURLToPath(new URL("../ui/vite.demo.config.ts", import.meta.url));

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
  plugins: [benchmarkHtml(), strataDemo()],
  build: {
    target: "es2022",
    sourcemap: false,
    assetsInlineLimit: 0,
  },
});
