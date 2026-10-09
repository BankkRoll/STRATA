# Track: shell features (`src-tauri/src/features/`)

The app-side (Tauri backend) features beyond scanning: store lifecycle, settings, cleanup,
built-in tools, history, tray, notifications, startup and helper service mode. SPEC §15, §18,
§19, §21 (crash mid-delete, Recycle Bin unavailable, locked between pre-flight and action,
multiple instances), M7/M11/M13.

## Status

| Item | State |
|---|---|
| Store lifecycle: open per user, `health()`, reset history/state, recovery before cleanup, daily retention + activity pruning | Done |
| Settings: load / validate / save / export / import (file dialogs), `settings://changed`, side effects | Done |
| Cleanup: plan / preflight / execute (Channel progress, cancellable) / retry / discard / history / restore | Done |
| `AuditLog` adapter over the store's undo log (write-ahead) + crash recovery | Done |
| Never-list enforced again at the app layer (plan, preflight, execute, retry) | Done |
| Privileged deletes: `PrivilegedBackend` hook + `cleanup_execute_elevated` | Done (hook; the helper client satisfies it) |
| Locks: query, polite close behind a consent round trip | Done |
| Built-in tools: list / prepare / run / cancel, DISM via helper or UAC with output, Empty Recycle Bin | Done |
| History: volumes, series, snapshots, diff, dir series, since last scan | Done |
| Tray (optional): free-space glance, quick scan, open, quit, close-to-tray; low-space toast | Done |
| Startup: launch at login (off by default), start minimized to tray, launch/second-instance args | Done |
| Helper service install / uninstall / status | Done (against the helper's agreed flags) |
| Recycle Bin size and support per volume | Done |
| `app_capabilities` | Done (with a registry other tracks extend) |

## Files

```
src-tauri/src/features/
  mod.rs        register(), setup(), app_capabilities, invoke_handler! macro
  error.rs      FeatureError { kind, message, detail? }, blocking()
  store.rs      AppStore, store_* commands, maintenance thread
  audit.rs      StoreAuditLog (AuditLog adapter), recover() crash recovery
  consent.rs    PendingConsents: the confirmation round trip
  settings.rs   settings_* commands, validation, effects, export/import
  cleanup.rs    CleanupService, PrivilegedBackend hook, cleanup_* commands
  locks.rs      locks_* commands, KnownHolders
  tools.rs      tools_* commands, recycle_bin_empty, recycle_bin_info
  history.rs    history_* commands
  tray.rs       tray icon, menu model, low-space monitor
  startup.rs    launch at login, launch args, second instance
  helper.rs     helper_service_* commands
```

## Registering in `lib.rs` (already done on this branch)

```rust
pub mod features;

let builder = tauri::Builder::default().plugin(tauri_plugin_single_instance::init(
    |app, argv, cwd| features::on_second_instance(app, argv, cwd),   // 1. args forwarded
));
features::register(builder)                                          // 2. dialog, notification, autostart
    .setup(|app| {
        /* window + backdrop as before */
        let launch = features::setup(app);                           // 3. store, recovery, tray, monitor
        if launch.show_window { window.show()?; }
        Ok(())
    })
    .invoke_handler(features::invoke_handler![app_info /*, backend commands... */])
```

`invoke_handler!` expands to `tauri::generate_handler![<all shell commands>, <yours>]`, so the
backend track adds its commands inside the same macro call (trailing comma allowed). Other
tracks:

- **Store:** use `features::store::handle(&app_handle)` (a cloned `strata_store::Store`); do not
  open a second store. Directory: `%LOCALAPPDATA%\app.strata.desktop\store`.
- **Capabilities:** call `features::register_capabilities(&app_handle, &["layout_open", ...])`
  in setup so `app_capabilities` lists your commands.
- **Helper client:** call `features::cleanup::set_privileged_backend(&app, Some(Arc::new(client)))`
  when the helper connects and `None` when it disconnects.
- **Cleanup queue:** `cleanup_queue_add { volumeId, ids }` (UI contract) belongs with the index.
  Build `strata_clean::flow::QueueItem`s from the index on the Rust side and call
  `app.state::<features::cleanup::CleanupState>().service().plan(items)`; that avoids sending
  64-bit file references through JSON (see `cleanup_plan` below).
- **Uninstallers:** prepare them with `features::tools::prepare_tool(&app, &ToolAction::Uninstall
  { app_name, uninstall_string })` from the app catalog; the webview cannot submit its own
  uninstall strings.

## Conventions

- Arguments are camelCase (Tauri maps them to the snake_case Rust parameters).
- DTOs made here are camelCase with times in **Unix ms** and 64-bit hashes as strings.
- Engine types passed through unchanged keep their crate's serde names (snake_case):
  `strata_clean::flow::{Plan, Decision, Acknowledgements, Progress, ExecutionReport, QueueItem}`,
  `preflight::ItemVerdict`, `locks::{LockHolder, CloseOutcome}`, `tools::{CommandSpec, ToolOutput}`
  and `strata_store::Settings`. Their exact shapes are in `docs/tracks/clean.md` and
  `docs/tracks/store.md`.
- Every command rejects with `FeatureError`:
  `{ kind: string, message: string, detail?: any }`. `kind` is one of `store_unavailable`,
  `store_corrupt`, `invalid_settings` (detail: `SettingsIssue[]`), `invalid_input`, `not_ready`,
  `unknown_plan`, `preflight_required`, `busy`, `refused` (detail: `Refusal`), `clean` (detail:
  `CleanError`), `restore` (detail: `RestoreError`), `tool` (detail: `ToolError`), `close_app`
  (detail: `CloseError`), `consent` (detail: `"unknown_token" | "expired" | "too_fast"`),
  `helper_missing`, `helper_declined`, `helper_failed`, `unsupported`, `io`, `cancelled`,
  `not_found`, `internal`.
- Blocking work (SQLite, Win32, Shell) runs on `spawn_blocking`, never on the IPC thread.

## Commands

### App, store, startup

| Command | Args | Returns |
|---|---|---|
| `app_capabilities` | – | `string[]`: commands that work **now** (see "Capabilities") |
| `app_launch_request` | – | `LaunchRequest \| null`, the first launch's request, once |
| `startup_status` | – | `{ launchAtLogin: boolean }` (actual Run-key state) |
| `store_health` | – | `StoreHealthDto` |
| `store_reset_history` | – | `{ movedTo: string \| null, health: StoreHealthDto }` |
| `store_reset_state` | – | same; **refused while `state.db` is healthy** (`invalid_input`) |

`LaunchRequest = { autostart: boolean, openPath: string | null, args: string[] }` (`openPath` is
the first non-flag argument, made absolute against the launch's working directory).

`StoreHealthDto = { open: boolean, error: string | null, history: DbHealthDto | null,
state: DbHealthDto | null }`, `DbHealthDto = { status: "ok" | "corrupt" | "too_new" |
"unavailable", detail: string | null, resettable: boolean }`.

### Settings

| Command | Args | Returns |
|---|---|---|
| `settings_load` | – | `Settings` (store type, snake_case) |
| `settings_validate` | `{ settings }` | `SettingsIssue[]` (`{ key, message }`), empty = valid |
| `settings_save` | `{ settings }` | saved `Settings`; emits `settings://changed` |
| `settings_export` | – | `{ path: string \| null, settings: null }` (save dialog; `null` path = cancelled) |
| `settings_import` | – | `{ path: string \| null, settings: Settings \| null }` (open dialog); emits `settings://changed` |

App-level validation on top of `Settings::validate()`: `tray.low_space_threshold_bytes` ≥ 100 MiB;
`startup.start_minimized_to_tray` needs `tray.enabled`. Import reads at most 1 MiB, decodes into a
scratch store, runs both validations, and only then writes (nothing is written on error).

Side effects of a change: `startup.launch_at_login` → autostart entry on/off; `tray.enabled` →
icon shown/removed; threshold/notification toggle → alerts re-armed and an immediate poll;
`appearance.units` → tray glance repainted.

### Cleanup

| Command | Args | Returns |
|---|---|---|
| `cleanup_status` | – | `{ readiness: { state: "recovering" \| "ready" } \| { state: "unavailable", reason }, elevatedRoute: boolean }` |
| `cleanup_plan` | `{ items: QueueItem[] }` | `{ planId: number, plan: Plan }` |
| `cleanup_preflight` | `{ planId, acks: Acknowledgements }` | `ItemVerdict[]` |
| `cleanup_execute` | `{ planId, decision: Decision, onProgress: Channel<Progress> }` | `ExecutionReport` |
| `cleanup_execute_elevated` | same | `ExecutionReport` (helper, permanent by file id) |
| `cleanup_cancel` | `{ planId }` | `null` (items already running finish) |
| `cleanup_retry` | `{ planId }` | `{ planId, plan }`: new plan of the retryable items of the last run |
| `cleanup_discard` | `{ planId }` | `null` |
| `cleanup_history` | `{ limit?: number (50, ≤500), before?: number }` | `ActionDto[]`, newest first |
| `cleanup_action` | `{ actionId }` | `{ action: ActionDto, items: ActionItemDto[] }` |
| `cleanup_restorable` | `{ limit?: number (100, ≤1000) }` | `ActionItemDto[]` |
| `cleanup_restore` | `{ actionId, itemId }` | `{ path: string }` (never overwrites) |

- `QueueItem = { id, path, expected: { file_ref, is_dir, size, modified }, safety }`.
  **`file_ref` and `modified` are u64 and can exceed 2^53**; from JavaScript they may lose
  precision, which makes the identity check refuse the item (safe, but useless). Build queue
  items on the Rust side (see "Registering").
- `Decision = { method: "recycle_bin" | "permanent", acks: { careful: number[], permanent,
  large_permanent, permanent_instead_of_recycle: number[] } }`.
- `Progress` (Channel) = `{ event: "started", items, bytes } | { event: "item_started", id } |
  { event: "item_finished", id, removed } | { event: "finished", summary }`.
- `ActionDto = { id, kind: "cleanup" | "duplicates" | "tool", status: "in_progress" | "completed"
  | "partial" | "failed" | "cancelled" | "interrupted", startedMs, finishedMs, itemCount,
  doneCount, failedCount, bytesDone }`.
- `ActionItemDto = { id, actionId, seq, path, size, method: "recycle" | "permanent" |
  "reboot_delete" | "tool", tier, result: "pending" | "done" | "failed" | "skipped", error,
  completedMs, restorable, restoredMs }`.

Rules enforced here:

1. Plans live on the backend; the UI refers to them by `planId` and cannot hand back an edited
   plan. Plans expire after 4 h idle.
2. The never-list runs before the flow sees the queue (refusals become `Refused` warnings), again
   on the stored plan at pre-flight, and again at execute (a hit fails the whole run with
   `refused`). Strata's install folder and its data folder are added as protected subtrees.
3. `cleanup_execute` needs a pre-flight of that plan in the last 10 minutes
   (`preflight_required`); a run clears it, so running again needs a new pre-flight.
4. One run at a time (`busy`). Cancellation is the flow's `CancelToken`.
5. Nothing acts until crash recovery finished (`not_ready` while `recovering`).
6. The elevated route requires a connected helper (`unsupported`), `method: "permanent"` with
   `acks.permanent` (`invalid_input` otherwise; the helper never recycles), and gates tiers and
   the large-delete threshold exactly like the flow. Each request is validated locally with
   `PrivilegedDeleteRequest::validate` (never-list, by-id shape) before it is sent.

### Locks (SPEC §15.3, §15.5)

| Command | Args | Returns |
|---|---|---|
| `locks_query` | `{ path }` | `{ holders: LockHolder[], runningApps: RunningAppWarning[] }` |
| `locks_close_prepare` | `{ pid, startTime }` | `ConsentTicket` |
| `locks_close_politely` | `{ token }` | `CloseOutcome` (`{ kind: "asked_windows", windows } \| { kind: "shut_down" }`) |
| `locks_close_cancel` | `{ token }` | `null` |

`ConsentTicket = { token: string, text: string, expiresInMs: number }`. Holders found by
`locks_query` and `cleanup_preflight` are remembered for 15 minutes; only those can be closed.

### Tools and Recycle Bin (SPEC §15.6)

| Command | Args | Returns |
|---|---|---|
| `tools_list` | – | `ToolInfo[]` = `{ request: ToolRequest, title, description, commandLine, launch, capturesOutput }` |
| `tools_prepare` | `{ request: ToolRequest }` | `{ token, text, expiresInMs, spec: CommandSpec \| null }` |
| `tools_run` | `{ token }` | `ToolOutput` = `{ exit_code, output }` (`null`s for the Recycle Bin) |
| `tools_cancel` | `{ token }` | `null` |
| `recycle_bin_empty` | – | `boolean`: the backend shows its **own native dialog**; `false` = cancelled or empty |
| `recycle_bin_info` | – | `{ volumes: RecycleBinVolume[], totalBytes, totalItems }` |

`ToolRequest = { kind: "disk_cleanup", drive: string | null } | { kind: "storage_sense_settings" }
| { kind: "dism_component_cleanup" } | { kind: "system_protection" } | { kind:
"hibernation_guidance" } | { kind: "compact_os_status" } | { kind: "empty_recycle_bin", root:
"C:\\" | null }`. Hibernation guidance is shown, never run (`tools_prepare` returns `tool`
error `not_runnable`). DISM runs through `PrivilegedBackend::run_tool` when a helper is
connected, otherwise through a UAC `cmd` with output captured to a temp file
(`strata_clean::tools::run`). DISM and emptying the bin are written ahead to the undo log as
`tool` actions.

`RecycleBinVolume = { volumeId, mountPoint, supported, reason: RecycleUnavailable | null,
reasonText, capacityBytes, bytes, items }`.

`recycle_bin_empty` returns `boolean`, not the `null` in `ui.md`, so the palette can tell a
cancel from a success.

### History (SPEC §18)

`volumeId` is the volume GUID path from `list_volumes`; it resolves to the store's newest
`(serial, guid_path)` key with that GUID path (case and trailing `\` ignored).

| Command | Args | Returns |
|---|---|---|
| `history_volumes` | – | `{ volumeId, serial: string, snapshotCount, firstMs, lastMs }[]` |
| `history_series` | `{ volumeId, fromMs?, toMs? }` | `{ snapshotId, atMs, totalBytes, usedBytes, freeBytes, allocatedSum, logicalSum }[]` |
| `history_snapshots` | `{ volumeId }` | `SnapshotDto[]` = `{ id, atMs, totalBytes, freeBytes, allocatedSum, logicalSum, fileCount, dirCount, scanner, storedDirs }` |
| `history_diff` | `{ from, to, options?: { sizeMode?, topN? (≤200), minChange?, largeThreshold? } }` | `{ from, to, usedDelta, scannedDelta, grown, shrunk, newLarge, deletedLarge }` with `DirChangeDto = { pathHash: string, path, before, after, deltaBytes }` |
| `history_dir_series` | `{ volumeId, path, lastN? (30, ≤365) }` | `{ snapshotId, atMs, sizes: { allocated, logical, files } \| null }[]` |
| `history_since_last_scan` | `{ volumeId }` | the UI's `SinceLastScan`: `{ deltaBytes, sinceMs, biggest: { path, deltaBytes } \| null } \| null` |

Unknown volumes return empty lists / `null`. The size mode defaults to
`scan.default_size_mode`. **`history_since_last_scan` is implemented here**; if the backend
track also adds one, keep one of them (same name, same shape).

### Helper service (SPEC §4, §19)

| Command | Args | Returns |
|---|---|---|
| `helper_service_install` | – | `HelperServiceStatus` (UAC prompt; runs `strata-helper.exe --install-service`) |
| `helper_service_uninstall` | – | `HelperServiceStatus` (`--uninstall-service`) |
| `helper_service_status` | – | `HelperServiceStatus` |

`HelperServiceStatus = { helperPresent, service: "not_installed" | "stopped" | "start_pending" |
"stop_pending" | "running" | "other" | "unknown", mode: "on_demand" | "service", exitCode }`.
The helper must sit next to the app executable. On exit code 0 the setting `helper.mode`
is updated and `settings://changed` emitted. Declined UAC → `helper_declined`; nonzero exit or
2-minute timeout → `helper_failed`. The service state comes from `sc.exe query StrataHelper`
(numeric `STATE` code, language-independent; exit 1060 = not installed).

**Coordination:** the service name `StrataHelper` (`helper::HELPER_SERVICE_NAME`) and the two
flags must match the helper binary.

## Events

| Event | Payload | When |
|---|---|---|
| `settings://changed` | `Settings` | after save, import, helper mode change |
| `app://second-instance` | `LaunchRequest` | another launch was started; the window is shown and focused first |
| `tray://quick-scan` | `{ volumeId: string \| null }` (system volume) | tray "Quick scan"; the UI (or a backend listener) calls `scan_start` |

## Capabilities

`app_capabilities` returns `app_info`, the always-available shell commands, plus:

- store-backed commands (settings, history, cleanup history/restore, tools_run,
  recycle_bin_empty, helper status) only when the store opened;
- `cleanup_plan/preflight/execute/cancel/retry/discard` only when the store opened and cleanup is
  not `unavailable` (during `recovering` they are listed and return `not_ready`);
- `cleanup_execute_elevated` only while a helper backend is installed;
- `helper_service_install/uninstall` only when `strata-helper.exe` is present;
- names registered with `features::register_capabilities`.

A unit test checks that the capability lists and the `invoke_handler!` list contain exactly the
same commands.

## Write-ahead and recovery

`StoreAuditLog` maps `strata_clean::audit::AuditLog` onto the store:

| `AuditLog` | Store | Durability |
|---|---|---|
| `begin_action` | `begin_action` with **every** planned item `pending` | committed on `state.db` (`synchronous=FULL`) before any item is touched |
| `item_started` | – (row already committed) | refuses (item untouched) unless the item was written ahead and `state.db` is healthy |
| `item_finished` | `complete_item` (recycled → `done` + `RestoreTicket` blob) | committed |
| `finish_action` | `finish_action` (`cancelled` / `partial` / `failed` / `completed`) | committed |

Per-item method: `recycle` unless the item is in `permanent_instead_of_recycle` or the method is
permanent. Volume key: GUID path from `strata_clean::volume::volume_info` and serial from
`strata_win::volume::local_volume_info`, cached per mount point.

At startup a background thread runs `audit::recover` before cleanup is enabled: for every
`in_progress` action, each `pending` item is checked on disk. Still there → `skipped`. Gone and
recycled → `done` with a Restore ticket recovered by `find_in_recycle_bin` (deleted at or after
the action start, 2-minute slack). Gone otherwise → `done` with a note. Not checkable →
`failed` with the reason. The action closes as `interrupted`. Then daily maintenance starts:
`apply_retention(settings.history.retention_policy())` and
`prune_activity(settings.activity.retention_days)`, at launch and every 24 h.

## Consent design

`consent::PendingConsents<T>`:

1. A `*_prepare` command builds the action from backend data (a lock holder found by a
   backend query; the bin's current count and size; a `CommandSpec` built from `{WINDIR}`),
   stores it under a 128-bit random token, and returns `{ token, text }`.
2. The UI shows `text` verbatim and, on the user's click, sends the token.
3. `take()` removes the token (single use, also on rejection), rejects it if older than
   `CONSENT_TTL` (120 s) or younger than `MIN_REVIEW` (400 ms, faster than anyone reads a
   prompt), and only then does the handler call `Prompt::confirm()`, redeeming the `Consent` on
   the same thread (it is `!Send`).

The webview can only confirm what the backend described, once, within the window. It never
sends the action itself. `recycle_bin_empty` goes further and shows a native dialog, so the
confirmation comes from outside the webview.

## Tray and notifications

- Tray (`tray.enabled`, off by default): tooltip `Strata: C: 120.3 GiB free`; menu = one
  disabled row per fixed volume (`C:  120.3 GiB free of 476.3 GiB`, system volume first),
  separator, "Quick scan", "Open Strata", separator, "Quit Strata". Left click opens the window.
- While the tray icon exists, closing the main window hides it to the tray (per-window
  `on_window_event`, so no `Builder::on_window_event` conflict). "Quit Strata" exits.
- Monitor thread: polls fixed, ready volumes every 60 s (`discover_volumes` without BitLocker
  or network queries), or right away on a settings change. It runs while the tray is on or
  `tray.low_space_notification` is set. A toast ("C: is low on space") fires once when free
  space falls below the threshold and re-arms only after it climbs back above threshold +
  max(5%, 256 MiB).
- Units follow `appearance.units`.

## Startup

- Autostart plugin registers `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` with
  `--autostart`. Off by default, set only by `settings_save`. At startup the entry is removed if
  the setting is off; it is **never re-added silently** (the user may have disabled it in Task
  Manager).
- Start minimized: when launched with `--autostart`, `startup.start_minimized_to_tray` and
  `tray.enabled` are both on, the window is not shown (`Launch.show_window = false`).
- Second instance: the single-instance callback shows and focuses the window and emits
  `app://second-instance` with the parsed arguments. The first launch's arguments are kept
  for `app_launch_request`.

## Plugins and config

| Addition | Why |
|---|---|
| `tauri` feature `tray-icon` | tray |
| `tauri-plugin-dialog 2.8` | export/import file dialogs, native Empty Recycle Bin confirmation |
| `tauri-plugin-notification 2.5` | low-space toast |
| `tauri-plugin-autostart 2.7` | launch at login |
| `getrandom 0.3` | consent tokens |
| `strata-core`, `strata-store`, `strata-clean`, `strata-win` | engine |
| `tempfile` (dev) | temp stores in tests |

No capability changes: every plugin is used from Rust only, so the webview gets no plugin
permissions (it cannot call the autostart/dialog/notification JS APIs directly). `tauri.conf.json`
is unchanged; NSIS's default naming already gives `Strata_<version>_x64-setup.exe` /
`Strata_<version>_arm64-setup.exe`.

## Tests

`cargo test -p strata-app`: **44 passing** (41 in `features`, 3 existing). Clippy
`--all-targets -D warnings` clean.

| Test | Covers |
|---|---|
| `cleanup::plan_preflight_execute_restore_round_trip` | Two temp files under `%TEMP%\strata-shell-tests\<random>`: plan → (execute without pre-flight refused) → preflight → execute to the **real Recycle Bin** with progress → undo log rows restorable → `restore_item` brings exactly those items back with the same contents → restoring again refused → re-run needs a new pre-flight. A drop guard restores the files if an assertion fails |
| `cleanup::never_list_is_enforced_at_the_app_layer` | `%SystemRoot%` in the queue is dropped before the flow with a `Refused` warning |
| `cleanup::elevated_route_sends_validated_requests_and_logs_them` | No helper → `unsupported`; recycle decision → refused; with a recording helper (which refuses), the request carries the queued path and the action is logged `failed`/`permanent`; the file is untouched |
| `cleanup::elevated_gate_requires_every_confirmation`, `cleanup_waits_for_recovery`, `unknown_plans_are_rejected` | gates, readiness, unknown plans |
| `audit::begin_action_is_durable_before_any_item_starts` | After `begin_action` + `item_started`, a second store handle (a "fresh process") sees every item `pending` |
| `audit::full_protocol_records_outcomes_and_status` | Recycled → restorable with a valid ticket blob, failed with message, permanent not restorable, `partial` |
| `audit::unknown_items_and_actions_are_refused` | Unplanned items abort `begin_action` with nothing written; `item_started` refuses unknown items/actions |
| `audit::crash_recovery_reconciles_pending_items` | Crash simulation with a fake probe: still present / in bin (ticket recovered) / in bin but too old / gone / uncheckable; action `interrupted`; idempotent |
| `consent::*` (4) | Round trip mints consent for exactly the shown action; unknown, too-fast, expired and replayed tokens refused; cancel; pruning; token format |
| `settings::*` (4) | App validation rules; save rejects invalid without writing; effects diff; export → import through a file; foreign, invalid-by-app-rule and oversized imports rejected with nothing written |
| `tray::*` (3) | Menu construction (order, labels, ids, empty state), tooltip and units, low-space edge trigger + hysteresis + reset |
| `tools::*` (6) | Request → `ToolAction` → exact absolute command lines; UI request shape; uninstall not accepted from the UI; removed silent flags shown; drive-root validation; tool actions written ahead; Recycle Bin facts for the system drive (read-only) |
| `helper::*` (3) | Elevated command construction and quoting; `sc query` parsing; read-only SCM query |
| `history::*` (3) | Volume id resolution, since-last-scan DTO shape (matches the UI's `SinceLastScan`), diff option clamping, ms conversion, path hashes as strings |
| `startup`, `store`, `locks`, `error`, `mod` | Argument parsing; maintenance on a fresh store; health DTOs; only reported holders closable; error wire shape; capability list ↔ handler list |

Safety: no test empties the Recycle Bin, closes an app, writes the Run key, or installs a
service. The only real-bin test recycles and restores its own two temp files.

## Manual test procedures

Run `pnpm tauri dev` (unelevated).

1. **Launch at login:** Settings → Startup → on, save. `startup_status` → `true`; Task
   Manager → Startup apps lists Strata. Turn off → entry removed. Sign out/in with it on and
   "start minimized" + tray on: no window, tray icon present.
2. **Tray:** enable tray → icon appears with the free-space tooltip; menu rows per fixed drive.
   "Open Strata" and left click show the window; closing the window hides it; "Quit Strata"
   exits. "Quick scan" shows the window and emits `tray://quick-scan`.
3. **Low-space toast:** set the threshold above a drive's free space (e.g. 2 TB) → within a
   second a toast "C: is low on space"; no repeat on later polls; lower the threshold, raise it
   again → toast again.
4. **Second instance:** with Strata running, run `strata.exe D:\Projects` → the window comes to
   front and `app://second-instance` carries `openPath: "D:\Projects"`.
5. **Empty Recycle Bin:** palette → Empty Recycle Bin → native dialog with item count and size;
   Cancel → `false`; confirm → emptied, `cleanup_history` shows a `tool` action.
6. **Close an app holding a file:** open a text file in Notepad, `locks_query` on it →
   holder Notepad; `locks_close_prepare` → text; confirm → Notepad receives `WM_CLOSE` (it may
   ask to save). Confirming twice or after 2 minutes fails with `consent`.
7. **DISM:** `tools_prepare { kind: "dism_component_cleanup" }` → command line shown; run → UAC
   prompt; output returned when finished (minutes).
8. **Helper service** (needs `strata-helper.exe` next to the app): install → UAC → status
   `stopped`/`running`, `mode: "service"`; uninstall → `not_installed`, `mode: "on_demand"`;
   decline UAC → `helper_declined`, nothing changed.
9. **Crash mid-delete:** queue a large folder of temp files you created, start
   `cleanup_execute`, kill the process from Task Manager mid-run. Relaunch: `cleanup_status`
   goes `recovering` → `ready`; `cleanup_history` shows the action `interrupted` with recycled
   items restorable.
10. **Damaged history:** with the app closed, overwrite `store\history.db` with garbage;
    launch → `store_health.history.status = "corrupt"`; `store_reset_history` → `ok`, settings
    kept.

## Decisions

- **Every planned item is written ahead in one transaction** before the first is touched. The
  store has no per-item "started" state, so `item_started` verifies the committed row and
  database health instead of writing. Recovery checks each `pending` item on disk.
- **Plans stay on the backend** (by id), so the reviewed plan is the executed plan.
- **Consent tokens** (single use, TTL 120 s, minimum review 400 ms) rather than a boolean
  "confirmed" argument; the action is always rebuilt from backend data. Empty Recycle Bin from
  the palette uses a native dialog because the UI contract asks the backend to confirm.
- **Uninstall strings are never accepted from the webview**; they come from the app catalog via
  `prepare_tool`.
- **The elevated route is permanent-only** and needs the same confirmations as a permanent
  delete; the helper cannot recycle into the user's bin.
- **Close-to-tray only while the tray icon exists**; otherwise closing quits as before.
- **Start minimized applies to `--autostart` launches only**; a launch from the Start menu
  always shows the window.
- **Launch at login is never re-enabled silently** at startup; only stale entries are removed.
- **Free space is polled every 60 s** rather than via volume notifications: there is no
  free-space change notification, and `GetDiskFreeSpaceExW` per fixed volume is negligible.
- **Settings keep the store's snake_case JSON**: `Settings` is one large typed struct shared with
  export files; a camelCase mirror would double the surface for no gain.
- **App-level settings rules** (low-space threshold minimum, start-minimized needs the tray) are
  checked on import too, through a scratch store.
- **The service state is read with `sc.exe query`** (numeric state, exit 1060) instead of new
  SCM FFI in the app, keeping `unsafe` out of `src-tauri`.

## Engine change requests

1. **`strata-store`: a per-item "started" mark.** Add `ItemResult::Started` (or
   `Store::mark_item_started(action, seq)`) so recovery can tell items that were never reached
   (`pending`) from ones that might have been acted on (`started`). The adapter would call it in
   `item_started`:

   ```rust
   // undo.rs
   pub fn mark_item_started(&self, id: ActionId, seq: u32) -> Result<()> {
       self.state().write(|tx| {
           require_in_progress(tx, id)?;
           let n = tx.execute(
               "UPDATE action_items SET result = 'started' WHERE action_id = ?1 AND seq = ?2 AND result = 'pending'",
               params![id.0, i64::from(seq)],
           )?;
           if n == 0 { return Err(StoreError::NotFound(format!("item {seq} of action {}", id.0))); }
           Ok(())
       })
   }
   ```
   (plus `Started = "started"` in `ItemResult`, and `SQL_SKIP_PENDING` also matching `started`
   rows as `failed`/"interrupted"). It costs one fsync per item.
2. **`strata-store`: `Store::item(ItemId) -> ItemRecord`.** `cleanup_restore` loads the whole
   action to find one item.
3. **`strata-clean`: public `recycle_bin_stats(root) -> (bytes, items)`.** `recycle_bin_info`
   currently reads the item count through `tools::empty_recycle_bin_prompt(..).action()` (no
   side effects, but indirect).
4. **`strata-clean`: public `flow::gate`.** The elevated route mirrors it in
   `cleanup::elevated_gate`; exporting it would remove the duplicate.

## Blockers and limits

- Not run against a real `strata-helper.exe` (another track builds it); install/uninstall are
  construction-tested and the SCM query is read-only.
- Tray, toasts, autostart, dialogs and polite close need a desktop session and are covered by the
  manual procedures above, not automated tests.
- `cleanup_plan` from JavaScript can lose `file_ref` precision (see Cleanup). Safe (refused),
  but the index should build queue items in Rust.
