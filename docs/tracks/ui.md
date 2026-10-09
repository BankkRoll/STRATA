# Track: UI (`ui/`)

Frontend for M4 (treemap, list, detail, home) and M9 (other views): React 19 + TypeScript +
Zustand, raw WebGL2 for the visual views, Canvas2D labels, TanStack Virtual for the tree-table.
Everything that needs the engine goes through typed contracts (below). Commands that are not in
the backend yet surface as designed "not available" states with a reason; nothing is faked.

## Status (SPEC §16)

| Item | State |
|---|---|
| App shell §16.4: top bar (breadcrumbs, palette trigger, size-mode toggle, color/style), left nav, visual area + split list, right detail, status bar | Done |
| Responsive to 900×600: nav → icon rail < 1100 px, detail → drawer, list capped on short windows | Done (screenshot-checked at 874×600 viewport) |
| Native title bar | Kept (existing decision in `DECISIONS.md`) |
| Keyboard: every control reachable, F6/Shift+F6 cycle regions, skip link, visible focus, `prefers-reduced-motion`, forced-colors | Done |
| Shared state (Zustand): volume, root path, selection, size mode, color mode, style, filters, view, panes | Done; switching views keeps context |
| §16.1 WebGL2 treemap: instanced quads straight from the 32-byte records, flat + cushion, hatched aggregates, borders, depth tint, hover/selection | Done |
| Color modes: category (+ patterns), file type, age heatmap, owning app, safety tier, changed recently (pulse) | Done (one packed `color_key`, switching is a uniform) |
| DPR-aware sizing, relayout on resize and DPR change (`devicePixelContentBoxSize`, `matchMedia(resolution)`) | Done |
| Context loss / restore | Done (state shown, resources rebuilt, frame re-uploaded) |
| Labels overlay (Canvas2D): ellipsis, collision culling, DPR-aware text | Done (treemap, icicle/flame, bubbles) |
| JS picking mirroring Rust `pick` incl. wide-directory grids | Done, equal to Rust on every fixture sample |
| Tooltip (name, both sizes, %, items, category, app, modified, safety) via `EntryInfoProvider` | Done |
| Click select, Ctrl+click toggle, double-click/Enter drill with GPU-tweened transition, Backspace/mouse-back up, breadcrumbs | Done |
| Wheel zoom about cursor (GPU), relayout at the new transform after 140 ms idle; middle-drag pan; `+`/`-`/`0` | Done |
| Marquee drag-select | Done (maximal fully-contained selectable records) |
| Context menu with all §16.1 items via typed `CommandBus`; unavailable ones disabled with reason | Done |
| §16.2 sunburst (vertex-tessellated arcs), icicle/flame, bubbles (SDF circles), mind map (circles + edge quads) | Done; lazy-loaded renderers |
| §16.2 list: virtualized treegrid, sortable, resizable, choosable columns, %-of-parent bar, arrows/expand/collapse/type-ahead, copy (TSV) / export (CSV) | Done |
| Accessible table alternative of every visual view ("Show as table", always present for screen readers) | Done |
| §16.3 detail panel: all sections with empty / unknown / loading / error / partial states | Done |
| §16.2 home: capacity bars (category-colored when scanned, unaccounted block shown), FS badge, state chip, Scan/Cancel/Open, elevation banner, since-last-scan banner | Done |
| §17 palette shell (Ctrl+K / Ctrl+F), fuzzy commands + streamed file search | Done |
| Formatting utils | Done |
| Tests (unit, fixture-based picking, components, bundle budget) | 101 tests + bundle check |

Not done (see "Next steps"): arc/circle view transitions, sunburst/mind-map labels, mind-map
expand/collapse, settings screen, largest-files/file-types/apps/categories/recommendations views.

## Layout

```
ui/src/
  lib/layout/frame.ts      decode layout frames (zero-copy typed-array views)
  lib/layout/pick.ts       picking (rects nested/stacked, arcs, circles), marquee
  lib/layout/navigate.ts   keyboard model over a frame
  lib/layout/transform.ts  visual-zoom math
  lib/layout/stream.ts     LayoutStream contract + Tauri implementation
  lib/{entries,rows,detail,volumes,search,commands,backend}.ts   backend contracts
  lib/{format,palette,types,hooks}.ts
  render/                  GL boundary (renderer.ts), Rect/Arc/CircleRenderer, labels, ViewController
  components/, views/      React UI
  store/                   app, settings, hover (vanilla), volumes
  dev/                     DEV-ONLY fixture harness (excluded from production builds)
  lib/layout/__fixtures__/ exported strata-layout frames + Rust pick results (tests, harness)
ui/scripts/check-bundle.mjs  bundle budget (runs in `pnpm build`)
ui/scripts/run-harness.mjs   headless-browser harness runner (shader check + benchmarks)
crates/strata-layout/examples/export_fixtures.rs   fixture exporter + reference frame encoder
```

Fixtures: `pnpm --dir ui fixtures` (small, committed) and `pnpm --dir ui fixtures:large`
(1M nodes / 503k rects, ~40 MB, git-ignored). Harness: `pnpm --dir ui dev`, open
`/?fixture` or `/?fixture=large` (`&view=sunburst|icicle|flame|bubbles|mindmap`), or run
`node ui/scripts/run-harness.mjs [small|large] [view]`.

## TS ↔ backend contract

All commands use Tauri's camelCase argument names. JSON unless marked binary. Every command
the UI uses is optional at runtime: `app_capabilities` tells the UI which exist; a missing
command shows a designed unavailable state.

### Commands

| Command | Args | Returns | Used by |
|---|---|---|---|
| `app_info` | – | `AppInfo` (exists) | shell |
| `app_capabilities` | – | `string[]` of implemented command names | command availability |
| `layout_open` | `{ onFrame: Channel<ArrayBuffer> }` | `{ streamId: number }` | each visual view |
| `layout_request` | `{ streamId, seq, request: LayoutRequest }` | `null`, **resolves after the frame for `seq` was sent** | view controller |
| `layout_close` | `{ streamId }` | `null` | unmount |
| `entry_info` | `{ volumeId, ids: number[] }` (≤ 512) | `EntryInfo[]` (missing ids omitted) | tooltip, labels, breadcrumbs, a11y table |
| `entry_path` | `{ volumeId, id }` | `string` (Win32 path, no `\\?\`) | Copy path |
| `entry_detail` | `{ volumeId, id }` | `EntryDetail` | detail panel |
| `list_children` | `{ query: RowQuery }` | **binary** row page (`InvokeResponseBody::Raw`) | list pane |
| `apps_brief` | – | `{ id, name }[]` | app column |
| `list_volumes` | – | `VolumeInfo[]` | home, breadcrumbs, status |
| event `volumes://changed` | payload `VolumeInfo[]` (full list) | – | hot-plug and scan-state updates |
| `helper_status` | – | `{ elevated, mode: "none" \| "on_demand" \| "service" }` | elevation banner |
| `helper_elevate` | – | `HelperStatus` (UAC prompt) | "Enable fast scan" |
| `scan_start` | `{ volumeId, mode: "auto" \| "fast" \| "standard" }` | `null` (progress via `volumes://changed`) | Scan |
| `scan_cancel` | `{ volumeId }` | `null` | Cancel scan |
| `history_since_last_scan` | `{ volumeId }` | `SinceLastScan \| null` | home banner |
| `search_open` | `{ onResults: Channel<SearchBatch> }` | `{ streamId }` | palette |
| `search_query` | `{ streamId, seq, query: SearchQuery }` | `null`; batches carry `seq` | palette (debounced 30 ms) |
| `search_close` | `{ streamId }` | `null` | palette close |
| `entry_action` | `{ action: "open" \| "reveal" \| "properties" \| "openTerminal", volumeId, ids }` | `null` | context menu |
| `cleanup_queue_add` | `{ volumeId, ids }` | `null` | Add to cleanup |
| `recycle_bin_empty` | – | `null`; **the backend must show its own confirmation** | palette |

Types are in `ui/src/lib/*.ts` with TSDoc (`LayoutRequest` in `lib/layout/stream.ts`,
`EntryInfo` in `lib/entries.ts`, `RowQuery`/`Row` in `lib/rows.ts`, `EntryDetail` in
`lib/detail.ts`, `VolumeInfo`/`HelperStatus`/`SinceLastScan` in `lib/volumes.ts`,
`SearchQuery`/`SearchBatch`/`SearchResult` in `lib/search.ts`). Enum strings use the
`strata-core` serde names (`allocated`, `probably`, `ai_models`, …). Times are Unix ms UTC
(`number | null`). Sizes are JSON numbers (exact below 2^53).

`LayoutRequest`: `{ volumeId, root, view: "treemap"|"sunburst"|"icicle"|"flame"|"bubbles"|"mindmap",
width, height (device px), dpr, transform: { scale, tx, ty }, sizeMode, style: "flat"|"cushion",
filters: { excluded: number[], categories: number[], minBytes, modifiedWithinDays: number|null },
animate: boolean }`.

`VolumeInfo` is the UI's proposal (no platform-track type has merged yet):
`{ id (volume GUID path), mountPoints[], label, filesystem: "NTFS"|"ReFS"|"FAT32"|"exFAT"|"FAT"|"network"|"other",
devDrive, kind: "fixed"|"removable"|"network"|"cdrom"|"ramdisk"|"unknown", isSystem, totalBytes,
freeBytes, clusterSize, serial, bitlocker: "none"|"unlocked"|"locked", present,
scan: { state: "never"|"scanning"|"live"|"stale"|"partial", progress: { entries, bytes,
fraction|null, etaSecs|null }|null, lastScanMs|null, scanner: "mft"|"walker"|null, rootId|null },
categoryBytes: { [categoryId]: allocatedBytes } | null }`.

### Layout frame (binary, one Channel message per layout)

Little-endian. Header 128 bytes, then five sections, each starting 16-byte aligned:

| Offset | Type | Field |
|---|---|---|
| 0 | u32 | magic `"STLF"` (`0x464C5453`) |
| 4 | u16 | version = 1 |
| 6 | u16 | view: 0 treemap, 1 icicle, 2 flame, 3 sunburst, 4 bubbles, 5 mind map |
| 8 | u32 | `seq` echoed from the request |
| 12 | u32 | root entry id |
| 16 | f32 ×3 | width, height (device px), dpr |
| 28 | u32 | flags: bit0 has cushions, bit1 has transitions |
| 32 | f64 ×3 | transform scale, tx, ty the layout was computed under |
| 56 | f32 ×3 | sunburst center x, y, ring width (0 otherwise) |
| 68 | u32 | reserved (0) |
| 72 | (u32 offset, u32 length) ×5 | sections: nodes, aggregates, labels, cushions, transitions |
| 112 | u64 | root size in the active size mode |
| 120 | 8 bytes | reserved (0) |

Section payloads are exactly `strata-layout`'s `as_bytes()` (formats in
`docs/tracks/layout.md`): `RectLayout::rects()/aggregates()/labels()/cushions()`,
`transition_rects(old, new)`; `ArcLayout::arcs()/aggregates()`; `CircleLayout::circles()/
aggregates()/labels()`. Empty section = length 0. The reference encoder is `write_frame` in
`crates/strata-layout/examples/export_fixtures.rs`; the UI decoder rejects bad magic, version,
view kind, out-of-bounds or misaligned sections, and cushion sections not parallel to nodes.

### Row page (binary `list_children` response)

Header 32 bytes: `magic u32 "STRP" (0x50525453) | version u16 = 1 | reserved u16 | parent u32 |
total u32 (children after filters) | offset u32 | count u32 | namesOffset u32 | namesLength u32`.
Then `count` × 64-byte rows, then the UTF-16LE name blob (raw `WideName` units; the UI decodes
unpaired surrogates to U+FFFD):

| Offset | Type | Field |
|---|---|---|
| 0 | u32 | id |
| 4 | u32 | parent id |
| 8 | u32 | `EntryFlags` bits |
| 12 | u16 | category |
| 14 | u8 | safety: 0 unclassified, 1 safe, 2 probably, 3 careful, 4 never |
| 15 | u8 | reserved |
| 16 | u64 | allocated |
| 24 | u64 | logical |
| 32 | u32 | items below (dirs) |
| 36 | u32 | direct children (dirs) |
| 40, 44, 48 | u32 ×3 | mtime, ctime, atime: seconds since 2000-01-01 UTC, 0 = unknown |
| 52 | u32 | app id (0 = none; names from `apps_brief`) |
| 56 | u32 | name offset (UTF-16 units into the blob) |
| 60 | u32 | name length (UTF-16 units) |

`RowQuery`: `{ volumeId, parent, sort: { key: "name"|"size"|"items"|"modified"|"created"|"accessed"|"category"|"safety"|"app", desc }, sizeMode, offset, limit (200), filters }`.
`size` sorts by the active `sizeMode`; ties break by id so pages are stable.

### `color_key` encoding (u32, packed, all modes at once)

The backend's `LayoutSource::color_key` returns:

| Bits | Field | Values |
|---|---|---|
| 0–3 | category | `strata_core::Category` discriminant (0–14) |
| 4–6 | safety | 0 unclassified, 1 safe, 2 probably, 3 careful, 4 never |
| 7–11 | age bucket | 0 unknown, 1–20 by modified age (`AGE_BUCKETS` in `lib/palette.ts`: <1 h, <6 h, <1 d, <3 d, <7 d, <14 d, <30 d, <60 d, <91 d, <182 d, <274 d, <365 d, <548 d, <730 d, <1095 d, <1461 d, <1826 d, <2557 d, <3652 d, older), 31 suspicious timestamp |
| 12–19 | file-type slot | 0 = directory/none, 1–255 = extension group slot assigned by the backend |
| 20–29 | app slot | 0 = unattributed, 1–1023 = app slot assigned by the backend |
| 30 | changed recently | set while a live update touched the entry within the decay window |
| 31 | reserved | 0 |

Directories use their dominant category (largest child bytes) and their newest-modified age.
Because every mode is in the key, changing the color mode never relayouts. Slots map to fixed
golden-angle hues; the legend names come from the tooltip (`EntryInfo.app`).

## Performance

Headless Edge (Chromium, ANGLE/D3D11) on a desktop with a mid-range discrete GPU, via
`node ui/scripts/run-harness.mjs`. Headless Chromium paces `requestAnimationFrame` at 100 Hz,
so 100 fps is the ceiling of this measurement; every run held it with no dropped frames.
5-second sweeps, rendering every frame while moving the hover target (and zoom/pan):

| Fixture | Instances | Sweep | fps | p95 / p99 frame | draw call CPU | label pass | pick (incl. `getBoundingClientRect`) |
|---|---|---|---|---|---|---|---|
| 1M-node tree, LOD off (`fixtures:large`) | 503,464 | hover | 100.0 | 10.1 / 10.2 ms | 0.03 ms | skipped (inputs unchanged) | 26 µs |
| same | 503,464 | zoom 1×–8× | 100.0 | 10.1 / 10.2 ms | 0.03 ms | 0.14 ms | 18 µs |
| same | 503,464 | pan at 4× | 100.0 | 10.1 / 10.2 ms | 0.03 ms | 0.08 ms | 20 µs |
| 4k-node tree, default LOD | 1,242 | hover / zoom / pan | 100.0 | 10.1 / 10.2 ms | 0.03 ms | ≤ 0.15 ms | 15–26 µs |

- Frame ingest for the 503k-rect frame (skip table + wide-dir grids + GPU upload): 15 ms, once
  per layout, never per frame.
- Pure hit-test cost (vitest, node): < 0.05 ms per query on the 3,000-child flat folder, the
  worst case for sibling scans (grid path).
- Bundle: entry chunk 112 kB gzip (React + ReactDOM are ~60 kB of it); sunburst renderer
  1.3 kB, circle renderer 1.9 kB and palette 2.9 kB gzip load lazily. `pnpm build` fails if the
  entry exceeds 130 kB gzip, a lazy chunk is merged back, or dev-harness code leaks in.
- No React render per frame: hover writes a vanilla store; only the tooltip subscribes and it
  follows the pointer imperatively. Hover/selection changes are a uniform / one byte per
  instance; labels redraw only when frame, transform, theme, units or names change.

Shader compilation: the harness compiles every program (rect, rect-transition, arc, circle,
mind-map edge) on load and reports `ok` per view kind; checked on the GPU above. jsdom cannot
run WebGL, so component tests use a recording `FakeRenderer` behind the `ViewRenderer`
interface.

## Accessibility

- Landmarks: banner (top bar), Views nav, Breadcrumb nav, main, Details complementary, status.
  Skip link; F6 / Shift+F6 cycle regions.
- Visual views: focusable `role="application"` region with instructions; arrows move between
  items (tree model: Left/Right siblings, Down child, Up parent, Home/End), Enter drills,
  Backspace up, Space selects, Shift+F10 / Menu key opens actions, `+`/`-`/`0` zoom; a polite
  live region announces the focused entry. "Show as table" toggles a real `<table>` of the
  current level, which is always present (visually hidden) for screen readers.
- List: ARIA treegrid with `aria-level/posinset/setsize/expanded/selected/rowindex`,
  `aria-sort` headers, keyboard-resizable column separators, roving tabindex, type-ahead.
- Context menu and palette follow the ARIA menu and combobox/listbox patterns; disabled items
  stay focusable/visible with `aria-disabled` and their reason in text.
- Color: fixed category palette (light + dark) plus a distinct pattern per category
  (toggle "Patterns"); safety uses Okabe–Ito hues; age uses a viridis-like ramp.
- `prefers-reduced-motion`: no drill tweens, no pulse, no CSS animation. `forced-colors`
  outlines selection.

## Decisions

- **One binary frame per layout** (header + sections) instead of one Channel message per
  buffer: a frame is atomic, `seq` makes stale frames droppable, and sections stay zero-copy.
- **Packed `color_key`** with all modes, so color-mode switches are free and need no relayout.
- **Subtree skip table rebuilt in JS** from `parent` (O(n)) rather than adding it to the wire;
  verified equal to Rust's `subtree_end` on every fixture.
- **Picking mirrors Rust exactly**, including `f32` edge rounding (`Math.fround`) and
  per-directory grids for > 256 children; JS == Rust on all 1,172 sample points per fixture.
- **Transitions only for rect views** (the backend's `transition_rects`). Arc/circle views
  switch without a tween for now.
- **Opaque canvas, no MSAA**: rect edges are pixel-aligned and circles anti-alias in the
  shader; sunburst edges alias slightly at 100% scale.
- **Visual zoom is GPU-only until 140 ms after the last wheel tick**, then the treemap relayouts
  at the new transform; other views only zoom visually.
- **Marquee selects maximal fully-contained entries** (dragging around a folder selects the
  folder, not its files).
- **Tauri services are injected** (`ServicesContext`), so tests and the dev harness run the
  real components on fixture data while production code has no fixture path at all.
- **`@types/node` dev dependency** only for test helpers that read fixture files.

## Blockers

None. Data-dependent features wait on the backend commands above.

## Backend requests (for the lead)

1. **Layout pipe in `src-tauri`**: `layout_open` creates a stream (per window/view) holding the
   last `RectLayout`/`ArcLayout`/`CircleLayout`; `layout_request` lays out with the request's
   viewport/dpr/transform/style (`TreemapConfig::new(width, height, dpr).with_view(...)`, set
   `cushion` when `style == "cushion"`), filters and size mode, computes
   `transition_rects(prev, new)` when `animate` and the view is treemap/icicle/flame, encodes the
   frame with `write_frame` (copy from the example) and sends it as
   `InvokeResponseBody::Raw` on the Channel, then resolves. Coalesce: if a newer `seq` is queued
   for the stream, skip older ones (the UI already keeps at most one in flight).
2. **`LayoutSource::color_key`** on the index per the packed encoding above; keep
   `strata_core::Category` discriminants stable.
3. **`entry_info` / `entry_path` / `entry_detail`** from the index (+ classifier/attribution/
   store when merged). `EntryDetail.history` from `strata-store` snapshots; `times.accessUnreliable`
   from the `disablelastaccess` check.
4. **`list_children`** binary row page as specified; sort server-side with id tie-break.
5. **`list_volumes` + `volumes://changed`, `helper_status`, `helper_elevate`, `scan_start`,
   `scan_cancel`, `history_since_last_scan`** (platform/scanner/store tracks). If the platform
   track's `VolumeInfo` differs, map it to the shape above in `src-tauri` or tell me to adapt.
6. **`app_capabilities`**: return the names of every command registered above, so the UI
   enables exactly what exists.
7. **`search_open/query/close`** with `SearchBatch` JSON batches (≤ 256 results, `done` on the
   last batch, `seq` echoed).
8. **`entry_action`, `cleanup_queue_add`, `recycle_bin_empty`** when the clean/shell work lands;
   the backend owns confirmations for destructive tools.
9. Optional layout-crate additions: arc/circle transition sections (pairs from
   `match_records` plus from/to geometry) so sunburst/bubbles can tween; label candidates for
   sunburst and mind map.

## Next steps

- Wire the contracts above as backend commands land; delete nothing on the UI side.
- Arc/circle tweening and sunburst/mind-map labels (needs request 9).
- Mind-map expand/collapse (per-node `max_children` override in `LayoutRequest`).
- Remaining §16.2 views (largest files, file types, apps, categories, recommendations,
  timeline, activity, duplicates) and the settings screen; persist appearance settings to
  `strata-store`.
- E2E with tauri-driver once the backend serves a fixture volume.
- Benchmark on the target WebView2 with vsync (headless caps at 100 Hz) and on an integrated GPU.
