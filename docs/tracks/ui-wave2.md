# Track: UI wave 2 (`ui/`)

Cleanup queue and flow, built-in tools, the analysis views (largest files, file types, apps,
categories, recommendations, duplicates, activity, history), the settings screen and the
detail-panel additions. Builds on wave 1 (`docs/tracks/ui.md`): same shell, same
`app_capabilities` gating, same "no fake data" rule. Every view binds to the typed contracts
below; a missing command shows a designed "isn't available in this build" state that names the
command. Fixtures exist only in tests (`ui/src/test/features.ts`).

## Status against SPEC

| SPEC | Item | State |
|---|---|---|
| §15.2 | Queue: add from any view (context menu / Delete key, list, largest files, recommendations, duplicates, app caches), nav badge with count, totals by tier | Done |
| §15.2 | Review: grouped by tier, expandable, deselect, Careful needs a tick, per-item explanation + safety badge, never-tier shown as protected and never sent | Done |
| §15.2 / §15.4 | Method: Recycle Bin (default from settings) / permanent with extra confirmation and a second one above `large_delete_confirm_bytes`; per-volume Recycle Bin availability shown before acting; items the bin can't take need an explicit "Delete permanently" or "Skip" | Done |
| §15.2 step 5 / §15.3 | Pre-flight: changed / missing / locked with "In use by: X (PID n) — [Close app] [Skip]"; polite close only after a consent dialog showing the backend's prompt text; "Delete on next restart" for locked items when the helper offers it | Done |
| §15.2 step 6 | Execute with streamed progress and Cancel, per-item results, retry of retryable failures (new plan, pre-flight again) | Done |
| §15.2 step 7 | Undo history tab, Restore single / selected | Done |
| §15.1 | Never-tier items can't be added: backend refusals with reasons are shown in the status bar and in a "Not added" panel; buttons on never-tier rows are disabled with the reason | Done |
| §15.5 | Running-app warnings ("Close X first") in review and pre-flight | Done |
| §15.6 | Tools panel: Empty Recycle Bin, Disk Cleanup, Storage Sense, DISM (elevated, output streamed), System Protection, hibernation guidance (never run), CompactOS status, app uninstaller (Apps view). Exact command shown before running | Done |
| §16.2 | Largest files: global or under the current folder, files/folders, top 1000, filters (extension, category, size, age, safety), select → detail panel | Done |
| §16.2 | File types: bars + full table by extension, detected-content table, click → Largest files filtered | Done |
| §12.3 | Apps: footprint by location, confidence + evidence, registry-vs-measured mismatch, Uninstall (confirmed), Clean caches (queued), orphaned app data | Done |
| §12.5 / §16.2 | Categories: totals with fixed palette + patterns, drill-through to map filter, Largest files, top contributors | Done |
| §16.2 | Free up space: ranked, explainable, previewable (deselect items), one-click add to queue, or open the matching tool | Done |
| §14 | Duplicates: scan with progress / cancel / resume, groups by wasted bytes, keep suggestion + reason, guardrailed selection (never every copy, never never-tier copies), replace with hardlinks behind a warning acknowledgement | Done |
| §11 | Activity: opt-in explanation, top writers now / hour / today, overhead state, clear data | Done |
| §18 | History: usage line chart, diff picker (any two snapshots) with grown / shrunk / new / deleted, since-last-scan banner links to it | Done |
| §19 | Settings: every key of `strata_store::Settings` with a control, live validation, save / revert, export / import, rules tools, data clearing, helper service install/uninstall, startup / tray, About (version, licenses, update channel + check) | Done |
| §16.3 / §13 | Detail panel: shared sparkline (falls back to `history_dir_series`), "Who wrote here" (ETW) for folders, "Why is this classified as X?" explanation | Done |
| §21 | Keyboard-only, screen reader, small windows: see Accessibility | Done |

## Architecture

```
ui/src/
  features.tsx            FeatureServices + FeaturesContext (default = Tauri implementation)
  lib/bridge.ts           listenEvent, callWithChannel (built on backend.ts `call`)
  lib/cleanup.ts          queue / plan / pre-flight / execute / undo contracts + reviewBlockers
  lib/tools.ts            tools_prepare / tools_run / tools_status
  lib/insights.ts         largest, file types, categories, apps, recommendations
  lib/dupes.ts            duplicate finder + selection guardrails
  lib/activity.ts         ETW activity
  lib/history.ts          snapshots, usage, diff, dir series
  lib/settings.ts         Settings (store serde shape), validation mirror, rules, data, about
  lib/chart.ts            scales, nice ticks, byte ticks, paths with gaps, delta format
  components/charts.tsx   Sparkline, TimeChart, BarList (SVG, each with a data table)
  components/feature.tsx  useCapability, useLoad, LoadState, Unavailable, ViewFrame, SafetyBadge, ConfirmDialog
  components/Explanation.tsx  classifier explanation
  store/queue.ts          queue mirror, useQueueSync, applyAddResult
  store/prefs.ts          applies persisted appearance at startup and after save
  store/insights.ts       shared Largest-files filters
  views/featureViews.tsx  nav registry + lazy router
  views/*View.tsx         one lazy chunk per view; views/DetailExtras.tsx for the detail panel
  views/features.css      styles for the above
```

Shared-file edits are small and additive: `ViewId` gained `FeatureView`; the nav renders an
"Insights" and a "Manage" group from `FEATURE_VIEWS`; `AppShell` mounts `useQueueSync` /
`useAppearanceSync` and renders `FeatureRouter` for feature views; the palette lists the new
views and "Open settings" / "Show largest files" now work; `CommandBus` routes "Add to cleanup"
through `addToQueue` so refusals are reported; the detail panel uses `DetailExtras`; the home
banner gained "See what changed". `services.tsx` and `backend.ts` are untouched.

## Backend contract (new commands, events, channels)

Conventions as in wave 1: Tauri camelCase argument names, JSON, times Unix ms UTC, sizes as JSON
numbers, enum strings in `snake_case` (Rust serde names). DTOs are camelCase
(`#[serde(rename_all = "camelCase")]`) **except `Settings`**, which is exactly the snake_case
serde shape of `strata_store::Settings` (pass-through). Tagged unions keep the Rust tag field
(`kind`, `fit`, `state`, `status`, `event`). The exact TypeScript types (with TSDoc) are in the
files listed; this table is the summary. Every command name must appear in `app_capabilities`
when implemented.

### Cleanup (`lib/cleanup.ts`)

| Command / event | Args | Returns |
|---|---|---|
| `cleanup_queue_list` | – | `QueueEntry[]` |
| `cleanup_queue_add` (changed) | `{ volumeId, ids: number[] }` | `QueueAddResult { added: QueueEntry[], refused: QueueRefusal[] }` (`null` still accepted) |
| `cleanup_queue_remove` | `{ ids: number[] }` (queue ids) | `null` |
| `cleanup_queue_clear` | – | `null` |
| event `cleanup://queue-changed` | payload `QueueEntry[]` (full queue) | – |
| `cleanup_plan` | `{ queueIds: number[] }` | `CleanupPlan` |
| `cleanup_preflight` | `{ planId, decision: Decision }` | `ItemVerdict[]` |
| `cleanup_close_prompt` | `{ pid, startTime }` | `ClosePrompt { promptId, message, app, pid, expiresMs }` (mints `consent::Prompt`; closes nothing) |
| `cleanup_close_app` | `{ promptId }` | `CloseOutcome` = `{ kind: "asked_windows", windows } \| { kind: "shut_down" }` |
| `cleanup_execute` | `{ planId, decision, onProgress: Channel<CleanupProgress> }` | `ExecutionReport` |
| `cleanup_cancel` | `{ planId }` | `null` |
| `cleanup_retry_plan` | `{ planId }` (of the finished run) | `CleanupPlan` (`Plan::retry`) |
| `cleanup_delete_on_reboot` | `{ planId, id }` | `null` (helper only) |
| `cleanup_history` | `{ limit, beforeActionId: number \| null }` | `UndoAction[]`, newest first |
| `cleanup_restore` | `{ itemIds: number[] }` | `RestoreResult[] { itemId, ok, message }` |

- `QueueEntry { id, volumeId, entryId, path, name, isDir, bytes, safety, category, ruleId|null,
  ruleName|null, explain, regenerable, app|null, addedMs, source: "manual"|"recommendation"|"duplicates"|"app_caches" }`.
  The backend resolves `Expected` (file ref, size, mtime) itself; the UI never sends paths.
- `QueueRefusal { entryId, path, reason: "never_tier"|"never_list"|"already_queued"|"inside_queued"|"not_found"|"virtual"|"unverifiable", message }`.
  `message` is shown verbatim.
- `CleanupPlan { planId, items: QueueEntry[], totals: TierTotal[], volumes: PlanVolume[], warnings: PlanWarning[], largeDeleteBytes, defaultMethod: "recycle_bin"|"permanent" }`.
  `PlanVolume { mountPoint, recycleBin: RecycleBinSupport, items, bytes }`;
  `RecycleBinSupport = { state: "available", capacity|null, used|null } | { state: "unavailable", reason: RecycleUnavailable }`;
  `RecycleFit = { fit: "fits" } | { fit: "unknown" } | { fit: "too_large", capacity } | { fit: "unavailable", reason }`.
- `PlanWarning` (tag `kind`): `refused { id, message }` (flatten `Refusal` to its message),
  `duplicate { id }`, `nested { id, inside }`, `never_tier { id }`, `needs_acknowledgement { id }`,
  `cannot_recycle { id, fit }`, `running_app { id, warning: { app, pids, reason: "owns_cache"|"holds_files" } }`.
- `Decision { method, acks: { careful: number[], permanent, largePermanent, permanentInsteadOfRecycle: number[] }, skip: number[] }`.
  Maps 1:1 onto `flow::Decision` + `Acknowledgements`; **`skip` is new**: drop those queue ids
  from the plan before pre-flight/execute (deselected in review, or skipped in pre-flight).
  Never-tier ids are always in `skip`.
- `CleanErrorInfo { kind (CleanError serde tag), message: CleanError::message(), retryable: is_retryable(), path|null, holders: LockHolder[] }`.
- `LockHolder { pid, startTime, appName, exePath|null, service|null, kind: AppKind, restartable }`.
- `ItemVerdict { id, path, verdict: { status: "ready", recycle: RecycleFit } | { status: "blocked", error: CleanErrorInfo }, holders, runningApps }`.
- `CleanupProgress` (tag `event`): `started { items, bytes }`, `item_started { id }`,
  `item_finished { id, removed }`, `finished { summary }`.
- `ExecutionReport { actionId|null, results: { id, path, outcome }[], summary: { succeeded, failed, skipped, bytes, cancelled } }`;
  `outcome` (tag `kind`): `recycled { restoreItemId|null }`, `deleted { bytes }`, `failed { error }`, `skipped { reason }`.
- `UndoAction { actionId, kind: "cleanup"|"duplicates"|"tool", status: "in_progress"|"completed"|"partial"|"failed"|"cancelled"|"interrupted", startedMs, finishedMs|null, itemCount, doneCount, failedCount, bytesDone, items: UndoItem[] }`;
  `UndoItem { itemId, path, bytes, method: "recycle"|"permanent"|"reboot_delete"|"tool", tier, result: "pending"|"done"|"failed"|"skipped", error|null, completedMs|null, restorable, restoredMs|null }`.

### Tools (`lib/tools.ts`)

| Command | Args | Returns |
|---|---|---|
| `tools_status` | – | `ToolsStatus { recycleBins: { drive, items, bytes }[], compactOs: "compact"\|"not_compact"\|"unknown", hibernation: { enabled\|null, hiberfilBytes\|null }, shadowStorageBytes\|null }` |
| `tools_prepare` | `{ action: ToolAction }` | `ToolPrompt { promptId, title, description, commandLine, launch: "process"\|"shell_open"\|"elevated"\|"guidance_only", capturesOutput, removedFlags, expiresMs, recycleBin: { items, bytes }\|null }` |
| `tools_run` | `{ promptId, onOutput: Channel<{ stream: "stdout"\|"stderr", line }> }` | `{ exitCode\|null, output }` |

`ToolAction` (tag `kind`): `empty_recycle_bin { drive|null }`, `disk_cleanup { drive|null }`,
`storage_sense_settings`, `dism_component_cleanup`, `system_protection`, `hibernation_guidance`,
`uninstall { appId }` (backend looks up `UninstallString` by catalog id), `compact_os_status`.
`tools_prepare` builds `tools::CommandSpec` (and, for emptying the bin, the consent prompt with
the current count/size); `tools_run` refuses expired or guidance-only prompts.

### Insights (`lib/insights.ts`)

| Command | Args | Returns |
|---|---|---|
| `insights_largest` | `{ query: { volumeId, scope: number\|null, kind: "files"\|"folders", limit, sizeMode, filters: { extensions, categories, safety, minBytes, modifiedWithinDays\|null, untouchedForDays\|null } } }` | `{ entries: LargestEntry[], matched }`; `LargestEntry { id, name, path, bytes, modifiedMs\|null, category, safety\|null, app\|null, extension\|null }` |
| `insights_file_types` | `{ volumeId, scope, sizeMode }` | `{ byExtension: { extension, group, files, bytes, mismatched }[], byDetectedType: { label, files, bytes }[], totalBytes }` |
| `insights_categories` | `{ volumeId, scope, sizeMode }` | `{ category, bytes, files, top: { id, name, path, bytes }[] }[]` |
| `apps_footprint` | – | `AppFootprint[] { id, name, publisher\|null, version\|null, source: "registry"\|"appx"\|"launcher"\|"rule"\|"heuristic", confidence, evidence, totalBytes, locations: { kind: "install"\|"data"\|"cache"\|"logs"\|"updates"\|"other", path, volumeId, entryId\|null, bytes, safety\|null }[], registryEstimateBytes\|null, mismatch, canUninstall, cacheBytes, running }` |
| `apps_orphans` | – | `{ path, volumeId, entryId\|null, bytes, lastActivityMs\|null, guessedApp\|null, reason }[]` |
| `apps_queue_caches` | `{ appId }` | `QueueAddResult` |
| `recommendations_list` | – | `Recommendation[] { id, kind, title, summary, explain, bytes, items, safety, action: { kind: "queue" } \| { kind: "tool", tool: ToolAction } \| { kind: "view", view: "duplicates"\|"apps"\|"largest" } }`, ranked |
| `recommendations_preview` | `{ id, limit }` | `{ items: { volumeId, entryId, path, bytes, safety, explain }[], total }` |
| `recommendations_queue` | `{ id, exclude: number[] }` (entry ids) | `QueueAddResult` |

### Duplicates (`lib/dupes.ts`)

| Command / event | Args | Returns |
|---|---|---|
| `dupes_status` | – | `DupeScanStatus { state: "idle"\|"running"\|"done"\|"cancelled"\|"error", phase: "grouping"\|"partial_hash"\|"full_hash"\|null, progress: { filesDone, filesTotal, bytesDone, bytesTotal, etaSecs\|null }\|null, lastRunMs\|null, groups, wastedBytes, message\|null }` |
| event `dupes://status` | payload `DupeScanStatus` | – |
| `dupes_start` | `{ volumeIds: string[], minBytes }` | `null` |
| `dupes_cancel` | – | `null` |
| `dupes_groups` | `{ offset, limit }` | `{ total, groups: DupeGroup[] }`, by wasted bytes desc |
| `dupes_queue` | `{ selections: { groupId, fileIds }[] }` | `QueueAddResult`; **refuse any selection covering every copy** |
| `dupes_hardlink_prompt` | `{ selections }` | `{ promptId, message, files, bytesSaved, refusedCrossVolume, expiresMs }` |
| `dupes_hardlink` | `{ promptId }` | `{ replaced, bytesSaved, failed: { path, message }[] }` |

`DupeGroup { id, size, wastedBytes, files: { fileId, volumeId, entryId, path, modifiedMs|null, safety|null, inDownloads }[], keep: { fileId, reason: "oldest"|"shortest_path"|"not_in_downloads"|"user_rule", explain }, sameVolume }`.

### Activity (`lib/activity.ts`)

| Command | Args | Returns |
|---|---|---|
| `activity_status` | – | `{ enabled, running, needsHelper, throttled, cpuPercent\|null, sinceMs\|null, retentionDays }` |
| `activity_set_enabled` | `{ enabled }` | same (persists `activity.enabled`, starts/stops ETW) |
| `activity_top` | `{ window: "now"\|"hour"\|"today", limit }` | `WriterRow[] { image, name, bytesWritten, filesCreated, filesDeleted, topDirs: { path, bytesWritten }[] }` |
| `activity_dir_writers` | `{ volumeId, id, days }` | `WriterRow[]` (store `dir_writers`) |
| `activity_clear` | – | `null` |

### History (`lib/history.ts`)

| Command | Args | Returns |
|---|---|---|
| `history_snapshots` | `{ volumeId }` | `SnapshotInfo[] { id, takenMs, totalBytes, usedBytes, freeBytes, allocatedSum, logicalSum, files, dirs, scanner }`, oldest first |
| `history_usage` | `{ volumeId, fromMs\|null, toMs\|null }` | `{ snapshotId, atMs, totalBytes, usedBytes, freeBytes }[]` |
| `history_diff` | `{ fromId, toId, sizeMode, topN }` (`fromId` older) | `SnapshotDiff { from, to, usedDelta, scannedDelta, grown, shrunk, newLarge, deletedLarge }`; `DirChange { path, before: DirSizes\|null, after\|null, delta, entryId\|null }`, `DirSizes { allocated, logical, files }` |
| `history_dir_series` | `{ volumeId, id, lastN }` | `{ atMs, allocated\|null, logical\|null }[]`, oldest first |

### Settings, rules, data, helper, about (`lib/settings.ts`)

| Command | Args | Returns |
|---|---|---|
| `settings_load` | – | `Settings` (store shape) |
| `settings_save` | `{ settings }` | `{ settings, issues: SettingsIssue[] }`; non-empty `issues` = nothing written; backend applies side effects (autostart, tray, USN, ETW) |
| `settings_export` | – | `string` (the `strata-settings` JSON envelope) |
| `settings_import` | `{ json }` | `{ settings, issues }` (validate everything before writing) |
| `rules_list` | – | `RuleInfo[] { id, name, pack, source: "builtin"\|"user", category, safety, explain, action: "delete"\|"open_tool"\|"info_only", app\|null, regenerable, overriddenBy\|null }` |
| `rules_open_folder` | – | `null` |
| `rules_reload` | – | `{ builtin, user, problems: { file, message }[] }` |
| `rules_explain` | `{ path }` | `Explanation { path, result: { category, safety, ruleId\|null, regenerable }, rule: RuleInfo\|null, originPath\|null, steps: { path, ruleId\|null, category, safety }[], trace: string[] }` |
| `data_clear` | `{ what: "history"\|"activity"\|"caches" }` | `null` (app data only) |
| `helper_service_status` | – | `{ installed, running }` |
| `helper_service_install` / `helper_service_uninstall` | – | `{ installed, running }` (UAC) |
| `about_licenses` | – | `{ name, version, license, ecosystem: "cargo"\|"npm", repository\|null }[]` |
| `updates_check` | – | `{ current, latest\|null, available, channel }` |

`SettingsIssue { key: "section.field", message }` as in `strata_store::SettingsIssue`.

## Tests

`pnpm --dir ui test`: **147 tests** in 10 files (wave 1: 101). New:

- `src/lib/wave2.test.ts` (19): review rules (Careful ticks, never ignored, permanent and
  large-delete confirmations, cannot-recycle decisions, tier totals), refusal messages, duplicate
  guardrails (never all copies, never-tier copies, keep suggestion, whole-selection check),
  settings validation mirror and changed keys, diff pick ordering, chart math and formatting
  (nice/byte ticks, tick labels incl. locale grouping, gap paths, flat domains, deltas),
  extension parsing, app sorting.
- `src/views/features.test.tsx` (20): queue unavailable state; tier totals and "Not added";
  full flow (review acknowledgements, never-tier excluded, permanent confirmation, pre-flight
  lock holder, consent dialog before polite close, skip, execute with progress, results, retry);
  missing Recycle Bin shown before acting; undo restore; context-menu add reporting a never-tier
  refusal; duplicate guardrail selection and hardlink acknowledgement; settings round trip with
  validation and appearance applied, backend issues, path tester; history diff picker; charts
  (keyboard readout, data table, sparkline gaps); tools confirm-before-run and unavailable;
  largest files selection, never-tier refusal and filters; detail-panel "why" and history
  fallback; lazy routing and nav gating; nav queue badge.
- `src/views/insights.test.tsx` (7): file types click-to-filter, categories drill-through and
  unavailable state, apps confidence / evidence / mismatch / uninstall confirm, recommendations
  preview with deselection and queueing, activity opt-in and clear.

Bundle (`pnpm --dir ui build`): entry 121.7 kB gzip (budget 130 kB); every new view is its own
lazy chunk (1.1–6.4 kB gzip) and `check-bundle.mjs` now fails if any of them is merged back.

## Accessibility

- Every view is a `section` with an `h1`; F6 reaches it. Tabs follow the ARIA tabs pattern
  (arrow keys); expandable groups use `aria-expanded`/`aria-controls`.
- `ConfirmDialog`: `role="dialog"`, `aria-modal`, focus starts on **Cancel** (Enter never
  confirms a destructive action by accident), Tab trapped, Escape cancels, focus returns.
- Safety badges carry text plus a distinct shape per tier (circle, square, triangle, cross);
  category bars use the fixed palette and the color-blind patterns; chart series differ by
  dash pattern too; `forced-colors` and `prefers-reduced-motion` handled.
- Charts are `role="img"` with summary labels, a keyboard cursor (arrows / Home / End) announced
  through a polite live region, and a real data table (visually hidden, or shown with
  "Show as table"). Bar lists are real lists with label and value text.
- Progress uses `role="progressbar"` with values; results and status lines are live regions;
  form errors are linked with `aria-describedby` and `aria-invalid`.
- Layout wraps down to 900×600 (cards, toolbars, tables with wrapping paths).

## Decisions

- **Separate `FeaturesContext` defaulting to the Tauri implementation** instead of growing
  `Services`: no wiring in `services.tsx` (being edited in parallel), tests inject fakes.
- **The backend owns the queue.** It resolves ids to identities and tiers and refuses never-tier
  / never-list items authoritatively; the UI mirrors it and shows refusals verbatim.
- **`Decision.skip`** carries deselections and pre-flight skips, so a plan is built once and the
  user's choices travel with each pre-flight/execute call.
- **Two-step consent for anything that acts outside the app** (polite close, tools, hardlinks):
  the backend mints a prompt with its own text and expiry; the UI shows it and only then calls
  the action with the prompt id. Mirrors `consent::Prompt`/`Consent`.
- **`Settings` uses the store's snake_case shape** so it passes through unchanged and issue keys
  name fields directly. Validation is mirrored for instant feedback; backend issues win.
- **Errors are flattened** to `CleanErrorInfo` (`message()` + `is_retryable()`), so the UI never
  re-implements `CleanError` wording.
- **Capability polling**: `app_capabilities` loads asynchronously and `Services` has no change
  event, so `useCapability` re-reads the set every 250 ms while it is empty (max 10 s) instead of
  flashing "unavailable".
- **Charts are hand-written SVG** (≈2 kB gzip with the math) rather than a chart library.
- **Feature styles live in `views/features.css`**, imported once from `components/feature.tsx`
  (CSS doesn't count toward the JS entry budget).

## Blockers

None on the UI side. Every view waits on the commands above.

## Backend requests

Implement the commands above (and list them in `app_capabilities`). Notes:

1. `cleanup_queue_add` now returns `QueueAddResult`; keep the queue in the backend and emit
   `cleanup://queue-changed` after every change. Refuse never-tier and never-list entries there.
2. Honour `Decision.skip` in `cleanup_preflight` / `cleanup_execute`; keep a plan per `planId`
   until a new plan replaces it.
3. Flatten `CleanError` to `CleanErrorInfo`; send `restoreItemId` (the undo-log item id) in
   `recycled` outcomes so Restore works straight from results.
4. Consent prompts (`cleanup_close_prompt`, `tools_prepare`, `dupes_hardlink_prompt`) should
   expire (≤ 120 s) and be single-use.
5. `dupes_queue` and `dupes_hardlink_prompt` must refuse selections covering every copy of a
   group and never-tier copies (the UI already prevents it).
6. `settings_save` applies side effects (launch at login, tray, USN, ETW) and returns the stored
   settings.
