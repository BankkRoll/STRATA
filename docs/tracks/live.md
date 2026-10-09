# Track: live updates (`strata-live`, M5)

Owner paths: `crates/strata-live/`, this file.

Status: **M5 engine side done.** The tailer, coalescing, cache catch-up, edge cases and the
non-NTFS rescan path are complete and tested without an elevated volume handle. What remains
for M5 is wiring: the helper must implement the two source traits over its FSCTLs (change
request 1 below adds the blocking wait to `ReadUsn`), and the app must run the tailer and
animate change sets. `cargo clippy -p strata-live --all-targets -- -D warnings` is clean, the
crate is `#![forbid(unsafe_code)]`, and it makes no OS calls besides the cache file.

## Done (by SPEC section)

| SPEC | Delivered | Where |
|---|---|---|
| §2 live ≤ 1 s | 251–265 ms record → `ChangeSet` (default 250 ms tick, real clock) | `benches/live.rs` |
| §2 idle CPU ≈ 0 | With nothing pending the read has no timeout: **0 wakeups in 3 s idle** | `tailer.rs`, bench, `edge.rs` |
| §9.1 cache + catch-up | `CacheFile` (atomic tmp + `sync_all` + rename, serialize under the lock, write outside it), header volume-serial check before decoding, `index_position`, replay to head before `Live` | `cache.rs` |
| §10.1 reading | `JournalSource::{query, read(journal_id, from, wait)}` returning the raw FSCTL buffer, parsed with `strata_ntfs::parse_usn_buffer` (V2, V3, V4) | `source.rs`, `tailer.rs` |
| §10.2 reasons | Create, delete, rename old/new (paired, cross-directory, split across reads), data overwrite/extend/truncate, named-stream data, hard-link, reparse, stream (ADS), basic-info, compression, encryption, EA, indexable all refresh; security, object id, transacted, integrity, storage-class only and bare close are ignored | `reason.rs`, `coalesce.rs` |
| §10.2 coalescing | One fetch per file per tick (10,000 writes → 1 fetch, tested); close records always refresh (see Decisions) | `coalesce.rs` |
| §10.2 minimal events | Deletes are removes without a fetch; one merged `ChangeSet` per tick (`ChangeMerger` keeps the index's "removed + created = slot reused" convention); parents whose name set changed are refreshed | `tailer.rs`, `merge.rs` |
| §10.3 wrap / id change | `Halt::NeedsRescan(JournalWrapped / JournalIdChanged / UsnAhead)`, at start, on resume and mid-tail; `RescanReason` has `Display` text for the unobtrusive notice | `status.rs` |
| §10.3 disabled | `Halt::JournalDisabled` (query `None`, `JournalInactive`, or a reset with no new journal) | `status.rs`, `tailer.rs` |
| §10.3 dismount / disconnect | `Halt::Stale(VolumeGone / Disconnected / Io)`; the applied position never covers unapplied records; cache saved on stale stop | `tailer.rs` |
| §10.3 sleep/resume | `Tailer::resume` re-validates and replays from the applied position | `tailer.rs` |
| §10.3 bursts | Bounded per tick (`max_refresh_per_tick` 16,384, `tick_budget` 150 ms, `apply_batch` 2,048 per lock hold), reading pauses above `max_pending` 262,144, `LiveStatus::CatchingUp { pending, unread_bytes }` at most once per tick | `tailer.rs` |
| §10.3 non-NTFS | `SubtreeWatcher` port, `RescanPlanner` (minimal shallow/deep targets, overflow → deep root, manual rescan), `reconcile_subtree`, `resolve_relative` | `watch.rs` |
| §10.3 ReFS | 128-bit ids that don't fit a `FileRef` → `NeedsRescan(UnsupportedFileIds)` (app falls back to the walker + watcher) | `tailer.rs` |
| §21 | Journal disabled / wrapped / id changed / dismounted mid-tail; 500k burst; sleep/resume | tests below |
| §22 live soak | Property soak: live index == fresh build after quiescence | `tests/soak.rs` |

## Public API

```rust
// Ports (helper/app implement them)
trait JournalSource { fn query(&mut self) -> Result<Option<JournalInfo>, SourceError>;
                      fn read(&mut self, journal_id: u64, from_usn: i64, wait: Option<Duration>)
                          -> Result<Vec<u8>, SourceError>; }   // FSCTL_READ_USN_JOURNAL output
trait RecordSource  { fn fetch(&mut self, refs: &[FileRef]) -> Result<Fetched, SourceError>; }
trait Clock         { fn now(&self) -> Instant; }              // SystemClock
trait IndexAccess   { fn with_index(&mut self, f: &mut dyn FnMut(&mut Index)) -> bool; }
                    // impl for Index, &Mutex<Index>, &RwLock<Index>, Arc<Mutex<_>>, Arc<RwLock<_>>
trait SubtreeWatcher { fn next(&mut self, wait: Option<Duration>) -> Result<WatchBatch, SourceError>; }

// Tailer
Tailer::start(TailerConfig, &mut dyn JournalSource, JournalPosition) -> Result<Tailer, Halt>
  .with_cache(CacheFile)
  .run(journal, records, index, clock, &AtomicBool, &mut dyn FnMut(LiveEvent)) -> Halt
  .step(...) -> Result<(), Halt>          // one read + flush/save when due
  .resume(journal) -> Result<(), Halt>    // after Stale or wake from sleep
  .save_now(index, clock, events)         // before suspend
  .position() / read_position() / status() / pending() / stats()
LiveEvent::{Tick(TickReport), Status(LiveStatus), Saved(JournalPosition), SaveFailed(String)}
TickReport { changes: ChangeSet, status, fetched, upserts, removes, unavailable, skipped, elapsed, applied }
Halt::{Stopped, JournalDisabled, Stale(StaleReason), NeedsRescan(RescanReason)}
check_position(JournalPosition, Option<JournalInfo>) -> Result<JournalInfo, Halt>

// Cache
CacheFile::new(path).{save(&mut Index, JournalPosition), write(&[u8]), load(serial) -> Result<Index, LoadError>}
LoadError::{Missing, Unusable(CacheError), VolumeMismatch{..}}.rescan_reason()
CachePolicy { interval: 300 s, save_on_stop: true };  index_position(&Index)

// Non-NTFS
RescanPlanner::{push(WatchBatch), rescan(path), take() -> Vec<RescanTarget{path, deep}>}
reconcile_subtree(&mut Index, scope, fresh: Vec<ScanRecord>, deep) -> Result<ChangeSet, IndexError>
resolve_relative(&Index, base, &[WideName]) -> Option<EntryId>

// Misc
merge_change_sets(..) / ChangeMerger;  reason::{needs_refresh, is_delete, is_namespace, IRRELEVANT, NAMESPACE}
```

## Tests

46 total, all passing in debug and release: 14 unit, 5 doctests, and integration suites.

- **`tests/soak.rs` (the acceptance test).** `tests/support` holds an in-memory volume model
  that journals every change the way NTFS does: reason bits accumulate per file while a handle
  is open, a record is written only when a new bit appears, a close record carries all bits,
  writes inside an open handle are invisible until the close, renames write old-name, new-name
  and close records, deletes are recursive, and directory index allocation grows and shrinks
  with the name count without a journal record of its own. Records are encoded as V2, V3 or
  mixed, plus V4 range records, and served through fake `JournalSource`/`RecordSource` with
  read buffers as small as 64 bytes, so rename pairs split across reads. Ops: create, delete,
  rename and cross-directory move, resize with open/close handles, close, hardlink add and
  remove, ADS add/resize/remove, reparse toggles, basic info, compression, encryption,
  security-only, duplicate late closes, deletes of never-seen refs, bursts, ticks, settle
  points (equality checked at each) and app restarts from a cache image. Tailing optionally
  starts before the scan (replay of already-scanned changes), with default or tiny batch
  limits (forcing backlogs). Equality is a canonical, id-independent comparison against a
  fresh `IndexBuilder` build, plus `check_invariants`. 96 cases by default plus a
  deterministic 3 × 3,000-op soak; **`STRATA_PROPTEST_CASES=20000` passed in release (131 s)**.
  Mutation check: disabling parent refresh, or ignoring close records, makes it fail at once.
- **`tests/edge.rs` (17).** Wrap at start and mid-tail, id change at start and mid-tail,
  disabled at start and mid-tail, position past the end, disconnect during fetch then volume
  gone, then resume and catch up to equality; resume after a wrap; cancel → `Stopped`; 128-bit
  ids; malformed buffer; split rename held until its partner (one fetch, both parents'
  aggregates in one tick); 10,000 writes → one fetch; security-only → no fetch and position
  advances; ghost deletes; duplicate/late closes; root never removed; idle read blocks
  (`wait == None`), pending work waits exactly to the tick deadline.
- **`tests/cache.rs` (4).** Atomic write (no leftover tmp), volume mismatch, truncated cache
  rejected; periodic save at the interval with journal id and applied USN in the header, then
  idle blocking; clean stop saves, the volume changes while closed, launch loads, replays
  (`CatchingUp` → `Live`) and equals a fresh scan; launch after a wrap needs a rescan.
- **`tests/burst.rs`.** 500,000 creates in 64 directories on the real clock: every tick within
  the per-tick cap and budget, progress reported, `Live` announced, equality. 1.9 s release,
  about 18 s debug.
- **`tests/watch.rs` (3).** Deep and shallow `reconcile_subtree` equal a fresh build (shallow
  removes vanished folders with their subtrees); `resolve_relative`.

## Benchmarks

`cargo bench -p strata-live` (report harness), release, 16-thread desktop CPU, Windows 11,
other workloads present (±20%).

| Metric | Result | Target |
|---|---|---|
| Throughput: 100k changes (60% resize, 20% create, 10% move, 10% delete = 199k records) into a 1M-entry index, parse + coalesce + fetch (in-memory) + apply | **629 ms = 316k records/s**, 238k index updates/s, slowest tick 19 ms | — |
| Latency: record written → `LiveEvent::Tick`, default tick, real clock, blocking source | min 251 ms, median 255 ms, max 265 ms | ≤ 1 s |
| Idle: journal reads while nothing changes for 3 s | **0** (one blocked read) | ≈ 0% CPU |
| Burst: 500k creates (`tests/burst.rs`, release) | 1.9 s total, 32 ticks, slowest 66 ms, longest index lock hold 24 ms, 8 progress events | UI responsive |

The fetch in these numbers is in-memory. Over IPC each 1,024-ref `ReadRecords` adds one round
trip plus `read_record` per ref in the helper (about 1 µs parse each, plus I/O for records not
in the cache).

## Decisions

- 2026-10-09: **Refresh by re-reading, not by interpreting records.** USN records carry no
  sizes, so every relevant record schedules a fetch of the file's *current* state, and only
  `FILE_DELETE` short-circuits to a remove (a deleted reference's sequence is never reused).
  This makes replay idempotent, so the cache may lag safely and catch-up can start before a
  scan.
- 2026-10-09: **Close records always refresh.** NTFS writes a record only when a reason bit is
  first set for an open file, so later writes in the same handle are invisible until the close
  record. Skipping "duplicate" closes loses those changes (the soak fails at once if closes
  are ignored).
- 2026-10-09: **Parent directories are refreshed** when their name set changes: from the
  record's parent (create, delete, rename halves, hardlink) and from the index diff of old vs.
  new links after each upsert or remove. NTFS journals no record for a directory whose `$I30`
  allocation changed. A directory fetched in the same batch is not fetched again.
- 2026-10-09: **Tick = first unapplied record + 250 ms**, not a sliding debounce, so sustained
  churn still reflects within one tick. With nothing pending the read has no timeout; with a
  backlog it doesn't block.
- 2026-10-09: **Rename halves are held one tick** when the new-name record is not read yet, so
  a move split across reads is one update. If the partner never comes, the file is refreshed
  anyway.
- 2026-10-09: **The applied position advances only when the dirty set is empty.** It never
  covers a read-but-unapplied record, which is what makes stale stops, crashes and
  `resume` safe. Under sustained overload it lags (longer replay), never skips.
- 2026-10-09: **Bounded ticks.** At most 16,384 refs and 150 ms per flush (checked between
  1,024-ref batches); updates are applied in 2,048-update lock holds. Reading pauses above
  262,144 pending refs, bounding memory (about 40 B per pending ref).
- 2026-10-09: **No compaction in the tailer.** `Index::compact` remaps ids, which the UI must
  coordinate, so the app decides (for example when `tombstones()` passes 25% and before a
  save).
- 2026-10-09: **References neither returned nor reported missing are left as they are**
  (`TickReport::unavailable`), not removed: an access-denied `OpenFileById` must not delete
  entries.
- 2026-10-09: **Updates that would remove or replace the root are dropped**
  (`TickReport::skipped`) so a bad record can't fail a whole batch.
- 2026-10-09: **The status is published with progress throttled to once per tick.** A 500k
  burst emitted 1,135 progress events before, 8 after. State changes (`Live` ↔ `CatchingUp`)
  go out at once.

## Blockers

- **Not elevated:** no real journal on this machine. Everything runs against the
  model-backed fakes. The real soak (SPEC §22: `npm install`, archive extraction, tree renames
  while tailing, then compare with a fresh MFT scan) needs the helper's source implementations
  and an elevated run.

## Change requests

1. **`strata-ipc`: blocking reads.** `ReadUsn` has no wait, so the helper can't block, and
   polling would break the idle-CPU target. The helper maps `wait_ms` onto
   `READ_USN_JOURNAL_DATA { BytesToWaitFor: 1, Timeout }`. `Timeout` is in seconds, so
   sub-second waits use overlapped I/O plus `CancelIoEx` at the deadline. `None` waits until
   data arrives or the request is cancelled (`Request::Cancel`, which must then answer
   `ErrorCode::Cancelled`).

   ```diff
   --- a/crates/strata-ipc/src/protocol.rs
   +++ b/crates/strata-ipc/src/protocol.rs
   @@ pub enum Request {
        ReadUsn {
            /// Volume GUID path.
            volume: String,
            /// Journal id the client expects; a changed id means a full rescan.
            journal_id: u64,
            /// First USN to read.
            from: i64,
            /// Maximum bytes of records to return.
            max_bytes: u32,
   +        /// Block until at least one record is available or this many
   +        /// milliseconds pass; `None` blocks until data or `Cancel`.
   +        #[serde(default)]
   +        wait_ms: Option<u32>,
        },
   ```

2. **`strata-ipc`: journal error codes**, so the tailer can tell a wrap from an id change
   from a disabled journal (each needs a different user notice):

   ```diff
   --- a/crates/strata-ipc/src/protocol.rs
   +++ b/crates/strata-ipc/src/protocol.rs
   @@ pub enum ErrorCode {
        /// I/O error.
        Io,
        /// Helper bug or unexpected state.
        Internal,
   +    /// `ERROR_JOURNAL_NOT_ACTIVE`.
   +    JournalNotActive,
   +    /// `ERROR_JOURNAL_DELETE_IN_PROGRESS`, or the journal id differs from
   +    /// the request's `journal_id`.
   +    JournalReset,
   +    /// `ERROR_JOURNAL_ENTRY_DELETED`: `from` was purged (journal wrapped).
   +    JournalEntryDeleted,
    }
   ```

   Mapping in the app adapter: `JournalNotActive` → `SourceError::JournalInactive`,
   `JournalReset` → `JournalReset`, `JournalEntryDeleted` → `UsnPurged`, `UnknownVolume` →
   `VolumeGone`, `Cancelled` → `Cancelled`, `IpcError::Disconnected` → `Disconnected`, else
   `Io`.

3. Optional, `strata-index`: `Index::apply` loses the change set of the already-applied
   prefix when it returns an error. The tailer pre-filters every error it can predict (root,
   reserved ref); only `TooManyEntries` remains, and it maps to a rescan. Returning
   `(ChangeSet, Result<(), IndexError>)` would remove the caveat.

## Wiring (helper and app)

**Helper (`strata-helper`, elevated):**

1. `QueryUsnJournal` → `FSCTL_QUERY_USN_JOURNAL` → `UsnJournalInfo` (`None` on
   `ERROR_JOURNAL_NOT_ACTIVE`).
2. `ReadUsn { journal_id, from, max_bytes: 65536, wait_ms }` → `FSCTL_READ_USN_JOURNAL` with
   `ReasonMask = 0xFFFFFFFF`, `ReturnOnlyOnClose = 0`, `UsnJournalID = journal_id`,
   `MinMajorVersion = 2`, `MaxMajorVersion = 4`. Reply `UsnRecords { next_usn, raw }`
   (records after the 8-byte prefix). Errors as in change request 2.
3. `ReadRecords { file_refs }` → `NtfsVolume::read_record(r.record())` per ref (keep the
   volume open between calls). Exact id match goes to `records`; `None`, or a sequence
   mismatch, goes to `missing`. Never fail the whole batch for one bad record; leave refs it
   cannot read out of both lists.

**App (`src-tauri`):**

1. Adapters: `HelperJournal` implements `JournalSource` over `QueryUsnJournal`/`ReadUsn`
   (prepend `next_usn.to_le_bytes()` to `raw`; map errors as above). `HelperRecords`
   implements `RecordSource` over `ReadRecords`. A `ReadUsn` reply or `ReadRecords` reply is
   one call each; the rate limiter already budgets one `ReadRecords` per tick.
2. Before a full scan: `QueryUsnJournal`; after it, `index.set_usn_position(journal_id,
   next_usn_before_scan)` and `index.set_now(..)`.
3. On launch: `CacheFile::new(<data dir>\<volume serial>.idx).load(serial)`; on `Err(e)`, scan
   and show `e.rescan_reason()` if it isn't `Missing`. Then `Tailer::start(cfg, journal,
   index_position(&index))`; on `Halt::NeedsRescan(r)` rescan and show `r.to_string()`, on
   `JournalDisabled` show "Live updates unavailable — journal disabled" with the enable action.
4. Run `tailer.run(..)` on a dedicated thread with `Arc<RwLock<Index>>` as `IndexAccess`.
   Forward `LiveEvent::Tick(r)` → `r.changes` to the UI (it animates `updated` ids and
   re-reads `aggregates`; ids in both `removed` and `created` were reused: drop, then add).
   Show `Status(CatchingUp { pending, .. })` as "Catching up… N changes".
5. Shutdown: set the stop flag and cancel the in-flight `ReadUsn`. `run` returns `Stopped`
   after saving. On `PBT_APMSUSPEND` call `save_now`. On resume (or after `Stale`, once the
   helper is back), call `tailer.resume(journal)` and run again.
6. Compaction: when `index.tombstones()` passes 25% of `slot_count()`, pause the tailer
   between steps, `compact()`, send the `IdRemap` to the UI, save.
7. Non-NTFS volumes: implement `SubtreeWatcher` with `ReadDirectoryChangesW` on the viewed
   folder only, feed batches to `RescanPlanner`, walk each `RescanTarget` with `strata-walk`
   (deep or one level), resolve it with `resolve_relative` and apply with
   `reconcile_subtree`. "Rescan" in the UI is `planner.rescan(path)`.
