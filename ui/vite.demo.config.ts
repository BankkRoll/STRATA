/**
 * Build of the website demo (`src/demo/`): the real app shell over sample
 * data, embedded by the project website in an iframe. The site build runs
 * this config and writes the output to `site/dist/demo/`; `pnpm demo` serves
 * it on its own.
 *
 * Kept separate from `vite.config.ts` so the app bundle (and its budget
 * check) never contains the demo or the sample data.
 */
import { fileURLToPath } from "node:url";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  root: fileURLToPath(new URL("./src/demo", import.meta.url)),
  // Relative asset URLs, so the build works under any path (`/STRATA/demo/`).
  base: "./",
  publicDir: false,
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 5174,
  },
  build: {
    outDir: fileURLToPath(new URL("./dist-demo", import.meta.url)),
    emptyOutDir: true,
    target: "es2022",
    // NOTE: the app styles use light-dark(); older CSS targets would make the
    // minifier rewrite or drop it.
    cssTarget: ["chrome123", "edge123", "firefox120", "safari17.5"],
    sourcemap: false,
    assetsInlineLimit: 0,
  },
});
