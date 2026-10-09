# Track: duplicates (`strata-dupes`, M10)

Owner scope: `crates/strata-dupes/`, this file. Status: **M10 engine done**: pipeline, never
hydrate, hardlink exclusion, resumable cache, keep suggestions, guarded selection, verification
before delete and the hardlink action are implemented and tested on the dev machine (Windows 11,
unelevated). App/UI wiring and the store adapter come next (steps below).

## Done vs. SPEC §14 / M10 acceptance

| SPEC | Delivered | Where |
|---|---|---|
| 1. Size groups, min size (default 1 MiB), exclude 0-byte | Yes; plus hardlink secondaries, repeated (volume, file ref), cloud (any state ≠ none), offline, non-content reparse points, NTFS metadata/virtual, synthetic walker ids, `$Recycle.Bin` | `gate.rs`, `scan.rs` |
| 2. Partial hash first+middle+last 64 KiB, xxh3 | Yes (size is hashed in; files ≤ 192 KiB are read whole; middle window 4 KiB aligned) | `hash.rs` |
| 3. Full BLAKE3, streamed, parallel, I/O-throttled | Yes: dedicated rayon pool of `concurrency` threads, one shared token bucket (`max_bytes_per_sec`), 1 MiB sequential reads (`read_buffer`), `FILE_FLAG_SEQUENTIAL_SCAN`, largest files first | `scan.rs`, `throttle.rs` |
| 4. Optional byte compare before delete | Yes: `verify_selection` with `VerifyMode::ByteCompare` or `FullHash` | `verify.rs` |
| Exclude hardlinks | Yes: `HARDLINK_SECONDARY` and same `(volume, FileRef)` | `gate.rs`, `scan.rs` |
| Never hydrate (M10 accept) | Yes, three layers (below) | `win.rs` |
| Background, cancellable, resumable, progress + ETA | Yes: `CancelToken` (strata-clean's), `Progress` events per phase with rate and ETA, hashes written to the cache every `cache_batch` files | `scan.rs`, `report.rs` |
| Cache keyed (file ref, size, mtime), invalidated by USN | Trait `HashCache` mirroring the store; the pipeline invalidates rows of files it sees change; the app calls `invalidate` from live updates | `cache.rs` |
| Groups sorted by wasted bytes, keep suggestion | Yes, with a reason; pure and table-tested | `report.rs`, `keep.rs` |
| Bulk select with guardrails (M10 accept) | Enforced in the type: a keeper that can never be marked; no `Deserialize`; validated `SelectionRequest`; re-checked when producing queue items; proptest | `select.rs` |
| Replace with hardlinks (same volume, confirmed, warnings) | Yes, through `Consent<ReplaceWithHardlinks>` and `strata-clean`'s flow | `hardlink.rs` |

### Why a scan cannot trigger a download

1. Candidates the index reports as cloud (any `CloudState` other than `None`), `OFFLINE`, or any
   reparse kind other than none/WOF/dedup are excluded **before anything is opened**.
2. Every open (`win::open`) uses `FILE_FLAG_OPEN_NO_RECALL | FILE_FLAG_OPEN_REPARSE_POINT`. The
   Cloud Files filter (and legacy HSM) recall on open only for `RECALL_ON_OPEN` files, and
   no-recall suppresses that; open-reparse-point means a link swapped in since the scan is not
   followed.
3. The first open of each file is **attributes only** (`FILE_READ_ATTRIBUTES`, no data access).
   The handle's attributes and reparse tag go through `check_handle_attributes`: any of
   `RECALL_ON_DATA_ACCESS`, `RECALL_ON_OPEN`, `OFFLINE`, `PINNED`, `UNPINNED`, or a cloud reparse
   tag (all 16 provider sub-tags) drops the file. Data handles repeat this check before the first
   read. Hydration on read only happens for ranges that are not local, i.e. on files carrying
   those bits, which never reach a read.

Tests: `gate::tests::handle_gate_refuses_every_placeholder_signal` (injected attributes, every
cloud sub-tag) and `pipeline::placeholders_are_never_read` (a real file gets `OFFLINE` after the
scan and is skipped as `Placeholder`; an index-flagged cloud candidate whose path does not even
exist is excluded without an open, proven by the absence of a `NotFound`).

## Public API

```rust
// Input: one Candidate per file (see "Feeding it from strata-index").
pub struct Candidate { volume: VolumeKey, file_ref: FileRef, path: PathBuf, size: u64, mtime: FileTime, flags: EntryFlags }
pub struct VolumeKey { serial: u64, guid_path: Arc<str> }   // mirrors strata_store::VolumeKey

// Run (blocking; call on a background thread). progress may run on worker threads.
let outcome: ScanOutcome = find_duplicates(candidates, &ScanConfig { min_size, concurrency,
    max_bytes_per_sec, read_buffer, allow_wof_and_dedup, progress_interval, cache_batch, keep },
    &cache /* &dyn HashCache */, &cancel /* strata_clean::CancelToken */, &|p: &Progress| ..);
// ScanOutcome::Completed(DuplicateReport { groups, stats, skipped }) | Cancelled(ScanStats)
// DuplicateGroup { id, size, hash, files: Vec<DupFile>, keep: KeepSuggestion }, wasted_bytes()
// Progress { phase, files_done, files_total, bytes_done, bytes_total, bytes_per_sec, eta_secs, cache_hits }
let (size_groups, excluded) = group_by_size(&candidates, min_size); // dry run, no I/O

// Cache contract (strata-store implements it through an adapter)
pub trait HashCache: Send + Sync {
    fn lookup(&self, &VolumeKey, &[HashKey]) -> Result<Vec<Option<CachedHash>>, CacheError>;
    fn upsert(&self, &VolumeKey, &[CachedHash]) -> Result<(), CacheError>;
    fn invalidate(&self, &VolumeKey, &[FileRef]) -> Result<u64, CacheError>;
}
MemoryHashCache::new() // same semantics, in process

// Keep suggestions
suggest_keep(&[KeepInput { path, mtime }], &KeepContext { downloads, temp_dirs, rules }) -> KeepSuggestion { index, reason }
report.apply_keep_context(&ctx); // after the user edits rules; KeepReason::message() for the UI

// Selection (guardrail) -> strata-clean
let mut sel = Selection::new(&report);            // or Selection::all_but_suggested(&report)
sel.mark(group, i)?; sel.unmark(group, i)?; sel.set_keeper(group, i)?;
sel.apply(&SelectionRequest { group, keep, marked })?;   // what the UI sends
let failures = verify_selection(&report, &sel, &VerifyConfig { mode: VerifyMode::ByteCompare, .. }, &cancel)?;
let queue: Vec<QueueItem> = sel.to_queue_items(&report, |f| classifier_tier(f))?;
// QueueItem.id = queue_item_id(group, index); parse_queue_item_id(id) -> (group, index)

// Hardlinks
let action = hardlink::plan(&report, &sel, group)?;          // HardlinkRefusal: different volume, link limit, nothing marked
let prompt = Prompt::new(action);                             // prompt.text() lists files + HardlinkWarning::ALL
let outcomes = hardlink::replace_with_hardlinks(&guard, prompt.confirm(), |f| tier(f),
    &mut audit_log, &LinkConfig::default(), &cancel)?;       // Vec<LinkOutcome { index, path, result: Result<RestoreTicket, LinkError> }>
```

## Pipeline details

1. Gate + de-duplicate by `(volume, file_ref)`.
2. **Measuring.** `HAS_ADS` candidates are measured first, because the index's `own_logical`
   includes alternate streams; then every member of a size group of two or more is measured. A
   measurement = attributes-only open, placeholder gate, file index == `FileRef`, plus the
   unnamed-stream size and full-precision last-write time. From here these handle values (not the
   index's second-precision mtime) are the truth and the cache key.
3. **Cache lookup** by `(volume, file_ref, size, mtime)`; a hit whose key differs is ignored even
   if a backend returns it.
4. **Partial hash**, regroup by `(size, partial)`.
5. **Full hash**, regroup by `(size, BLAKE3)`.

Every hash reopens the file (no recall), re-verifies id, attributes, size and mtime against the
measurement, and re-reads size and mtime after the last byte. Any change → `SkipReason::Changed`
and its cache row is invalidated. `SkippedFile` lists each dropped file with a typed reason
(placeholder, reparse, id mismatch, changed, not found, access denied, sharing violation, I/O).

## Tests

`cargo test -p strata-dupes`: **37 passing** (12 unit, 11 pipeline, 6 selection, 3 hardlink,
5 doctests). Clippy `-D warnings` clean on `--all-targets`. Real files only under
`D:\strata-dupes-tests\` (fallback `%TEMP%`), removed on drop.

- Unit: candidate gate table, handle gate (every placeholder bit and cloud sub-tag, WOF on/off),
  I/O error mapping, partial windows, memory cache semantics, throttle (unlimited, rate,
  cancel while waiting), keep-suggestion table (13 cases: each reason, nested rules, `\\?\` and
  case spellings, sibling-prefix folders, unknown times, single file) and order independence.
- `tests/pipeline.rs`: identical files grouped and sorted by wasted bytes with verified ids;
  same size + different middle (partial splits) and different outside the windows (only full
  splits); hardlinks (flagged secondary, and unflagged same ref); ADS ignored and untouched;
  0-byte and below-min excluded without opening; **file rewritten between partial and full hash**
  (from the phase-start progress event) dropped as `Changed` and its cache row invalidated;
  **cancel mid full-hash then resume** (cached partials and fulls reused, third run reads 0
  bytes); placeholder simulation (`OFFLINE` set after the scan); wrong file ref; progress phases
  in order with ETA and throttled rate; selection → `QueueItem`s → `strata_clean::flow::plan` +
  `preflight` Ready, then a modified copy is caught by both `verify_selection` and pre-flight.
- `tests/selection.rs` (proptest, 2000 cases): random sequences of mark/unmark/set-keeper/
  apply/all-but-keeper with out-of-range indices and unknown groups never mark every copy, and no
  produced queue covers a whole group; any request marking all copies is refused and leaves the
  selection unchanged; plus keeper hand-off, report mismatch, queue expectations, hardlink plan
  refusals.
- `tests/hardlink.rs`: two copies replaced (same file id as the keeper, link count 3, no temp
  names left, originals recycled through strata-clean with the audit log; the test restores
  them and checks they are separate files again); a copy changed after the scan is left alone;
  a changed keeper stops every link.

The Recycle Bin entries the hardlink test creates are restored by the test itself.

## Benchmarks

16-thread desktop, Windows 11, release (`cargo bench -p strata-dupes`). Indicative (±30%).

| Measure | Result |
|---|---|
| `group_by_size`, 1,000,000 synthetic candidates (log-uniform sizes, 1% hardlink secondaries) | **76–86 ms** → 1,562 size groups, 22,936 files to measure |
| Full pipeline, 16 × 256 MiB (8 pairs), OS cache warm, concurrency 1 / 2 / 4 | **2.7 / 4.4 / 5.6 GB/s** (hash-bound) |
| Same set, rerun from cache | 0 bytes read, < 1 ms |
| Throttle at 200 MiB/s | 200 MiB/s observed |
| Read-only real folder (a Rust toolchain install, 167,792 files, 4.3 GB; listing by the bench harness 35 s, not part of the pipeline) | 41 measured, 31 fully hashed, 157 MiB read, 12 groups, 90 MiB wasted, **0.6 s** |

Cold-disk throughput could not be measured unelevated (no way to flush the standby list); on
cold data the pipeline is bound by the disk, which is why reads are large and sequential and the
default concurrency is `min(4, cpus)` (set 1–2 for spinning disks).

## Decisions

- 2026-10-09 — **Measure every size-group member from a handle before hashing.** The index stores
  the all-streams logical size and second-precision mtimes, which can neither group ADS-bearing
  files correctly nor key a cache exactly. The attributes-only open also verifies identity and
  the placeholder gate before any data handle exists. Cost: one metadata open per size-group
  member (cheap next to hashing).
- 2026-10-09 — **WOF-compressed and dedup files are included by default**
  (`allow_wof_and_dedup`): their data is local and the filter returns plain content. Every other
  reparse point is excluded. Hydrated cloud files (`LocallyAvailable`, `AlwaysKeep`) are
  excluded too: the provider can dehydrate them at any time, and hashing would race that.
- 2026-10-09 — **Synthetic walker ids are excluded** (`Exclusion::UnverifiableId`): their
  identity can't be checked on a handle and strata-clean would refuse to delete them anyway.
- 2026-10-09 — **Files in `$Recycle.Bin`/`RECYCLER` are excluded**: already deleted, and the
  never-list refuses them.
- 2026-10-09 — **Cross-volume duplicates are grouped** (content is the same); only the hardlink
  action requires one volume.
- 2026-10-09 — **Resume = cache.** Hashes are upserted every `cache_batch` (64) files while
  hashing, so a cancel, crash or restart loses at most a batch. There is no separate checkpoint
  file. A cancelled scan returns no groups (partial groups would be misleading).
- 2026-10-09 — **Cache failures are misses**, counted in `ScanStats::cache_errors`; the scan
  never fails because SQLite did.
- 2026-10-09 — **Hashing shares files with everyone** (`FILE_SHARE_READ|WRITE|DELETE`) so a scan
  never blocks another app; changes during a read are caught by the before/after facts check.
- 2026-10-09 — **Keep ranking order:** user rule > not temp/cache > not Downloads > oldest >
  shortest path > path. The reason is the first criterion separating the winner from the
  runner-up. Downloads comes only from resolved known folders (localized/redirected safe); temp
  and cache folders also match by name (`temp`, `tmp`, `*cache*`), since app cache folder names
  are not localized.
- 2026-10-09 — **Guardrail as a type:** a keeper index that is never marked, private fields, no
  `Deserialize`. Marking the keeper hands the role to an unmarked copy or fails.
  `marked_files`/`to_queue_items` re-check the invariant and that keeper and marked copies are
  distinct files. Queue item ids encode `(group << 24) | index`.
- 2026-10-09 — **Hardlink action implemented** (not left out): link under a temporary name first
  (nothing removed yet), recycle the original through `strata_clean::flow::{plan, execute}`
  (never-list, TOCTOU pre-flight, write-ahead audit), then rename with `MoveFileExW` without
  `REPLACE_EXISTING`. The keeper is held open **denying writers** from its re-hash until the end
  (verified: `CreateHardLinkW` still succeeds with that handle open). Temporary links are removed
  through `strata_clean::permanent::delete_permanently` by the keeper's id, so this crate never
  deletes anything itself. On a rename failure the original is restored from the Recycle Bin when
  its name is free. Each copy is one cleaner action in the audit log. "Careful" copies are
  acknowledged by the consent, whose text names every path.

## Blockers

None. Not exercised: a real OneDrive placeholder (the dev session has no Cloud Files provider
syncing a test folder; `RECALL_ON_DATA_ACCESS` cannot be set by user code, so the simulation
uses `OFFLINE` plus injected attributes), ReFS (128-bit ids; matching uses the 64-bit file index
like strata-clean), and cold-cache throughput (needs elevation).

## Change requests

None required. Optional, for `strata-index`: expose the unnamed-stream logical size
(`Sizes::logical`) alongside `own_logical`, so ADS-bearing files need no extra measurement before
size grouping. Not worth a new 4–8 B/entry column today; `HAS_ADS` + measuring handles it.

`Cargo.lock` gained `blake3 1.8.7` (+ `arrayvec`, `constant_time_eq`, `cpufeatures`). No root
`Cargo.toml` change (`crates/*` picks the crate up).

## Wiring steps

### App (`src-tauri`)

1. **Cache adapter** over `strata_store::Store`:
   ```rust
   struct StoreCache(strata_store::Store);
   fn vk(v: &strata_dupes::VolumeKey) -> strata_store::VolumeKey {
       strata_store::VolumeKey { serial: v.serial, guid_path: v.guid_path.to_string() }
   }
   impl strata_dupes::HashCache for StoreCache {
       fn lookup(&self, v, keys) { self.0.lookup_hashes(&vk(v), &map_keys(keys)).map(map_back).map_err(|e| CacheError(e.to_string())) }
       fn upsert(&self, v, rows) { self.0.upsert_hashes(&vk(v), &map_rows(rows)).map_err(..) }
       fn invalidate(&self, v, refs) { self.0.invalidate_hashes(&vk(v), refs).map_err(..) }
   }
   ```
   `HashKey`/`CachedHash` are field-for-field copies of the store's types.
2. **Feeding it from `strata-index`** (one volume; repeat per volume and chain the iterators):
   ```rust
   let candidates = (0..index.slot_count() as u32).map(EntryId).filter(|&id| index.is_live(id) && !index.is_dir(id))
       .filter(|&id| index.own_logical(id) >= min_size)            // cheap pre-filter
       .filter_map(|id| Some(Candidate {
           volume: volume_key.clone(),
           file_ref: index.file_ref(id)?,
           path: PathBuf::from(OsString::from_wide(index.path(id).units())),
           size: index.own_logical(id),                           // includes ADS; HAS_ADS flag triggers re-measure
           mtime: index.times(id).map_or(FileTime(0), |t| t.modified.to_filetime()),
           flags: index.flags(id),
       }));
   ```
   Path prefix must be the drive (or mount) path, not the GUID path, so keep rules and the
   cleaner's never-list see user-facing paths.
3. Run `find_duplicates` on a background thread with a `CancelToken` per run; forward `Progress`
   to a Tauri Channel (it arrives from worker threads); keep the `DuplicateReport` in app state.
   `ScanConfig.keep` gets `downloads` (every profile's `KnownFolder::Downloads` from strata-win),
   `temp_dirs` (`%TEMP%`, `Windows\Temp`) and the user's rules from settings.
4. Live updates: for every modified or deleted file ref in a USN batch, call
   `cache.invalidate(&volume, &refs)` (and drop it from the in-memory report if shown).
5. Delete: `sel.to_queue_items(&report, |f| classifier.safety(&f.path))` →
   optional `verify_selection` (setting "Byte-compare before deleting", default on for
   `ByteCompare`) → unmark failures → hand the items to the existing cleanup review flow
   (`flow::plan` / `preflight` / `execute`). Use `parse_queue_item_id` to map results back.
6. Hardlinks: `hardlink::plan` → show `Prompt::text()` → on the confirm click
   `Prompt::confirm()` → `replace_with_hardlinks` with the store-backed `AuditLog` adapter.

### UI

- Duplicates view: groups sorted as returned (wasted bytes), size, copy count, hash prefix;
  per copy path, mtime, volume, the keep badge with `KeepReason::message()`.
- Selection: send `SelectionRequest { group, keep, marked }` per change; show the error text of
  `SelectionError::WouldDeleteAllCopies` inline. "Select all duplicates" = `all_but_suggested`.
- Progress: phase name, `files_done/files_total`, bytes, rate and `eta_secs` (the full-hash
  phase's ETA is the meaningful one); Cancel button; "Resume" is simply a new scan.
- Show `ScanStats.excluded` (e.g. "312 cloud-only files skipped, never downloaded") and the
  `skipped` list with reasons.
- Hardlink dialog: per `HardlinkWarning::ALL` message, explicit confirmation, results per copy
  with `LinkError` text; disabled with `HardlinkRefusal` text when not applicable.
