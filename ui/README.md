# ui

The frontend: React 19 + TypeScript + Vite, Zustand for state, TanStack Virtual for the
tree-table, and raw WebGL2 (instanced quads) for the visual views, with Canvas2D labels.

Everything that needs the engine goes through typed contracts in `src/lib/` and is injected via
`src/services.tsx`, so tests and the dev harness swap in fakes or exported layout fixtures
without the components knowing. When the backend reports a command as unavailable, the UI shows
a designed state with the reason.

## Layout

| Path | What |
|---|---|
| `src/components/`, `src/views/` | App shell, nav, top bar, command palette, list, detail panel, home |
| `src/render/` | WebGL2 renderers (rect, arc, circle), labels, `ViewController` |
| `src/lib/layout/` | Zero-copy decoding of `strata-layout` frames, picking, navigation |
| `src/lib/` | Backend contracts (`volumes`, `rows`, `entries`, `detail`, `search`, `commands`) |
| `src/store/` | Zustand stores |
| `src/dev/` | Dev-only fixture harness, excluded from production builds |

## Commands

```powershell
pnpm --dir ui dev          # Vite dev server, including the fixture harness
pnpm --dir ui typecheck
pnpm --dir ui lint
pnpm --dir ui test         # Vitest
pnpm --dir ui build        # also enforces the bundle-size budget
pnpm --dir ui fixtures     # re-export layout fixtures from strata-layout
```

Run the whole app with `pnpm tauri dev` from the repo root.
