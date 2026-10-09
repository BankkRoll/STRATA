# strata-dupes

Finds duplicate files and turns the user's choice into safe cleanup work.

- `find_duplicates(candidates, &ScanConfig, &dyn HashCache, &CancelToken, &progress)` groups
  candidates by size, measures each one from a handle, then hashes the first, middle and last
  64 KiB (xxh3) and finally the whole file (BLAKE3), in parallel and under an optional bytes/s
  limit. It returns `ScanOutcome::Completed(DuplicateReport)` with groups sorted by wasted bytes,
  or `ScanOutcome::Cancelled`. Hashes go to the `HashCache` as they finish, so a rerun resumes.
- Cloud placeholders are never hydrated: cloud, offline and link candidates are never opened,
  every open uses `FILE_FLAG_OPEN_NO_RECALL`, and the attributes are re-checked on the handle
  before any read. Hardlinks count as one file; every file id is verified on its handle.
- `suggest_keep` picks the copy to keep (user rules, not in temp/cache, not in Downloads, oldest,
  shortest path) and says why.
- `Selection` can never mark every copy of a group. `Selection::to_queue_items` produces
  `strata_clean::flow::QueueItem`s, so deletes go through the cleaner's pre-flight, undo log and
  Recycle Bin. `verify_selection` re-checks copies by hash or byte for byte first.
- `hardlink::plan` and `hardlink::replace_with_hardlinks` replace copies on the same volume with
  hardlinks to the kept file after explicit consent, recycling the originals.

The app adapts `strata_store::Store` to `HashCache` (`lookup_hashes`, `upsert_hashes`,
`invalidate_hashes`) and calls `HashCache::invalidate` from live updates.

## Testing

```powershell
cargo test -p strata-dupes     # creates files under D:\strata-dupes-tests (or %TEMP%)
cargo bench -p strata-dupes    # grouping of 1M synthetic candidates and hashing throughput
```

Measured on a 16-thread desktop: size grouping of 1,000,000 candidates in about 80 ms; full
hashing of 4 GiB of cached data at 2.7 GB/s on one thread and 5.6 GB/s on four.
