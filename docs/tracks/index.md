# Track: index (`strata-index`)

Status: **M3 index done, M5 live-update API done, M8 search core done.** All tests pass,
clippy is clean (`-D warnings`), and the crate is `#![forbid(unsafe_code)]`.

## What's done (by SPEC section)

| SPEC | Delivered |
|---|---|
| Â§2 memory â‰¤ 64 B/entry | **58.7 B/entry at 1M, 59.2 at 5M** (11.4% directories), all side tables included and names excluded. Lite mode: 42.7. |
| Â§2 search â‰¤ 50 ms / 5M | First batch **â‰¤ 1.1 ms**; complete scan **â‰¤ 28 ms** for every query tried. No trigram index needed. |
| Â§6.2 orphans / stale refs / cycles / metadata | Missing or stale parent (sequence mismatch), a non-directory parent, a reparse-point parent or no links all mean `ORPHAN` under "Orphaned entries". Parent cycles mean `CYCLE_BROKEN` (every member of the loop) under the same node. `NTFS_METADATA` entries go under "NTFS metadata" but keep internal structure (`$Extend\$UsnJrnl`). The real parent stays queryable through `intended_parent`. |
| Â§7 accounting | Logical = `Sizes::total_logical`, allocated = `Sizes::total_allocated`. Both are stored, so switching modes is free. Directory overhead and ADS count toward the directory/file itself. |
| Â§7.1 aggregates | Subtree logical, allocated, files, dirs, newest/oldest mtime, and the largest descendant **in each mode**, plus effective `PARTIAL`. Computed bottom-up in parallel (level by level), and incrementally in O(depth). |
| Â§7.2 hardlinks | One entry per link; `links[0]` is primary, the others get `HARDLINK_SECONDARY` and contribute 0. `IndexOptions::split_hardlinks` splits bytes evenly, with the remainder going to the primary, so totals stay exact. |
| Â§7.3 reparse | Traversal-blocking reparse points (symlink, junction, mount point, ...) are leaves. Anything claiming one as its parent is orphaned. |
| Â§5/Â§7.5 virtual blocks | `IndexBuilder::add_virtual_block` and `Index::{add,set,remove}_virtual_block` (e.g. "Unaccounted / system reserved", "System Restore / Shadow copies"). They add bytes but are never counted as files. |
| Â§9.1 layout, cache, lite | SoA columns, u32 ids, `u32::MAX - 65536` limit (refused gracefully), lossless WTF-8 names, mmap-friendly LE cache with xxh3 per section, lite mode. |
| Â§9.2 queries | Children sorted by size/count/name (natural, case-insensitive)/modified/category with paging and dirs-first; path with LRU; heap top-N (global or under X); combinable filters; extension and category breakdowns; lookup by `FileRef`. |
| Â§10.2 live updates | `upsert`, `remove`, `apply(batch)` â†’ `ChangeSet` (created/updated/removed + new aggregates of every changed dir). |
| Â§13 | `SUSPICIOUS_TIME` via `FileTime::is_suspicious(now)` (zero = unknown is never flagged). Suspicious and unknown times are excluded from newest/oldest. |
| Â§17 search | Full query language (below), parallel streaming scan, cancel token, relevance or size ranking, pluggable safety/app providers. |
| Â§21 | Unpaired surrogates, case-variant names, hardlinks, sparse/huge sizes (â‰¥ 4 GiB spill), junction "cycles", parent cycles, orphans, access-denied partial totals. |
| Â§22 | Property tests (below), plus unit and integration tests and criterion benchmarks. |

## Public API (crate root)

- `IndexBuilder::new(IndexOptions)`, `push`, `push_batch`, `reserve`, `set_partial`,
  `add_virtual_block`, `finish() -> Index`, `finish_with_stats() -> (Index, BuildStats)`.
- `Index` accessors: `root`, `orphans_node`, `metadata_node`, `len`, `slot_count`, `is_live`,
  `parent`, `intended_parent`, `children`, `child_count`, `flags`, `is_dir`, `file_ref`, `name`
  (lossless `WideName`), `name_lossy`, `name_wtf8`, `own_logical`/`own_allocated`/`own_size`,
  `contribution(id, mode)`, `size(id, mode)` (subtree for dirs), `times`, `category`/`set_category`,
  `owner_app`/`set_owner_app`, `ext_id`, `extension`, `extension_id`, `aggregate -> DirAggregate`,
  `lookup(FileRef)`, `links(FileRef)`, `link_count`, `memory_report`, `check_invariants`.
- Live: `upsert(ScanRecord)`, `remove(FileRef)`, `apply(impl IntoIterator<Item = Update>)` â†’
  `ChangeSet { created, updated, removed, aggregates: Vec<(EntryId, DirAggregate)> }`;
  `set_now`, `set_usn_position`, `tombstones`, `compact() -> IdRemap`.
- Queries: `children_sorted(dir, &ChildQuery)`, `path`, `path_string`, `top_n(scope, n, EntryKind,
  mode, Option<&Filter>)`, `filter_entries`, `for_each_in_subtree`, `extension_breakdown`,
  `category_breakdown`.
- Search (`strata_index::search`): `Query::parse(text, now)`, `Query::parse_regex` (UI regex toggle),
  `Index::search(&Query, &SearchOptions, &CancelToken, on_batch) -> SearchOutcome`.
  `SearchOptions { limit, sort: Relevance | Size, mode, chunk, safety: Option<&SafetyFn>,
  app_matches: Option<&AppMatchFn> }`.
- Cache: `Index::save(path)` (atomic tmp+rename), `Index::load(path)`, `to_bytes`/`from_bytes`,
  `cache::read_header` (check serial/journal before a full load), `CacheError`.

### Query language

Name terms: `foo` (substring), `^foo` (prefix), `*.gguf` / `img_??.png` (wildcard, whole name),
`re:...` or `/.../` (regex), `node_modules\react` (trailing path components, exact or wildcard;
`/` also separates), `"quoted phrase"`. Filters: `size:>1gb | <10mb | 1mb..5mb | 5mb` (binary
units b/kb/mb/gb/tb, decimals), `modified:<30d | >1y | >2024-01-01 | 2024-01-01..2024-06-30 | today`
(h/d/w/m=30d/y), `ext:mp4,mkv`, `app:`, `cat:` (prefix of category name), `safe:yes|no|<tier>`,
`dir:`/`file:` (optional value adds a term), `vol:D`, `attr:hidden,system,...,junction`,
`cloud:online|local|pinned`, `case:yes`. Everything ANDs together, and comma lists inside a
filter are alternatives. Case-insensitive by default. A token whose key isn't a known filter is a
name term. For directories, `size:` and `modified:` use the subtree total and the newest mtime.

## Tests

79 total: 20 unit, 18 build, 15 live (4 property tests), 6 query, 8 search, 5 cache, 7 doctests.

- **Property tests** (`tests/live.rs`): random sequences of up to 60 ops (create file/dir,
  delete, rename, move (including into its own subtree), resize, touch (including suspicious and
  unknown times), hardlink add/remove, record reuse with a new sequence (and kind flip), flag
  toggles (PARTIAL, ACCESS_DENIED, HIDDEN, NTFS_METADATA, junction), dangling/stale parent).
  They are applied through the live path and compared with a fresh build of the final record set:
  identical canonical forms (identity = file ref + name, parent identity, flags, sizes, times,
  intended parent, aggregates with largest-descendant *sizes*). Each run also checks
  `check_invariants`, compaction and a cache round trip. Variants: split-hardlink mode, and
  checking every intermediate state. Default 400/400/64/64 cases; `STRATA_PROPTEST_CASES=4000`
  soak passed (release and debug).
- The cache test flips one byte every 97 bytes across the whole file. Every flip outside
  inter-section padding is rejected, and truncated or garbage input fails cleanly.

## Benchmarks

Machine: 16-thread desktop CPU, 32 GB RAM, Windows 11, release profile. Other workloads shared
the machine, so timings are indicative (Â±30% run to run). Data comes from a deterministic synthetic
volume (`benches/support/synth.rs`): 45% node_modules-heavy dev tree, 30% WinSxS-like system tree
with hardlinks into System32, 15% media library, the rest documents and hex-named caches;
11.4% directories, about 16.5 name bytes/entry. Reproduce with `cargo bench -p strata-index`
(`STRATA_BENCH_CRITERION=0` gives the report only, `STRATA_BENCH_MAX=1000000` skips 5M).

| entries | mode | staging (push) | finish | resolve | cycles | layout | aggregate | B/entry excl. names |
|---|---|---|---|---|---|---|---|---|
| 1,000,002 | full | 485 ms | 113 ms | 6 | 4 | 91 | 12 | **58.72** |
| 1,000,002 | lite | 505 ms | 128 ms | 6 | 4 | 107 | 10 | **42.72** |
| 5,000,002 | full | 2.8 s | 628 ms | 26 | 21 | 538 | 42 | **59.22** |

Staging includes cloning records and WTF-8 encoding. Criterion `build/1m_records`
(push + finish) took 649 ms.

Memory formula: about 52.3 B per entry plus 48 B per directory, so â‰¤ 64 holds up to roughly 24%
directories. At 5M the split is columns 240 MB, tree 47 MB, maps 6.8 MB, name index 1.9 MB, and
name bytes 83 MB (excluded).

Search, 5M entries (median of 7, default chunk 16k entries):

| query | first batch | complete | matches |
|---|---|---|---|
| `react` | 0.65 ms | 22.6 ms | 56,845 |
| `^index` | 0.59 ms | 21.1 ms | 101,442 |
| `*.gguf` | (no hits) | 27.2 ms | 0 |
| `*.mp4` | 1.08 ms | 25.3 ms | 2,290 |
| `IMG_0*.jpg` | 1.08 ms | 24.7 ms | 503 |
| `node_modules\react*` | 0.73 ms | 27.9 ms | 870 |
| `size:>100mb` | 0.30 ms | 9.2 ms | 8,860 |
| `ext:dll size:>1mb` | 0.33 ms | 7.8 ms | 32,345 |
| `re:^DSC\d{5}\.NEF$` | 1.44 ms | 24.7 ms | 49,902 |
| `zzzz-no-match` | (no hits) | 20.9 ms | 0 |

Other numbers:

- Name encoding, 1M names, single thread, needle `react`: UTF-16 takes 31.0 MB and scans in
  53.5 ms; WTF-8 takes 15.5 MB and scans in 26.9 ms.
- Children of a 100k-entry directory (criterion, first 200): by size 8.0 ms, by name 12.8 ms.
  Full sort: 11.4 ms by size, 20.8 ms by name.
- Top-100 files: 3.7 ms at 1M (criterion), 12.2 ms at 5M. Extension breakdown: 0.9 ms at 1M,
  3.0 ms at 5M.
- Paths: about 2 Âµs each, cold.
- Live updates, 1M index, 100k mixed (60% resize, 20% create, 10% rename, 10% delete) in
  batches of 1000: **216kâ€“334k updates/s**. Criterion: 1000 resizes take 2.7 ms.
- Cache, 1M entries: 75.2 MB, serialize 42â€“51 ms, load + full validation 26â€“43 ms.

## Decisions

- 2026-10-09 â€” **No `FileRef â†’ EntryId` hash map for the bulk of entries.** The base region is
  sorted by key (directories first, then files), and lookup is a binary search over the
  `file_ref` column. Live-created entries use a small delta map. A hash map would cost 6â€“11
  B/entry and blow the 64 B budget. Removed base slots therefore become tombstones, not free-list
  entries, because reuse would break the sort order. `compact()` (an `IdRemap` is returned)
  reclaims them; call it when `tombstones()` exceeds about 25% of slots, or before saving.
- 2026-10-09 â€” **Lookup key = MFT record number** (full value for synthetic walker ids). The
  stored reference is compared for the sequence check, so a reused record replaces its stale
  predecessor, and children holding the old sequence become orphans.
- 2026-10-09 â€” **Sizes stored as `u32` with a spill map** for values â‰¥ 4 GiB (exact). That saves
  8 B/entry.
- 2026-10-09 â€” **CSR child lists**, not `first_child`/`next_sibling`. Directory rows hold a
  range, and growth spills into a per-directory overflow vector. Same memory as a sibling chain,
  but contiguous iteration, and an unlink is a vectorizable scan instead of a pointer chase.
  Base directories' row index = their `EntryId`, so no map is needed.
- 2026-10-09 â€” **WTF-8 names with a LEB128 length prefix, sampled offsets** (one `u32` per 16
  entries) instead of per-entry offset+length. Saves 5.75 B/entry. Search is one sequential pass,
  and WTF-8 halves the memory and doubles the single-thread scan speed versus UTF-16 (numbers
  above). Renamed or new names are appended with an override map.
- 2026-10-09 â€” **Largest descendant stored for both size modes** (+4 B/dir) so mode switching
  stays free. Ties break toward the lower id, which is why `compact()` recomputes aggregates.
- 2026-10-09 â€” **Incremental extremes:** sums take exact deltas. Newest/oldest/largest/partial
  improve in O(1). When the changed child *held* the extreme and got worse, only that ancestor
  row is recomputed from its children (O(fanout)), bottom-up, and the walk stops at the first
  unchanged ancestor.
- 2026-10-09 â€” **Placement is a pure function of the record set**, applied identically by the
  build and the live path. Entries shown away from their real parent are kept in `detached`
  (intended parent) and indexed by key in `pending`. Changing a record re-evaluates its
  dependents transitively along intended-parent links (a found bug: breaking a 3-cycle turns a
  member two hops away into a tail).
- 2026-10-09 â€” **All cycle members** go to Orphaned with `CYCLE_BROKEN`, not one arbitrary
  member. That is deterministic regardless of ids or arrival order, which the property tests
  need.
- 2026-10-09 â€” **Both virtual group nodes always exist** (they may be empty) so live and fresh
  indexes agree structurally. The UI should hide empty virtual directories.
- 2026-10-09 â€” **Lite mode** drops the four time columns (âˆ’16 B/entry). Time aggregates are
  computed at build. Afterwards they only move monotonically, since per-file times aren't there
  to recompute from. Everything else is exact.
- 2026-10-09 â€” **Search scans without an auxiliary index.** It already meets the target by ~2Ã—
  at 5M on full completion and ~40Ã— on first batch. Case folding is ASCII fast-path plus
  `char::to_lowercase`; surrogates pass through.
- 2026-10-09 â€” **`safe:` and `app:` without a provider match nothing** (rather than being
  ignored), so a stale UI can't show unsafe results as "safe".
- 2026-10-09 â€” **The regex toggle is a parse mode** (`Query::parse_regex`), not a search option:
  a regex such as `^a\d+` must not first be split into path components.

## Blockers

None for this track. The real-volume comparisons in SPEC Â§2 (WizTree etc.) need the scanner
tracks plus an elevated run.

## Core change requests

1. Optional, for consistency: a stable machine key per category, so the search parser doesn't
   duplicate the serde names (`strata-index/src/search/parse.rs::category_matches`).

```diff
--- a/crates/strata-core/src/tiers.rs
+++ b/crates/strata-core/src/tiers.rs
@@ impl Category {
+    /// Stable lowercase key (the serde name), e.g. `ai_models`.
+    #[must_use]
+    pub const fn key(self) -> &'static str {
+        match self {
+            Self::Unknown => "unknown",
+            Self::System => "system",
+            Self::Apps => "apps",
+            Self::Games => "games",
+            Self::AiModels => "ai_models",
+            Self::DevBuild => "dev_build",
+            Self::Caches => "caches",
+            Self::Temp => "temp",
+            Self::Downloads => "downloads",
+            Self::Documents => "documents",
+            Self::Media => "media",
+            Self::Archives => "archives",
+            Self::Cloud => "cloud",
+            Self::RecycleBin => "recycle_bin",
+            Self::NtfsMetadata => "ntfs_metadata",
+        }
+    }
```

No other core changes were needed. `EntryFlags` already has everything (`HARDLINK_SECONDARY`,
`ORPHAN`, `CYCLE_BROKEN`, `VIRTUAL`, `PARTIAL`, `SUSPICIOUS_TIME`).

## Notes for integrators

- Scanners: emit the root as a self-linked directory (NTFS record 5). Without one, a virtual root
  is synthesized and everything becomes an orphan. Mark incomplete directories with `PARTIAL` or
  `ACCESS_DENIED`, and call `IndexBuilder::set_partial(true)` for a cancelled scan.
- USN tailer (M5): coalesce per tick, then `apply(batch)`; forward `ChangeSet` to the UI;
  `set_usn_position` after each batch; `save` periodically. On load, use `cache::read_header` to
  compare serial and journal id before calling `load`.
- `on_batch` in `search` may run concurrently on worker threads. Forward to a channel. Batches
  are unsorted chunk results, and `SearchOutcome::hits` is the final ranked list.

## Next steps

- M5: wire to the USN tailer, decide the compaction policy (on idle, or before save), and run
  the soak test against real churn.
- Optional mmap loader for the cache. The columns are already laid out for it, but the loader
  copies today.
- Parallelize `NameStore::from_base` and `layout_tree` if the 5M `finish` (â‰ˆ 0.6 s) matters
  against the 12 s scan budget.
- The classifier track writes `category`/`owner_app` through `set_category`/`set_owner_app`, or
  a bulk setter if profiling asks for one.
