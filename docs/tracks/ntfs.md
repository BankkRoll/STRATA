# Track: NTFS (M1 MFT scanner + CLI)

Owner paths: `crates/strata-ntfs/`, `crates/strata-cli/`, `tests/fixtures/`, this file.

## Done (mapped to SPEC)

| SPEC | What | Where |
|---|---|---|
| §6.1.1 | `ReadAt` for `[u8]`/`Vec<u8>`/`&T`, `std::fs::File`, raw volumes (`\\.\X:`, read sharing) with `IoMode::NoBuffering` (aligned direct reads, bounce buffer otherwise) and `IoMode::Sequential` | `strata-ntfs/src/io.rs` |
| §6.1.2 | Boot sector: OEM id, 0x55AA, bytes/sector, sectors/cluster incl. 2^(256-n) (up to 2 MiB), signed clusters-per-record/index, total sectors, $MFT/$MFTMirr LCN, serial; all validated | `boot.rs` |
| §6.1.3 | `$MFT` bootstrap from record 0; `$DATA` continued in extension records via resident or non-resident attribute list, read through the runs known so far, merged in VCN order | `volume.rs` |
| §6.1.4 | `$MFT:$BITMAP` skip (`ScanOptions::use_mft_bitmap`): unused chunks are not read at all | `scan.rs` |
| §6.1.5 | I/O thread → bounded crossbeam channel → rayon fixup/parse/assemble; 8 MiB default chunks (multiple of record size, validated); buffer recycling; per-record retry on read errors | `scan.rs` |
| §6.2 | `FILE`/`BAAD`/garbage/zero classification; fixups (512-byte stride); in-use and directory flags; sequence numbers; record number = MFT index; extension records merged by base reference (before or after base; stale ones counted); `$ATTRIBUTE_LIST` resident and non-resident; `$STANDARD_INFORMATION`; `$FILE_NAME` with namespace 2 skipped and one `NameLink` per remaining name; FN created time of the first link; `$DATA` unnamed/named, resident/non-resident, VCN-0 sizes, compressed/sparse total-allocated, split instances; `$REPARSE_POINT` with symlink/mount-point targets (print name, else substitute), non-resident buffers read from disk; WOF; cloud states; `$INDEX_ALLOCATION` as directory overhead; metadata flags; orphans and cycles emitted faithfully | `record.rs`, `attr.rs`, `assemble.rs` |
| §6.3 | Runlist decoder: nibble sizes ≤ 8, signed relative LCN, sparse runs, zero/negative lengths, out-of-volume LCNs, overflow, ≤ 65,536 runs | `runlist.rs` |
| §6.4 | Batches of `strata_core::ScanRecord` to a sink callback; `ScanStats` | `scan.rs` |
| §7 | Logical, allocated (resident 0, compressed/sparse total-allocated, WOF), ADS (excluding `WofCompressedData`), directory overhead | `assemble.rs` |
| §7.5, §5 | CLI reconciliation: `GetDiskFreeSpaceExW` (drives) or `$Bitmap` (images) vs. Σ allocated + other non-resident attributes; gap printed as "Unaccounted" with likely causes | `strata-cli/src/report.rs` |
| §10.1 | `USN_RECORD_V2`, `V3` (128-bit ids), `V4` range records, reason constants, fully bounds-checked | `usn.rs` |
| §21 | Synthetic coverage of every MFT-related checklist item (see Tests) | `strata-ntfs/tests/` |
| §22 | Unit, integration, property (never-panic) tests; cargo-fuzz targets + seed corpus; criterion benches; VHDX fixture scripts with golden comparison | `tests/`, `fuzz/`, `benches/`, `tests/fixtures/` |
| §23 M1 | `strata-cli scan <X:|image> [--json] [--top] [--chunk-mib] [--no-buffering|--sequential] [--mft-bitmap]` | `strata-cli` |

## Public API (what other tracks call)

```rust
use strata_ntfs::{IoMode, NtfsVolume, RawVolume, ScanOptions};

// strata-helper (elevated): full scan, streamed into the index.
let volume = NtfsVolume::open(RawVolume::open_drive('C', IoMode::NoBuffering)?)?;
let stats = volume.scan(
    &ScanOptions { cancel: Some(cancel_flag.clone()), ..ScanOptions::default() },
    |batch: Vec<strata_core::ScanRecord>| sender.send(batch),   // or index.ingest(batch)
)?;

// USN refresh (M5): one merged record, attribute lists followed.
let fresh: Option<ScanRecord> = volume.read_record(file_ref.record())?;   // None = free/extension
if fresh.as_ref().is_some_and(|r| r.id != file_ref) { /* reused: sequence changed */ }

// Journal buffers from FSCTL_READ_USN_JOURNAL.
let (next_usn, records) = strata_ntfs::parse_usn_buffer(&buf)?;
for r in records { match r? { UsnRecord::Change(c) => .., UsnRecord::Range(r) => .. } }

// Reconciliation.
let used = volume.count_used_clusters()? * volume.boot().cluster_size;
```

Other public items: `BootSector`, `MftLayout` (`record_count`, `readable_records`,
`fragment_count`), `ScanStats`, `parse_record`/`parse_fixed_record`/`RecordOutcome`/
`ParsedRecord`, `assemble`, `AttrIter`, `decode_runlist`/`encode_runlist`, `apply_fixups`,
attribute decoders, `AlignedBuf`, `usn::*` (reason constants, `FileId128`, `UsnChange`,
`UsnRange`). `test_image` (feature `test-image`) builds byte-exact images:
`ImageBuilder`, `RecordBuilder`, `NonResidentSpec`, value encoders, `sample_image()`.

Emission contract for `strata-index`:

- One `ScanRecord` per in-use base record. Extension records never appear on their own.
- Order: base records in record-number order per chunk, then records that needed completion
  (attribute lists, non-resident reparse buffers) in record-number order at the end.
- The root (record 5) links to itself (`parent == FileRef(5, 5)`, name `.`).
- Orphans (missing parent / wrong parent sequence) and cycles are emitted as-is; the index
  attaches them (SPEC §6.2). Records with no `$FILE_NAME` have empty `links`.
- `NTFS_METADATA` is set for records 0–15 except 5 and for direct children of `$Extend`
  (record 11). Deeper `$Extend` descendants (`$Extend\$RmMetadata\$Txf`, …) must inherit the
  flag from their ancestor in the index.
- `attributes` are `$STANDARD_INFORMATION` attributes plus `FILE_ATTRIBUTE_DIRECTORY` for
  directories (SI does not store that bit), matching what the walker sees.
- `SUSPICIOUS_TIME`, `HARDLINK_SECONDARY`, `ORPHAN`, `CYCLE_BROKEN`, `PARTIAL` are left for the
  index, which knows "now", link order and the tree.

## Tests

84 tests, all passing (`cargo test -p strata-ntfs -p strata-cli`):

| Suite | Count | Covers |
|---|---|---|
| strata-ntfs unit | 24 | boot sector geometries and rejection, fixups, runlists (negative offsets, malformed, round trip, run cap), attribute bounds, attr-list/reparse/SI/FN decoders, aligned I/O, USN V2/V3/V4 round trip and rejection |
| strata-ntfs `tests/cases.rs` | 15 | exact `ScanRecord`s for: resident/non-resident data; hardlinks + DOS + POSIX names and FN created time; POSIX case variants and unpaired surrogates; sparse and compressed total-allocated; WOF (and a non-WOF `WofCompressedData` counted as ADS); cloud pinned/unpinned/recall states; symlink, junction, relative symlink, unknown and WSL tags; 40 ADS + an 8 TiB sparse ADS; directory index overhead; extension records before and after the base with resident and non-resident attribute lists (scan and `read_record`); stale extensions; BAAD/torn/garbage/malformed; orphans and cycles; metadata flags incl. `$Extend` children and `$BadClus`; records with no names |
| strata-ntfs `tests/volume.rs` | 10 | 9 geometries (512 B–2 MiB clusters, 512/1024/4096-byte records, 512/4096-byte sectors), fragmented MFT with `$DATA` in an extension record, exact `$Bitmap` reconciliation; chunk boundaries and records straddling fragments; `$MFT:$BITMAP` option; cancellation; invalid chunk sizes; bad sectors; `read_record` == scan for every record; raw-volume modes over a file (both flags, real `FILE_FLAG_NO_BUFFERING`); boot/MFT bootstrap failures; 120-fragment MFT |
| strata-ntfs `tests/robustness.rs` | 9 | proptest never-panic: arbitrary bytes as records, forced `FILE` headers, mutations of valid records, runlists (with output invariants), attribute lists, reparse buffers, USN buffers, boot sectors, mutated whole images (open + scan + `read_record` + `$Bitmap`) |
| strata-ntfs doctests | 12 | public examples |
| strata-cli unit | 9 | argument parsing, path reconstruction (nested, orphan, stale sequence, cycle, memo), top-N heap, sizes, flag names, token elevation, disk space |
| strata-cli `tests/cli.rs` | 4 | binary against builder images: report, top-N paths with hardlinks, zero "Unaccounted"; golden JSON in both I/O modes with `--mft-bitmap`; exit 2 when unelevated on a drive; usage/open/boot errors |
| strata-cli doctest | 1 | `parse_args` |

`cargo clippy -p strata-ntfs -p strata-cli --all-targets -- -D warnings` and
`cargo fmt --all -- --check` are clean.

## Benchmarks

`cargo bench -p strata-ntfs --bench parse` (criterion, release, a desktop 8-core/16-thread x64 CPU, Windows 11).
**The machine was at 100% CPU from other processes during every run**, so these are pessimistic
and noisy (wide confidence intervals); rerun on an idle machine.

| Bench | Result | Per record |
|---|---|---|
| `parse/1M_records_single_thread` (memcpy 1 KiB + fixups + parse + assemble, 64k distinct records cycled) | 0.64–1.02 s per 1M | 0.64–1.0 µs |
| `parse/1M_records_rayon` (same, rayon over 64 MiB refills, memcpy included) | 155–291 ms per 1M (3.4–6.4 M rec/s) | 155–291 ns |
| `pipeline/in_memory_image_200k` (`NtfsVolume::scan` end to end: I/O thread, channel, rayon, sink) | 177–235 ms per 200k (0.85–1.1 M rec/s) on the first run; 325–525 ms on a later, more contended run | 0.9–2.6 µs |

Against the SPEC §2 target of ≤ 3 s for 1M files end to end on NVMe: the CPU side parses 1M
records in ~0.2–0.3 s in parallel, and the full pipeline over memory runs ~1M records/s even
on a saturated machine. Reading a 1 GiB MFT on NVMe (~2–3 GB/s) overlaps with parsing, so the
budget has ample headroom. The remaining cost is allocation (≈5 small allocations per record:
names, links, data pieces, boxed record); switching the helper binary to a faster global
allocator (e.g. mimalloc) is the next lever if needed. End-to-end numbers on a real volume
need an elevated run (`strata-cli scan C:` prints records/s), as does the
`--no-buffering` vs `--sequential` comparison.

## Decisions

- 2026-10-09 — **Minimum MFT record size is 512 bytes, not 256.** The fixup stride is fixed at
  512 bytes (`usa_count - 1 == record_size / 512`); a 256-byte record could not carry a valid
  update sequence array.
- 2026-10-09 — **Every `$INDEX_ALLOCATION` counts as `dir_overhead`, not only `$I30`.** View
  indexes (`$Secure:$SDH/$SII`, `$ObjId:$O`, `$Quota:$O/$Q`, `$Reparse:$R`) are real disk usage
  on metadata files; dropping them would leave an unexplained reconciliation gap.
- 2026-10-09 — **`$BadClus:$Bad` allocation is computed from its runs.** Its header claims the
  whole volume and does not always carry the sparse flag; header allocation would report the
  disk as full. Reserved records (< 16) always decode runlists for this.
- 2026-10-09 — **Non-resident attributes that are not file content (`$ATTRIBUTE_LIST`,
  `$BITMAP`, `$EA`, `$LOGGED_UTILITY_STREAM`, non-resident reparse buffers) are summed into
  `ScanStats::other_attr_allocated`**, because `Sizes` has no field for them (see core change
  request). The CLI adds them to the accounted total, which makes builder images reconcile to
  0 bytes.
- 2026-10-09 — **Zero-signature records count as free**; records beyond `$MFT`'s initialized
  size are counted free without being read.
- 2026-10-09 — **A failed chunk read is retried record by record**; only unreadable records are
  lost (`ScanStats::unreadable`). The scan fails only if no record of a chunk can be read.
- 2026-10-09 — **Records needing completion are deferred to the end of the pass** (attribute
  list or non-resident reparse buffer); all others are emitted as their chunk completes. The
  full scan merges extension records by base reference; `read_record` follows the attribute
  list instead.
- 2026-10-09 — **`read_record` returns `None` for extension records** (they are part of another
  file) and an error for corrupt ones.
- 2026-10-09 — **`ScanRecord::attributes` includes `FILE_ATTRIBUTE_DIRECTORY` for
  directories.** `$STANDARD_INFORMATION` omits it; the walker's Win32 attributes include it.
- 2026-10-09 — **A `WofCompressedData` stream is special only when the reparse tag is WOF.**
  Otherwise it is an ordinary ADS.
- 2026-10-09 — **Default I/O mode: `NoBuffering` for drives, `Sequential` for images.** SPEC
  §6.1 lists unbuffered first; image files benefit from the cache. Both are selectable.
- 2026-10-09 — **Unbuffered alignment is a fixed 4096 bytes** (a multiple of 512e and 4Kn sector
  sizes), so no device query (`IOCTL_DISK_GET_DRIVE_GEOMETRY`) and no unsafe code is needed.
  `strata-ntfs` has no `unsafe` at all.
- 2026-10-09 — **CLI exit codes: 0 ok, 1 failure, 2 not elevated, 64 usage error.** Elevation is
  checked with `GetTokenInformation(TokenElevation)` before opening the drive.
- 2026-10-09 — **Golden JSON (`strata-golden/1`) has one entry per path**, hardlinks
  distinguished by `link_index`; paths use U+FFFD for unpaired surrogates. Documented in
  `tests/fixtures/README.md`.
- 2026-10-09 — **The synthetic builder can "claim" more clusters than it backs with bytes**, so
  tests can describe terabyte files without allocating them.
- 2026-10-09 — **Fuzz crate keeps its own `[workspace]`** under `crates/strata-ntfs/fuzz`, and
  defines the COFF sancov section bounds in a data-only lib (see Blockers).

## Blockers

- **Not elevated.** No raw volume, no VHDX mount, no USN journal. Consequences: the fixture
  scripts (`tests/fixtures/`) have not run end to end and `tests/fixtures/golden/` is empty;
  there are no real-volume timings, no `--no-buffering` vs `--sequential` comparison, and no
  WizTree comparison. What was verified instead is listed in `tests/fixtures/README.md`
  ("Status"). To unblock: from an elevated terminal run
  `cargo build --release -p strata-cli`, `strata-cli scan C:` (records/s, reconciliation) and
  `tests\fixtures\Invoke-StrataFixtures.ps1 -OutDir D:\strata-fixtures -UpdateGolden`.
- **Coverage-guided fuzzing on this machine.** `cargo-fuzz` is not installed and the MSVC
  AddressSanitizer runtime is absent. Building the targets by hand with nightly-2026-07-01 and
  the sancov flags cargo-fuzz uses works (after defining the COFF section bounds in
  `fuzz/src/lib.rs` and forcing `/include:main`), but libFuzzer then aborts with "The size of
  coverage PC tables does not match the number of instrumented PCs" (1328 counters vs 1335 PC
  entries, most likely alignment padding between `.SCOVP$M` contributions from different
  objects; compiler-rt's own Windows section file is built for the ASan runtime, which is not
  installed). Without the PC table, LLVM fails on
  crossbeam-epoch ("Associative COMDAT symbol does not exist"). Run the targets on Linux/WSL
  (`cargo +nightly fuzz run record -- -max_total_time=600`) or on Windows with the "C++
  AddressSanitizer" VS component and `cargo fuzz run`. The stable proptest suite
  (`tests/robustness.rs`) mirrors every target and runs in `cargo test` (512 cases per
  property, 48 for whole images); `PROPTEST_CASES=200000 cargo test --release -p strata-ntfs
  --test robustness` found no panics.
- **EFS and case-sensitive directories** could not be exercised locally (Windows Home; WSL
  feature absent). The fixture script records them as skipped when unavailable.

## Core change requests

1. Per-record accounting for non-content non-resident attributes, so reconciliation can be
   attributed to files instead of a volume-level stat (e.g. a 100k-link file's attribute list,
   `$MFT:$BITMAP`):

   ```diff
   --- a/crates/strata-core/src/record.rs
   +++ b/crates/strata-core/src/record.rs
   @@ pub struct Sizes {
        /// Directory B-tree (`$INDEX_ALLOCATION`) bytes; 0 for files.
        pub dir_overhead: u64,
   +    /// On-disk bytes of non-resident attributes that are neither content
   +    /// nor directory indexes: `$ATTRIBUTE_LIST`, `$BITMAP`, `$EA`,
   +    /// `$LOGGED_UTILITY_STREAM`, non-resident `$REPARSE_POINT`.
   +    pub attr_overhead: u64,
    }
   @@ impl Sizes {
        pub const fn total_allocated(&self) -> u64 {
            self.allocated
                .saturating_add(self.ads_allocated)
                .saturating_add(self.dir_overhead)
   +            .saturating_add(self.attr_overhead)
        }
   ```

   Local workaround in place: `ParsedRecord::other_allocated` per record and
   `ScanStats::other_attr_allocated` per scan. If accepted, `assemble` sums the parsed values
   into `attr_overhead` and the stat becomes redundant.

## Next steps

1. Elevated run: `strata-cli scan C:` for real timings (both I/O modes) and reconciliation,
   then the fixture suite with `-UpdateGolden`; commit `tests/fixtures/golden/*.json` and record
   numbers in `docs/BENCHMARKS.md`.
2. Run the fuzz targets for a bounded time in CI on a Linux runner (corpus in
   `crates/strata-ntfs/fuzz/corpus`, regenerate with
   `cargo run -p strata-ntfs --features test-image --example gen_fuzz_corpus`).
3. M3: the helper streams `scan` batches over the pipe; `strata-index` consumes the emission
   contract above (propagate `NTFS_METADATA` below `$Extend`, compute `HARDLINK_SECONDARY`,
   orphans and cycles).
4. M5: live tailing on top of `parse_usn_buffer` + `read_record`.
5. If profiling on an idle machine shows allocation dominating, try mimalloc in the helper and
   a pooled name arena in `ParsedRecord`.
