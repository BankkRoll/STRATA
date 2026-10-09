# Strata website

The project site at <https://bankkroll.github.io/STRATA/>: a static Vite + TypeScript page.

The interactive demo is the app itself. `pnpm --dir site build` also builds `ui/vite.demo.config.ts`
(entry `ui/src/demo/`) into `site/dist/demo/`: the real app shell and stylesheet over the sample tree
that `strata-layout` exports for the UI tests, with layouts computed in the browser by a TypeScript
port of the layout engine (checked byte for byte against the Rust fixtures). Only Explore works;
every other area shows the app's own unavailable state. The page embeds it in a same-origin iframe
at 1280×800, scaled to fit.

## Develop

```powershell
pnpm install
pnpm --dir site dev       # http://localhost:5173/STRATA/
pnpm --dir site build     # type-check and build to site/dist
pnpm --dir site preview   # serve the build
```

Append `?nogl` to the URL to see the app's own state for machines without WebGL2.
`pnpm --dir ui demo` serves the demo on its own.

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
touches `site/`, `ui/src/`, the demo build config or the workflow, and on manual runs. The repository's Pages source
must be set to **GitHub Actions**.
