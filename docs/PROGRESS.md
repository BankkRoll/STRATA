# Progress

Resume point for any fresh session. Update at the end of every session.
Per-track details (API, benchmarks, decisions) live in `docs/tracks/<track>.md`.

## Milestones

| # | Milestone | Status |
|---|---|---|
| M0 | Foundations | **Done.** CI green on x64 + ARM64 (run 37966756973) |
| M1 | MFT scanner CLI | In progress (ntfs track) |
| M2 | Fallback walker | In progress (walk track) |
| M3 | Index + helper + IPC | In progress (index, platform tracks); helper binary after M1 merges |
| M4 | Treemap + list + detail | Layout engine **merged** (`strata-layout`); WebGL renderer + UI not started |
| M5 | Live updates | Incremental index API in progress (index track) |
| M6 | Classifier + attribution | In progress (classify track) |
| M7 | Cleanup | Safety core in progress (clean track) |
| M8 | Search + palette | Search engine in progress (index track) |
| M9 | Other views | Sunburst, icicle/flame, circle packing, mind map layouts **merged**; UI not started |
| M10 | Duplicates | Hash cache storage merged (`strata-store`); pipeline not started |
| M11 | History + timeline | Storage, diffs, retention **merged** (`strata-store`); UI not started |
| M12 | ETW | Rollup storage merged; tracing not started |
| M13 | Settings, tray, service | Settings model + persistence merged; UI/tray/service not started |
| M14 | Licensing, installer, updater | License storage merged; rest not started |
| M15 | Polish + hardening | Not started |

## Done

- M0: workspace, Tauri 2.12 shell (single instance, Mica/solid backdrop), themed empty home
  screen, `strata-core` shared types, docs, CI (parallel jobs, SHA-pinned actions, Dependabot).
- `strata-layout`: squarified treemap with LOD + viewport culling, picking (<1 µs), transitions,
  cushion coefficients, sunburst, icicle/flame, circle packing, mind map. 100k relayout 3.2 ms,
  1M tree 6.8 ms. 75 tests. See `docs/tracks/layout.md` for byte-exact buffer formats.
- `strata-store`: two SQLite files (`history.db` rebuildable, `state.db` durable), versioned
  migrations, corruption recovery, compact snapshot blobs (12 B/dir), diffs, retention, settings
  (all §19 keys), write-ahead undo log, activity rollups, hash cache, license storage. 109 tests.

## Running

Parallel tracks in worktrees: ntfs, walk, index, classify, clean, platform.

## Next

- Merge remaining tracks as they finish; apply core change requests.
- Implement `LayoutSource` on the index; stream layout buffers over a Tauri Channel.
- Helper binary (after ntfs + platform merge).

## Known issues

- `cargo test --workspace` must exclude `strata-app` (tauri-build stub msvcrt.lib leaks into
  doctests); test it separately. See CLAUDE.md.

## Benchmarks

| Area | Metric | Result | Target |
|---|---|---|---|
| Layout | 100k-entry subtree relayout | 3.2 ms | ≤ 50 ms |
| Layout | 1M-node treemap with LOD | 6.8 ms | 60 fps |
| Layout | Pick (hit test) | 0.12–0.29 µs | < 1 ms |
| Store | Snapshot commit, 50k dirs | 53–215 ms | < 1 s |
| CI | Full pipeline (cold cache) | ~11 min wall (was 25+) | fast |
