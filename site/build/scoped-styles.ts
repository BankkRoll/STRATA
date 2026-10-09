/**
 * Vite plugin serving the app's stylesheet (`ui/src/styles.css`) as the
 * virtual module `virtual:strata-app.css`, wrapped in `@scope (.strata-app)`.
 *
 * The demo window then uses the product's own tokens and component styles
 * (tooltip, breadcrumbs, segmented controls, legend, tiers) without the app's
 * global rules (`body`, `*`, `:focus-visible`) leaking into the page.
 */
import { readFileSync } from "node:fs";
import type { Plugin } from "vite";

/** Matches one `@keyframes` block, including its nested keyframe selectors. */
const KEYFRAMES = /@keyframes[^{]+\{(?:[^{}]*\{[^{}]*\})*[^{}]*\}/g;

const ID = "virtual:strata-app.css";
const RESOLVED = "\0strata-app.css";

/**
 * Wraps a stylesheet in `@scope (.strata-app)`. `:root` becomes `:scope` so
 * the tokens attach to the demo window; keyframes are hoisted out because
 * they are global by nature.
 *
 * @param css - The app stylesheet.
 * @returns The scoped stylesheet.
 */
export function scopeStyles(css: string): string {
  const keyframes = css.match(KEYFRAMES) ?? [];
  const rules = css.replace(KEYFRAMES, "").replaceAll(":root", ":scope");
  return `${keyframes.join("\n")}\n@scope (.strata-app) {\n${rules}\n}\n`;
}

/**
 * @param file - Absolute path of `ui/src/styles.css`.
 * @returns The plugin.
 */
export function scopedAppStyles(file: string): Plugin {
  return {
    name: "strata-scoped-app-styles",
    enforce: "pre",
    resolveId(source) {
      return source === ID ? RESOLVED : null;
    },
    load(source) {
      if (source !== RESOLVED) return null;
      this.addWatchFile(file);
      return scopeStyles(readFileSync(file, "utf8"));
    },
  };
}
