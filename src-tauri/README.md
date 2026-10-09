# src-tauri (`strata-app`)

The desktop app: the unelevated Tauri v2 process that hosts the WebView2 frontend from
[`ui/`](../ui) and wires the engine crates to it. It owns the index, layout, classifier, store
and cleanup flow, streams layout buffers to the UI over Tauri Channels, and talks to the
elevated [`strata-helper`](../crates/strata-helper) through the
[`strata-ipc`](../crates/strata-ipc) pipe client.

## Code

- `src/lib.rs`: `run()` builds the app, registers plugins and Tauri commands.
- `src/backdrop.rs`: Windows 11 Mica backdrop, with a solid fallback on older builds.
- `tauri.conf.json`, `capabilities/`: window, bundle and permission config.

The main window is single-instance and starts hidden until its backdrop is applied, so a
transparent frame is never visible.

## Run and test

```powershell
pnpm --dir ui build     # once: generate_context! needs ui/dist
pnpm tauri dev          # from the repo root
cargo test -p strata-app
```

Test this crate on its own rather than in an unqualified `cargo test --workspace`; see the
root [README](../README.md).
