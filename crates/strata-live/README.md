# strata-live

Keeps a volume's in-memory index (`strata-index`) in step with the disk after a scan.

- **USN tailing.** `Tailer` reads the NTFS change journal (`USN_RECORD_V2/V3/V4`) through a
  `JournalSource`, coalesces records per file within a tick (250 ms by default), re-reads changed
  files through a `RecordSource`, applies them with `Index::apply` and emits one merged
  `ChangeSet` per tick as `LiveEvent::Tick`. An idle volume costs one blocked read and no CPU.
- **Typed outcomes.** `Halt::NeedsRescan(RescanReason)` (journal wrapped or recreated, malformed
  data), `Halt::JournalDisabled`, `Halt::Stale(StaleReason)` (disconnect, dismount) and
  `LiveStatus::CatchingUp { pending, .. }` during bursts. `Tailer::resume` catches up after a
  reconnect or wake from sleep.
- **Cache and catch-up.** `CacheFile` writes atomically (temporary file, flush, rename) and
  stores the last applied USN; `Tailer::start` validates it with `check_position` and replays
  to the journal head on launch.
- **Volumes without a journal.** `SubtreeWatcher`, `RescanPlanner` and `reconcile_subtree` fold
  folder re-walks back into the index.

The crate makes no OS calls besides the cache file: the helper or app implements
`JournalSource`, `RecordSource` and `SubtreeWatcher`.

```rust
let mut tailer = Tailer::start(TailerConfig::default(), &mut journal, index_position(&index))?
    .with_cache(CacheFile::new(cache_path));
let halt = tailer.run(&mut journal, &mut records, &mut index_lock, &SystemClock, &stop, &mut on_event);
```

Measured on a 16-thread desktop: about 316,000 journal records/s applied end to end into a
1,000,000-entry index; record-to-tick latency 251–265 ms with the default tick; 500,000 files
created at once drain in 1.9 s with ticks under 70 ms and index lock holds under 25 ms.

Test with `cargo test -p strata-live` (`STRATA_PROPTEST_CASES=20000` for a longer soak) and
benchmark with `cargo bench -p strata-live`.
