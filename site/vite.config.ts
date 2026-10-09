/**
 * Build for the project website (GitHub Pages, served under `/STRATA/`).
 *
 * Why vanilla TypeScript and not React: the app's WebGL renderers, layout
 * frame decoder, picker and label overlay all sit behind `ViewController`,
 * which is framework-free. The demo only needs that controller plus a little
 * chrome (tooltip, breadcrumbs, legend, detail strip), so pulling in React
 * would add ~60 kB gzip to a page whose other sections are static HTML.
 *
 * `@ui` points at `../ui/src` so the demo imports the product's renderer and
 * decoders directly; nothing is copied. Their bare imports (`zustand`) resolve
 * from `ui/node_modules`, which the workspace install provides.
 */
import { fileURLToPath } from "node:url";
import { defineConfig, type Plugin } from "vite";
import { renderBenchmarks } from "./build/benchmarks.ts";
import { scopedAppStyles } from "./build/scoped-styles.ts";

const uiSrc = fileURLToPath(new URL("../ui/src", import.meta.url));
const benchmarks = fileURLToPath(new URL("./data/benchmarks.json", import.meta.url));

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

export default defineConfig({
  base: "/STRATA/",
  plugins: [scopedAppStyles(`${uiSrc}/styles.css`), benchmarkHtml()],
  resolve: {
    alias: { "@ui": uiSrc },
  },
  build: {
    target: "es2022",
    // NOTE: the app tokens use light-dark() and the demo styles use @scope;
    // older CSS targets would make the minifier rewrite or drop both.
    cssTarget: ["chrome123", "edge123", "firefox146", "safari17.5"],
    sourcemap: false,
    assetsInlineLimit: 0,
  },
});
