# Store track (`strata-store`)

SQLite persistence for snapshots/history (SPEC §18), settings (§19), the
write-ahead undo/audit log (§15.2 step 7, §15.7, §21), ETW rollups (§11), the
duplicate hash cache (§14) and license storage (§20).

## Status

Done; all items in the track brief are implemented and tested.

| Area | State |
|---|---|
| Lifecycle: WAL, `synchronous`, `foreign_keys`, busy timeout | Done |
| Versioned migrations (`user_version`) + snapshot test + migrate-from-every-version test | Done |
| Corruption detection (`quick_check` on open, `SQLITE_CORRUPT`/`NOTADB` at runtime) + reset | Done |
| Snapshots (volume totals, dir aggregates, path dedup, packed blob) | Done |
| Retention (age limit + weekly thinning, injectable clock, path GC) | Done |
| Queries: usage series, diff (deepest contributor), since-last-scan, sparkline | Done |
| Settings (every §19 setting, per-key versions, forward compat, export/import) | Done |
| Undo/audit log (write-ahead, crash recovery, history, restorable items) | Done |
| ETW hourly rollups + last writer + retention + clear | Done |
| Duplicate hash cache (bulk upsert, lookup, invalidation) | Done |
| License storage | Done |
| Threading model (`Store` handle, writer + reader pool) | Done |

## Files

```
crates/strata-store/
  Cargo.toml
  schema/history.sql, schema/state.sql   # schema snapshots (test fixtures + docs)
  src/lib.rs        Store handle, open/health/reset, module docs
  src/db.rs         connections, pragmas, quick_check, migrations, reset
  src/schema.rs     embedded migrations (DDL constants)
  src/error.rs      StoreError, DbKind
  src/clock.rs      Timestamp (UTC), Clock, SystemClock, ManualClock
  src/path.rs       normalize_path, path_hash, parent_hash
  src/codec.rs      packed snapshot blob (varint)
  src/snapshot.rs   snapshot writer and series queries
  src/diff.rs       diff + since-last-scan + heuristic
  src/retention.rs  retention thinning
  src/settings.rs   Settings and persistence
  src/undo.rs       undo/audit log
  src/activity.rs   ETW rollups, last writer
  src/hashes.rs     duplicate hash cache
  src/license.rs    license storage
  tests/            integration tests + ignored benchmarks
```

## Database files and threading

A store is a directory with two databases:

| File | Contents | `synchronous` | Reset API |
|---|---|---|---|
| `history.db` | snapshots, paths, activity rollups, last writer, hash cache (all derived, rebuildable) | `NORMAL` | `reset_history()` |
| `state.db` | settings, undo/audit log, license (user state) | `FULL` | `reset_state()` |

- `Store` is `Clone + Send + Sync` (an `Arc`). Each database has one writer
  connection behind a mutex and a pool of up to 4 idle reader connections.
  With WAL, readers never wait for the writer and each read call runs in one
  read transaction (consistent view).
- Every multi-row write is one transaction (`BEGIN IMMEDIATE`).
- Each `Db` holds an `RwLock`: calls take the read side for their duration;
  `reset` takes the write side so no connection is open while the file is
  renamed (Windows: SQLite opens without `FILE_SHARE_DELETE`). Threads using
  the store during a reset wait, then see the fresh database.
- Calls block; the app should call from a background thread.
- `Store::open` only fails on directory I/O. A corrupt/too-new/unopenable DB is
  reported by `health()` and by typed errors from calls that touch it; the
  other DB keeps working. No call panics.

## Schema (DDL)

Integers that are conceptually `u64` (hashes, file refs, FILETIMEs, serials)
are stored bit-cast to `i64`. All times are UTC Unix seconds.

### history.db (user_version 3)

```sql
-- v1 snapshots
CREATE TABLE volumes (
    id          INTEGER PRIMARY KEY,
    serial      INTEGER NOT NULL,
    guid_path   TEXT    NOT NULL,
    UNIQUE (serial, guid_path)
);
CREATE TABLE paths (
    id          INTEGER PRIMARY KEY,
    hash        INTEGER NOT NULL UNIQUE,      -- path_hash(display path)
    parent_hash INTEGER,                      -- path_hash(parent), NULL for roots
    path        TEXT    NOT NULL              -- display path, stored once
);
CREATE TABLE snapshots (
    id            INTEGER PRIMARY KEY,
    volume_id     INTEGER NOT NULL REFERENCES volumes (id) ON DELETE CASCADE,
    taken_at      INTEGER NOT NULL,
    total_bytes   INTEGER NOT NULL,
    free_bytes    INTEGER NOT NULL,
    allocated_sum INTEGER NOT NULL,
    logical_sum   INTEGER NOT NULL,
    file_count    INTEGER NOT NULL,
    dir_count     INTEGER NOT NULL,
    scanner       TEXT    NOT NULL,           -- 'mft' | 'walker'
    min_dir_bytes INTEGER NOT NULL,
    stored_dirs   INTEGER NOT NULL
);
CREATE INDEX snapshots_by_volume_time ON snapshots (volume_id, taken_at);
CREATE TABLE snapshot_dirs (
    snapshot_id INTEGER PRIMARY KEY REFERENCES snapshots (id) ON DELETE CASCADE,
    codec       INTEGER NOT NULL,             -- 1 = varint blob, see src/codec.rs
    data        BLOB    NOT NULL
);

-- v2 activity rollups
CREATE TABLE process_images (
    id   INTEGER PRIMARY KEY,
    path TEXT    NOT NULL UNIQUE
);
CREATE TABLE activity_hourly (
    hour          INTEGER NOT NULL,           -- start of UTC hour
    image_id      INTEGER NOT NULL REFERENCES process_images (id) ON DELETE CASCADE,
    dir_hash      INTEGER NOT NULL,
    bytes_written INTEGER NOT NULL DEFAULT 0,
    files_created INTEGER NOT NULL DEFAULT 0,
    files_deleted INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (hour, image_id, dir_hash)
) WITHOUT ROWID;
CREATE INDEX activity_by_dir ON activity_hourly (dir_hash, hour);
CREATE TABLE last_writer (
    path_hash  INTEGER PRIMARY KEY,
    image_id   INTEGER NOT NULL REFERENCES process_images (id) ON DELETE CASCADE,
    pid        INTEGER,
    written_at INTEGER NOT NULL
) WITHOUT ROWID;

-- v3 duplicate hash cache
CREATE TABLE hash_cache (
    volume_id INTEGER NOT NULL REFERENCES volumes (id) ON DELETE CASCADE,
    file_ref  INTEGER NOT NULL,
    size      INTEGER NOT NULL,
    mtime     INTEGER NOT NULL,
    partial   INTEGER NOT NULL,               -- xxh3 of first/middle/last 64 KiB
    full      BLOB CHECK (full IS NULL OR length(full) = 32),  -- BLAKE3
    PRIMARY KEY (volume_id, file_ref)
) WITHOUT ROWID;
```

### state.db (user_version 3)

```sql
-- v1 settings
CREATE TABLE settings (
    key        TEXT    PRIMARY KEY,           -- 'section.field'
    version    INTEGER NOT NULL,              -- per-key schema version
    value      TEXT    NOT NULL,              -- JSON
    updated_at INTEGER NOT NULL
) WITHOUT ROWID;

-- v2 undo / audit log
CREATE TABLE actions (
    id          INTEGER PRIMARY KEY,
    kind        TEXT    NOT NULL,             -- cleanup | duplicates | tool
    status      TEXT    NOT NULL,             -- in_progress | completed | partial | failed | cancelled | interrupted
    started_at  INTEGER NOT NULL,
    finished_at INTEGER
);
CREATE INDEX actions_in_progress ON actions (id) WHERE status = 'in_progress';
CREATE TABLE action_items (
    id            INTEGER PRIMARY KEY,
    action_id     INTEGER NOT NULL REFERENCES actions (id) ON DELETE CASCADE,
    seq           INTEGER NOT NULL,
    path          TEXT    NOT NULL,
    volume_serial INTEGER NOT NULL,
    volume_guid   TEXT    NOT NULL,
    file_ref      INTEGER NOT NULL,
    size          INTEGER NOT NULL,
    mtime         INTEGER NOT NULL,           -- FILETIME
    method        TEXT    NOT NULL,           -- recycle | permanent | reboot_delete | tool
    tier          TEXT    NOT NULL,           -- safe | probably | careful | never
    rule_id       TEXT,
    result        TEXT    NOT NULL DEFAULT 'pending',  -- pending | done | failed | skipped
    error         TEXT,
    completed_at  INTEGER,
    original_path TEXT,                       -- Recycle Bin restore info
    restore_blob  BLOB,                       -- opaque, defined by strata-clean
    restored_at   INTEGER,
    UNIQUE (action_id, seq)
);
CREATE INDEX action_items_restorable ON action_items (completed_at)
    WHERE method = 'recycle' AND result = 'done' AND restored_at IS NULL;

-- v3 license
CREATE TABLE license (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    payload      BLOB    NOT NULL,
    signature    BLOB    NOT NULL,
    activated_at INTEGER NOT NULL
);
```

### Snapshot blob (codec 1)

Unsigned LEB128 throughout: `count`, then per row (sorted by `paths.id`):
`id delta`, `allocated tag` (`allocated/4096*2` when cluster-aligned, else
`allocated*2+1`), `zigzag(logical - allocated)`, `files`. Decoding is fully
bounds-checked (proptest: never panics on garbage).

## Public API (all on `Store` unless noted)

Lifecycle: `Store::open(dir)`, `Store::open_with_clock(dir, Arc<dyn Clock>)`,
`health() -> StoreHealth`, `reset_history()`, `reset_state() -> ResetReport`,
`dir()`.

Snapshots: `begin_snapshot(&VolumeKey, VolumeTotals) -> SnapshotWriter`,
`begin_snapshot_with(.., SnapshotOptions)`; `SnapshotWriter::add_dirs(iter of
DirAggregate) -> usize`, `SnapshotWriter::commit() -> SnapshotId`;
`snapshot(id)`, `snapshots(&volume)`, `latest_snapshot(&volume)`, `volumes()`,
`snapshot_dir(id, path)`, `clear_history()`.

Queries: `usage_series(&volume, from, to) -> Vec<UsagePoint>`,
`dir_series(&volume, path_hash, last_n) -> Vec<DirPoint>`,
`diff(from, to, &DiffOptions) -> SnapshotDiff`,
`since_last_scan(&volume, &DiffOptions) -> Option<SinceLastScan>`.

Retention: `apply_retention(&RetentionPolicy) -> RetentionReport`.

Settings: `load_settings()`, `save_settings(&Settings)`, `export_settings() ->
String`, `import_settings(&str) -> Settings`; `Settings::validate()`;
`HistorySettings::{retention_policy, snapshot_options}`.

Undo log: `begin_action(ActionKind, &[PlannedItem]) -> ActionId`,
`complete_item(id, seq, &ItemOutcome)`, `finish_action(id, ActionStatus)`,
`recover_incomplete() -> Vec<ActionRecord>`, `action(id)`,
`action_history(limit, before)`, `restorable_items(limit)`,
`mark_restored(ItemId)`.

Activity: `record_activity(&[ActivitySample])`, `top_writers(since, limit)`,
`dir_writers(dir_hash, since, limit)`, `set_last_writers(&[LastWrite])`,
`last_writer(path_hash)`, `prune_activity(days)`, `clear_activity()`.

Hash cache: `upsert_hashes(&volume, &[CachedHash])`, `lookup_hashes(&volume,
&[HashKey]) -> Vec<Option<CachedHash>>`, `invalidate_hashes(&volume,
&[FileRef])`, `clear_hash_cache()`.

License: `save_license(payload, signature)`, `load_license()`,
`clear_license()`.

Free functions: `path_hash(&str) -> u64`, `normalize_path`, `parent_hash`.
Time: `Timestamp` (UTC seconds; `from_utc`, `iso_week_index`, `hour_start`,
`Display` as RFC 3339), `Clock`, `SystemClock`, `ManualClock`.

## Tests

`cargo test -p strata-store`: **109 passing** (41 unit, 51 integration, 17
doctests), plus 2 ignored benchmarks. Clippy `-D warnings` clean on
`--all-targets`.

| Suite | Count | Covers |
|---|---|---|
| unit (`src/`) | 41 | migrations from every version, schema snapshots, too-new, corruption latch, pragmas, codec (+2 proptests), civil-date math, ISO weeks, retention rules (DST, year-end week, ties, future, per-volume), path normalization (+proptest), settings fallback rules |
| `tests/snapshots.rs` | 17 | round trip, min-size filter, dedup, usage series, sparkline gaps, path sharing, diff heuristic, logical mode, bad input, since-last-scan, retention + path GC, idempotence, clear, 50k dirs |
| `tests/corruption.rs` | 7 | garbage file, damaged pages, corrupt state only, unique reset names, too-new reset, damaged blob, unwritable dir |
| `tests/undo.rs` | 6 | full protocol, crash simulation + recovery, misuse, restorable/mark restored, paging, extreme values |
| `tests/settings_store.rs` | 10 | round trip, validation, unknown keys preserved, missing/bad keys, newer-version rows, export/import, import validation, partial import |
| `tests/caches.rs` | 8 | hourly sums, prune/clear, last-writer ordering, hash hits/misses, full-hash retention, invalidation, license |
| `tests/concurrency.rs` | 3 | readers during writes, concurrent writers, reset while reading |

Regenerate schema snapshots after adding a migration:
`$env:STRATA_UPDATE_SNAPSHOTS=1; cargo test -p strata-store schema`.

## Benchmarks

`cargo test -p strata-store --release --test bench -- --ignored --nocapture`
on an 8-core desktop CPU with NVMe, 50,000 synthetic directories per snapshot (sizes 1 MiB
to 10 GiB, cluster-aligned, 2-7 levels deep), two runs:

| Measure | Result |
|---|---|
| Commit, first snapshot (50k new paths) | 147-215 ms; file +5.7 MB (mostly the `paths` table) |
| Commit, later snapshot (paths exist) | median 53-65 ms, max 81-103 ms |
| **Bytes per later snapshot (packed blob)** | **598 KB = 12.0 B/dir** |
| Row table `WITHOUT ROWID (snapshot_id, path_id)` | 1.46 MB = 29.3 B/dir |
| Row table `WITHOUT ROWID (snapshot_id, path_hash)` | 1.74 MB = 34.9 B/dir |
| Rowid table + index | 2.19 MB = 43.8 B/dir |
| Diff of two 50k snapshots (min_change 1 MiB) | 14-35 ms |
| `dir_series` over 30 snapshots | 36-48 ms |
| Retention deleting 29 snapshots + path GC | 27-31 ms |

90 days of daily 50k-dir snapshots thinned per policy (~40 kept) cost about
24 MB plus the shared paths table.

## Decisions

- 2026-10-09 — **Two database files (`history.db`, `state.db`).** Resetting a
  corrupt history must not lose settings, the undo log (Recycle Bin restores
  depend on it) or the license; it also lets the undo log run
  `synchronous=FULL` without slowing snapshot commits, and keeps a long
  snapshot commit from delaying a write-ahead delete record.
- 2026-10-09 — **Snapshot rows packed into one varint blob per snapshot**
  rather than one SQL row per directory. 12.0 vs 29.3 B/dir (2.4x smaller)
  than the best row layout at similar commit time; diffs decode two blobs in
  milliseconds. Costs: sparkline queries decode N blobs (~40 ms for 30), and
  path GC decodes all blobs (only during retention). No compression
  dependency added.
- 2026-10-09 — **Dense `paths.id` in blobs, `path_hash` as the external key.**
  Ids assigned in path order make sorted deltas ~1 byte; the 64-bit hash
  would cost ~6 more bytes per row.
- 2026-10-09 — **Min-size filter uses `max(allocated, logical)`** (default 16
  MiB) so sparse/compressed folders stay visible in both size modes.
- 2026-10-09 — **Deepest-contributor heuristic:** a parent is dropped from
  grown/shrunk when a direct child that is itself listed explains at least
  `dominance` (0.9) of its change; new/deleted lists report topmost folders
  only. See `src/diff.rs` module docs.
- 2026-10-09 — **Retention: the newest snapshot per volume is never deleted**
  (keeps the since-last-scan baseline after long breaks); "last of the ISO
  week" is judged over all of the volume's snapshots, in UTC.
- 2026-10-09 — **All stored times are UTC Unix seconds**; `Timestamp` has its
  own civil-date math (no chrono dependency). DST is a display concern.
- 2026-10-09 — **Settings stored per leaf key** (`section.field`, JSON value,
  per-key version). Unknown keys untouched; bad values fall back per key;
  cross-field violations revert the section; newer-versioned rows are not
  overwritten unless the user changed that value.
- 2026-10-09 — **Undo log write-ahead = committed `IMMEDIATE` transaction on a
  `synchronous=FULL` WAL database.** `finish_action` turns leftover `pending`
  items into `skipped`; recovery is the app's job using
  `recover_incomplete()` + `complete_item` + `finish_action(Interrupted)`.
- 2026-10-09 — **Hash cache keyed (volume, file_ref)** with size/mtime checked
  on lookup (one row per file, never stale hits). A partial-only upsert for
  unchanged content keeps an existing full hash.
- 2026-10-09 — **Store opens never fail on database problems**, only on
  directory I/O; a broken DB is reported via `health()` and typed errors.
  `quick_check` runs on every open (O(pages), tens of ms per 100 MB).
- 2026-10-09 — **Paths normalized NTFS-style** (single-char uppercase only,
  `ß` stays `ß`); WSL case-sensitive siblings share a key (accepted).

## Blockers

None.

## Notes for the lead / other tracks

- `Cargo.lock` gained `rusqlite 0.40.2` (bundled SQLite), `xxhash-rust
  0.8.19`, and `tempfile` (dev). No root `Cargo.toml` change; the crate is
  picked up by `members = ["crates/*"]`.
- **Clean track:** `RestoreInfo.blob` is opaque bytes for whatever the Shell
  restore needs; `DeleteMethod`/`ActionKind` strings are fixed in the DB.
- **ETW track:** key directories with `strata_store::path_hash` so activity
  joins with snapshots and the index.
- **Duplicates:** call `invalidate_hashes` from the USN apply path for
  modified/deleted file refs.
- **App:** on startup, check `health()` and offer "Reset history" for
  `history != Ok`; call `recover_incomplete()` before enabling cleanup; run
  `apply_retention(settings.history.retention_policy())` and
  `prune_activity(settings.activity.retention_days)` daily.

## Core change requests

None required. Optional: a `Safety::as_str()` in `strata-core` would let the
store drop its private mapping (it currently mirrors the serde names).

## Next steps

- Wire into `src-tauri` (background thread, Tauri commands, startup health
  check and recovery).
- If sparklines over many snapshots need to be faster, cache decoded blobs
  per snapshot or add a per-snapshot sparse offset table to the codec (codec
  2; the `codec` column already allows it).
- `docs/BENCHMARKS.md`: copy the numbers above once that file exists.
