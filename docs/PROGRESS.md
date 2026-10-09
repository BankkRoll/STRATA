# Progress

Resume point for any fresh session. Update at the end of every session.
Per-track details (API, benchmarks, decisions) live in `docs/tracks/<track>.md`.

## Milestones

| # | Milestone | Status |
|---|---|---|
| M0 | Foundations | **Done.** CI green on x64 + ARM64 |
| M1 | MFT scanner CLI | Engine + CLI merged, tested on synthetic images. Needs an elevated run: real-volume timing, reconciliation, VHDX golden data |
| M2 | Fallback walker | **Merged.** C:\ at 20–23 s per 1M entries (target ≤ 30 s). MFT-vs-walker reconciliation pending fixtures |
| M3 | Index + helper + IPC | Index, Win32 layer and secured pipe merged. Helper binary + app streaming not started |
| M4 | Treemap + list + detail | Layout engine merged; WebGL UI in progress (ui track) |
| M5 | Live updates | Index live-update API merged (property-tested). USN tailing, cache catch-up not started |
| M6 | Classifier + attribution | Engine, 240 rules, app catalog merged. Apps/category views not started |
| M7 | Cleanup | Safety core merged (never-list, TOCTOU, locks, Recycle Bin + restore). Queue UI not started |
| M8 | Search + palette | Search engine merged (first batch ~1 ms over 5M). Palette UI in progress |
| M9 | Other views | Layouts merged; renderers in progress (ui track) |
| M10 | Duplicates | Hash cache storage merged; pipeline not started |
| M11 | History + timeline | Storage, diffs, retention merged; UI not started |
| M12 | ETW | Rollup storage merged; tracing not started |
| M13 | Settings, tray, service | Settings model merged; UI/tray/service not started |
| M14 | Licensing, installer, updater | License storage merged; rest not started |
| M15 | Polish + hardening | Not started |

## Merged crates

| Crate | Purpose | Tests |
|---|---|---|
| `strata-core` | Shared types and contracts | 26 |
| `strata-ntfs` + `strata-cli` | Raw MFT scanner, USN parser, CLI | 84 |
| `strata-walk` | Unelevated parallel walker | 56 |
| `strata-index` | SoA index, aggregates, live updates, search, cache | 79 |
| `strata-layout` | Treemap, sunburst, icicle, packing, mind map, picking | 75 |
| `strata-classify` | Rule engine, 240 rules, app catalog, sniffing | 68 |
| `strata-clean` | Never-list, pre-flight, deletes, restore, locks | 119 |
| `strata-store` | SQLite history, settings, undo log, caches | 109 |
| `strata-win` | Volumes, known folders, elevation, signatures | 57 |
| `strata-ipc` | Helper protocol, framing, secured named pipe | 39 |

## Running

- ui track: WebGL renderers, app shell, list/detail panels, home screen, palette.

## Next

- Apply pending core change requests (see "Core change requests" in each track doc):
  `FolderSource`, `LocalAppDataLow`/`SavedGames`, `Sizes::attr_overhead`, `Category::key()`.
- `strata-helper` binary (wiring steps in `docs/tracks/platform.md`).
- App backend: scan → index → classify → layout → Tauri Channel.

## Known issues

- `cargo test --workspace` must exclude `strata-app` (tauri-build stub msvcrt.lib leaks into
  doctests); test it separately. See CLAUDE.md.
- cargo-fuzz targets build but libFuzzer doesn't run on this Windows toolchain; run on Linux/WSL.
  Stable proptest never-panic suites cover the same inputs.

## Benchmarks

| Area | Metric | Result | Target |
|---|---|---|---|
| NTFS | Parse, 1M records (parallel, in memory) | 155–291 ms | ≤ 3 s end to end |
| Walker | C:\ default walk | 20–23 s per 1M | ≤ 30 s |
| Index | Bytes per entry (names excluded) | 58.7–59.2 | ≤ 64 |
| Search | First results, 5M names | ≤ 1.1 ms | ≤ 50 ms |
| Layout | 100k-entry subtree relayout | 3.2 ms | ≤ 50 ms |
| Layout | Pick (hit test) | 0.12–0.29 µs | < 1 ms |
| Classify | Per entry | ~150 ns | — |
| IPC | Named pipe throughput | 3.2 M records/s | — |
| Store | Snapshot commit, 50k dirs | 53–215 ms | < 1 s |
