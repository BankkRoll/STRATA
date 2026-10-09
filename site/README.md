# Strata website

The project site at <https://bankkroll.github.io/STRATA/>: a static Vite + TypeScript page.

The interactive demo is not a recording. It imports the app's WebGL2 renderers, layout-frame
decoder and view controller from `ui/src` (via the `@ui` alias) and replays the layout
fixtures that `strata-layout` exports for the UI tests. The demo window is styled by the app's
own `ui/src/styles.css`, scoped to `.strata-app` at build time.

## Develop

```powershell
pnpm install
pnpm --dir site dev       # http://localhost:5173/STRATA/
pnpm --dir site build     # type-check and build to site/dist
pnpm --dir site preview   # serve the build
```

Append `?nogl` to the URL to see the SVG fallback used when WebGL2 is unavailable.

## Content

- **Benchmarks** come from `data/benchmarks.json` and are rendered into the HTML at build time
  (`build/benchmarks.ts` documents and validates the schema). Every number must trace to
  `docs/BENCHMARKS.md`. The head-to-head block appears only once
  `comparison.results` has entries; publishing a head-to-head is a data-only change: paste the
  `comparison` block that `bench/verify-elevated.ps1` prints.
- **Social image:** `public/og.png` is rendered from `build/og.svg` with a headless browser
  screenshot at 1200×630.

## Deploy

`.github/workflows/pages.yml` builds and deploys to GitHub Pages on every push to `main` that
touches `site/`, `ui/src/` or the workflow, and on manual runs. The repository's Pages source
must be set to **GitHub Actions**.
