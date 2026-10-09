# Track: fallback walker (`strata-walk`)

Status: **feature-complete for M2's walker half.** Reconciliation against the MFT scanner
on fixtures is still open: it needs `strata-ntfs` and the VHDX fixture volumes.

## Done (mapped to SPEC)

| SPEC | What |
|---|---|
| §8 traversal | Parallel work-stealing traversal: one rayon task per directory, large directories' allocation pass split into 1024-file chunk tasks. |
| §8 listing | Both methods: `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` with a 64 KiB buffer on a handle opened `FILE_LIST_DIRECTORY \| SYNCHRONIZE` + backup intent (**default**), and `FindFirstFileExW(FindExInfoBasic, FIND_FIRST_EX_LARGE_FETCH)`. If a filesystem rejects `FileIdExtd…`, the walker falls back to `FileIdBothDirectoryInfo` and then `FileFullDirectoryInfo`. `WalkStats::dir_info_fallback` reports when that happens. |
| §8 / §21 paths | Every path is in `\\?\` form (`\\?\UNC\` for shares), and every open goes through `NtCreateFile` with `\??\` NT paths or handle-relative names. Paths and names stay raw UTF-16 end to end. Tested: paths over 400 chars, 1,100-level nesting, trailing dot and space, `CON`/`NUL.txt`/`AUX`/`COM1`/`lpt1.log`/`PRN` (as a directory), leading space, emoji, RTL, unpaired high and low surrogates, and per-directory case sensitivity (`A.txt` next to `a.txt`). |
| §6.2 / §7.3 / §8 reparse | Reparse points are never followed: every open uses `FILE_OPEN_REPARSE_POINT`. The tag comes from the listing (`ReparsePointTag`, `EaSize` or `dwReserved0`) and is classified with `ReparseKind::from_tag`. A directory is descended only if `!blocks_traversal()`. Symlink and junction/mount-point targets are read with `FSCTL_GET_REPARSE_POINT`; volume mount points render as `\\?\Volume{…}\` and act as boundaries. The walk root is the one path that is followed. |
| §7 sizes | Logical size comes from the listing. Allocation comes from the listing (option b) or from the allocation pass. Without either, it is estimated (logical rounded up to the cluster size, 0 for recall/offline content) and flagged `ALLOC_ESTIMATED`. Compressed and sparse files use `FileCompressionInfo` (the handle form of `GetCompressedFileSizeW`). NTFS-resident streams count 0, matching the MFT rule. Directory index overhead is filled from `FileStandardInfo` on the directory handle. |
| §7.4 / §21 cloud | Nothing ever opens with data access. File opens add `FILE_OPEN_NO_RECALL`. Files with recall/offline bits get only `FileStandardInfo`, with no stream or compression queries. Directories with `RECALL_ON_OPEN` (unpopulated placeholders) are recorded with `PARTIAL` and not listed. `CloudState` comes from attributes. |
| §6.2 ADS | `FileStreamInfo` on the handle the pass already holds gives each named stream's size **and allocation**, which `FindFirstStreamW` cannot. `WofCompressedData` is folded into allocation and never reported as an ADS. Directory ADS are reported too. |
| §7.2 hardlinks | One record per file id. All links found inside the root are merged (see below). |
| Identity | Real 64-bit NTFS/ReFS file ids (they equal the MFT file reference: record + sequence) for files and directories. Everything else gets a synthetic id with `SYNTHETIC_BIT`. The root links to itself, and its name is the root's display path. |
| §8 / §21 errors | An access-denied directory is emitted with `ACCESS_DENIED` and the walk continues. A vanished directory or file is dropped. A directory replaced by a file is re-stat'ed and emitted as the file now there. A file that is denied or locked (e.g. `pagefile.sys`) keeps its listing data. Errors are counted by kind (`ErrorCounts`). |
| §8 network | Workers are capped at `network_concurrency` on UNC/remote volumes. Each blocking request (one listing, one 64-file allocation chunk, one reparse read) runs on the worker's private I/O thread with a deadline. On timeout or cancel the walker calls `CancelSynchronousIo` and waits 250 ms. If the call still hasn't returned, the I/O thread is abandoned and replaced. The affected directory is flagged `PARTIAL`. Tested against a real blocking kernel read and a `\\localhost\C$` walk. |
| §8 cancellation | The cancel token is checked between directories, between 64 KiB listing buffers or 256 `FindNextFileW` entries, and every 64 files in the allocation pass. Unvisited and cut-short directories are emitted with `PARTIAL`, held hardlink records are flushed, and `WalkStats::{cancelled, partial}` are set. On C:\ a cancel returns in **48–180 ms**. |

## Public API

```rust
let walker = Walker::new(root, WalkOptions {
    threads,              // default 2 x logical CPUs, clamped to 4..=64
    network_concurrency,  // default 8
    timeout,              // per network request, default 30 s
    allocation_pass,      // default true
    listing,              // ListingMethod::DirectoryInfo (default) | FindFirstFile
    batch_size,           // records per sink batch, default 4096
    progress_interval,    // default 100 ms
})?;
let stats: WalkStats = walker.run(&mut sink, &cancel)?;   // cancel: CancelToken
```

- `trait WalkSink { fn records(&mut self, Vec<ScanRecord>); fn progress(&mut self, &Progress) {} }`
  is called **on the caller's thread**, so it needs neither `Send` nor `Sync`. Workers feed it
  through a bounded queue, which gives backpressure.
  Provided sinks: `Vec<ScanRecord>`, `FnSink::new(|batch| ..)` /
  `FnSink::with_progress(..)`, and `ChannelSink` (forwards `WalkEvent::{Records, Progress}` to a
  crossbeam channel for an IPC pump).
- Contract: every id is delivered exactly once, and a directory may arrive before or after its
  children.
- `WalkStats`: `totals: Progress` (dirs, files, logical/allocated bytes, errors, elapsed),
  `access_denied_dirs`, `partial_dirs`, `estimated_allocations`, `hardlinks_merged`,
  `errors: ErrorCounts`, `cancelled`, `partial`, `dir_info_fallback`, and
  `volume: Option<VolumeStats>` (mount, filesystem, cluster size, total/free/used from
  `GetDiskFreeSpaceExW`, `is_network`).
- `WalkError` covers only root problems (`RootNotFound`, `NotADirectory`, `InvalidRoot`) and
  pool creation. Everything inside the tree is a flag or a counter.
- There is no separate `refine_allocation` phase: the allocation pass runs inline per
  directory, in parallel with listing, so every record is final when it reaches the sink
  (see decisions).

## Hardlink timing and merging

With the allocation pass on, `NumberOfLinks` comes from `FileStandardInfo`. A file with more
than one link goes into a sharded table keyed by its 128-bit file id, and the table merges
`links` as sightings arrive from any thread. When the collected link count reaches
`NumberOfLinks`, the merged record is emitted. At the end of the walk, or on cancel, any record
still waiting (some links lie outside the root) is emitted with the links found. Memory is
bounded by the number of hardlinked files.

With the pass off there are no link counts, so the walker can't hold records back. Ids still
stay unique: the first sighting keeps the real id, and later sightings get a synthetic id plus
`HARDLINK_SECONDARY`. `FindFirstFile` without the pass has no ids at all, so hardlinks are
double counted (and every allocation is flagged estimated).

On network volumes and FAT, ids are synthetic and hardlinks are not merged.

## Tests

`cargo test -p strata-walk`: **51 unit/integration tests + 5 doctests, all green.**
`cargo clippy -p strata-walk --all-targets -D warnings` is clean.

All fixtures are real trees in `%TEMP%`, and no admin is needed. Each structural test runs
across the four `{DirectoryInfo, FindFirstFile} x {alloc on, off}` modes and asserts exact
records, sizes, flags and ids. Coverage:

- Nested tree, exact sizes, resident-file 0 allocation, and ids equal to the NTFS file id.
- Directory index overhead.
- More than 260 chars, 1,100-level nesting.
- Unusual names: reserved device names, trailing dot and space, leading space, emoji, RTL,
  unpaired surrogates.
- Case-sensitive directory.
- Junctions, including two cycles back to the root: recorded with target, not traversed.
  Also a junction given as the walk root.
- Symlinks: **skipped on this machine** with the logged reason (no Developer Mode, error 1314).
  The target parser is unit-tested on symlink buffers.
- Hardlinks: 3 links merged, plus a link whose sibling is outside the root. Also the fast-mode
  secondary flagging.
- Sparse: 100 MiB logical, 64 KiB allocated.
- LZNT1-compressed: allocation equals the compressed size.
- WOF (`compact /exe:xpress4k`).
- ADS on files and directories, with per-stream allocation.
- Access-denied directory (an `icacls /deny (RD)` ACE, removed on drop).
- Mid-scan races, injected deterministically through test-only hooks: directory replaced by a
  file, directory deleted before listing, file deleted between listing and probe.
- Root errors.
- Cancellation: mid-walk, and with a pre-cancelled token.
- Sinks and progress monotonicity.
- Both listing methods produce identical records after the allocation pass.
- All three information classes parse the same real listing.
- Walk over `\\localhost\C$`.
- Timed runner: timeout, cancel, and `CancelSynchronousIo` aborting a real blocking
  synchronous-pipe read with `ERROR_OPERATION_ABORTED`.
- Proptest: parsers never panic on arbitrary bytes.

## Benchmarks

Machine: desktop 8-core/16-thread CPU, C: = 500 GB NVMe (NTFS, 4 KiB clusters), D: = 2 TB
NVMe. Unelevated, release build, `examples/walkbench.rs`. Other agents were building on the
same machine, so expect ±20% noise. Runs after the first are warm-cache: the standby list
can't be purged without admin.

**Full `C:\`** (4.62 M records with the pass, 5.47 M without; 32 threads):

| Listing | Alloc pass | Time | Entries/s | s per 1M entries | Allocated |
|---|---|---|---|---|---|
| DirectoryInfo | on | 107.7 s / 93.7 s | 43–49 k | **23.3 / 20.3** | 430.7 GiB |
| FindFirstFile | on | 91.1 s | 51 k | 19.7 | 430.7 GiB |
| DirectoryInfo | off | 16.3 s | 336 k | 2.97 | 462.0 GiB (hardlinks/WOF counted wrong) |
| FindFirstFile | off | 13.4 s | 408 k | 2.45 | 464.5 GiB (all estimated) |

Other measurements on C:\:

- 105 directories were access-denied. 432 access-denied errors in total: those 105
  directories plus 327 file opens.
- 4 sharing violations (`pagefile.sys` and friends; their listing sizes are kept).
- 851,615 hardlink names merged.

**`C:\Users\me`** (a developer profile) (4.13 M records): DirectoryInfo + pass at 32 threads took 82.8 s
(20.1 s/1M). At 64 threads it took 84–88 s, at 16 threads 141 s, at 8 threads 237 s. Listing
only took 12.0 s (2.4 s/1M). The first run, cold and concurrent with tree generation, took
385 s.

**Synthetic `D:\strata-walk-tests\tree1m`** (1,000,000 files, 10,211 dirs, sizes 0–16 KiB):

| Listing | Alloc pass | Time | s per 1M |
|---|---|---|---|
| DirectoryInfo | on | 13.3 s (10.3 s at 8 threads) | 13.2 |
| FindFirstFile | on | 13.7 s | 13.6 |
| DirectoryInfo | off | **0.34 s** | 0.33 |
| FindFirstFile | off | 0.37 s | 0.37 |

Allocation-pass thread scaling on D: (warm, CPU-bound): 1 thread 57 s, 4 threads 13.8 s,
8 threads 10.3 s, 16 threads 14.1 s, 64 threads 14.2 s. Per-file cost single-threaded is about
20–25 µs: roughly 18 µs for the open/close itself plus about 6 µs for `FileStreamInfo`.
`NtQueryInformationByName(FileStatInformation)` (no handle) was tried and was not measurably
cheaper, so it was dropped.

**Target ≤ 30 s per 1M files: met** with the full allocation pass (20–23 s/1M on real C:,
13 s/1M on D:). Listing alone is 0.3–3 s/1M.

### Reconciliation with `GetDiskFreeSpaceExW` (C:\, pass on)

Used space is 458.3 GiB. The walk found 430.7 GiB allocated (files + ADS + directory index),
which leaves **27.6 GiB unaccounted**:

- `System Volume Information`: shadow copies and restore points. Access denied unelevated;
  `vssadmin list shadowstorage` also needs admin. Likely the bulk of the gap.
- NTFS metadata the walker can't see: `$MFT` (about 1 KiB × ~5.5 M records ≈ 5+ GiB including
  free records and the MFT zone), `$LogFile`, `$UsnJrnl:$J`, `$Secure`, `$Bitmap`.
- The 105 access-denied directories, e.g. `C:\Windows\System32\config`, other users' profiles,
  and `WindowsApps` internals.
- Already counted, so not part of the gap: `hiberfil.sys` (12.7 GiB), `pagefile.sys` (5 GiB),
  `swapfile.sys`. They are included from listing data even though opens fail with a sharing
  violation.

With the pass off, the sum *exceeds* used space (462–465 GiB). Two causes: 851 k hardlink names
double counted (WinSxS/System32 plus package-manager stores in the profile), and WOF files
estimated at full size. That is the concrete case for keeping the pass on by default.

## Decisions

- 2026-10-09 — **Default `ListingMethod::DirectoryInfo` (`FileIdExtdDirectoryInfo`).** Listing
  speed is a wash: 0.33 vs 0.37 s/1M on D:, and 2.97 vs 2.45 s/1M on C:, within noise. But it
  returns the 64-bit file id (equal to the MFT reference), exact allocation, change time and
  the reparse tag with no per-file open. It keeps a directory handle open, which gives the
  directory's id, index overhead and ADS, and allows handle-relative opens in the allocation
  pass. `FindFirstFile` remains selectable, and the tests prove both produce identical records
  after the pass.
- 2026-10-09 — **Allocation pass on by default, inline per directory**, not a separate
  `refine_allocation` phase. Option (b) already gives exact allocation for ordinary files, so
  the pass exists for what listings cannot provide:
  - hardlink counts (851 k extra names on C:, about 30 GiB of double counting otherwise);
  - ADS;
  - WOF files: the WOF filter hides both the reparse tag and the allocation from listings
    *and* from `FileAttributeTagInfo`, so the listing reports 0 bytes.

  It costs about 6–7× the listing time but still meets the 30 s/1M target. Running it inline
  means every record reaches the sink final, ids never change, and hardlinks can be merged
  before emission. A two-phase fast-then-refine mode would need record replacement in the
  index. `allocation_pass: false` is the 0.3–3 s/1M quick mode, and every gap it leaves is
  flagged (`ALLOC_ESTIMATED`, `HARDLINK_SECONDARY`).
- 2026-10-09 — **Scheduler: `rayon::scope`**, one task per directory and per 1024-file chunk.
  Rayon is per-worker crossbeam-deque work stealing, which is the design in SPEC §8. The pure
  listing walk reaches 3 M entries/s on 32 threads (10 k directories in 0.34 s), so scheduling
  is not on the critical path. 97% of a default walk is the syscall-bound allocation pass. A
  hand-rolled crossbeam-deque pool was therefore not built or benchmarked separately. If
  profiling ever shows scheduler overhead, it is a drop-in swap inside `walker.rs`.
- 2026-10-09 — **Default threads = 2 × logical CPUs (clamp 4..64).** On a cold-ish C:, the pass
  is MFT-record-I/O bound and keeps scaling up to 32–64 threads (237 s at 8 threads, 83 s at
  32). On a warm D: it peaks at 8 threads (10.3 s vs 13.3 s at 32). The cold case matters more.
- 2026-10-09 — **All opens go through `NtCreateFile`.** It takes handle-relative names, NT
  paths with no Win32 normalisation, and `FILE_OPEN_NO_RECALL` / `FILE_OPEN_REPARSE_POINT` /
  `FILE_OPEN_FOR_BACKUP_INTENT` create options. `FILE_OPEN_NO_RECALL` is rejected together with
  `FILE_DIRECTORY_FILE`, so directory opens omit it.
- 2026-10-09 — **NTFS allocations that are not whole clusters count as 0.** NTFS reports a
  resident stream's allocation rounded to 8 bytes. The MFT rule (SPEC §6.2) counts resident
  data as 0.
- 2026-10-09 — **A non-empty, non-sparse, non-compressed, non-remote file whose listing
  reports 0 allocation is treated as a hidden WOF file.** With the pass off it is flagged
  `ALLOC_ESTIMATED`; the pass reads the real (compressed) allocation.
- 2026-10-09 — **Real ids only on local NTFS/ReFS.** FAT ids encode directory-entry positions,
  and SMB servers may synthesise ids. 128-bit ReFS ids that don't fit 64 bits get a synthetic
  `FileRef` but still merge hardlinks by the full 128-bit key.
- 2026-10-09 — **Unpopulated cloud directories (`RECALL_ON_OPEN`) are not listed.** Listing
  them makes the provider fetch the contents. They are recorded with `PARTIAL`.

## Known gaps

- The walker can't flag WOF files: the WOF filter hides the tag, so `reparse()` is `None`
  where the MFT scanner says `Wof`. Allocation is correct.
- Directories don't get `fn_created` or `NTFS_METADATA`, because the walker never sees `$`
  files.
- `PARTIAL` is set on the directories that are themselves incomplete. Ancestors are not
  re-flagged, because they were already emitted. The index should propagate `PARTIAL` upward
  during aggregation.
- Symlink fixtures are skipped on machines without Developer Mode.
- A hung SMB call that ignores `CancelSynchronousIo` leaves one abandoned thread until the
  redirector gives up. This is documented in `timed.rs`, and Win32 has no way to force-cancel.

## Blockers

None.

## Core change requests

None required. Suggestions for the index track:

- Propagate `PARTIAL` and `ACCESS_DENIED` to ancestors when aggregating.
- Treat `HARDLINK_SECONDARY` on input records as "already deduplicated", for walker fast mode.

## Next steps

1. M2 reconciliation: once `strata-ntfs` and the VHDX fixture scripts land, run both scanners
   on the same fixture volume and assert per-path equality. Expected differences: WOF flag,
   NTFS metadata, `fn_created`.
2. Measure on ReFS/Dev Drive and exFAT. This exercises the `FileIdBoth`/`Full` fallback; it
   needs an elevated VHDX mount.
3. Optional two-phase mode for the UI: listing-only first (0.3–3 s/1M), then refine. This
   needs an index "replace record" operation.
