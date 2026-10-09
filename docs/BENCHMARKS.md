# Benchmarks

How Strata's performance is measured, how to reproduce each number, and the results.
Component designs are described in the [README](../README.md#how-it-works).

## Methodology

- **Release builds only.** Criterion benches use the `bench` profile (release with debug info);
  examples and the store bench run with `--release`.
- **Synthetic data** where a result must be reproducible and machine-independent: deterministic
  generators with fixed seeds model the shapes that dominate real disks (dependency trees,
  WinSxS-style hardlink farms, media libraries, hex-named caches). Index data averages 11.4%
  directories and about 16.5 name bytes per entry; layout trees use fan-out 16, 20% directories,
  depth 12 and log-uniform sizes from 1 B to 4 GiB with 2% empty files.
- **Real data** where the operating system is the cost: the standard scanner is timed on a real
  system volume (about 4.6 million entries) and a generated 1-million-file tree.
- **MFT parsing** is measured on in-memory NTFS images built byte-exactly by the test-image
  builder, isolating CPU cost from disk speed. Real-volume end-to-end scans are timed with the
  CLI (below).
- **UI frame times** come from a headless Chromium (the WebView2 engine) on the real GPU through
  ANGLE/D3D11. Headless Chromium paces animation frames at 100 Hz, so 100 fps is the ceiling of
  that measurement.
- **Reference machine:** 8-core/16-thread desktop CPU, NVMe SSDs, mid-range discrete GPU,
  Windows 11. Runs were taken on a busy machine, so treat results as upper bounds with
  ±20–30% run-to-run noise. Ranges are the spread across runs.

## Reproducing

| Area | Command |
|---|---|
| MFT parse | `cargo bench -p strata-ntfs --bench parse` |
| MFT scan, real volume (elevated terminal) | `cargo build --release -p strata-cli` then `target\release\strata-cli scan C:` (prints records/s and the reconciliation) |
| Standard scanner | `cargo run --release -p strata-walk --example walkbench -- <root> [dirinfo\|find] [alloc 0\|1] [threads]` |
| Standard scanner test tree | `cargo run --release -p strata-walk --example mktree -- <root> 1000000` |
| Index, search, live updates, cache | `cargo bench -p strata-index` (`STRATA_BENCH_CRITERION=0` prints the report only; `STRATA_BENCH_MAX=1000000` skips 5M) |
| Layout and picking | `cargo bench -p strata-layout` |
| Pipe throughput and codec | `cargo bench -p strata-ipc` |
| Classifier | `cargo run --release -p strata-classify --example bench_classify` |
| Live updates | `cargo bench -p strata-live` |
| Duplicates | `cargo bench -p strata-dupes` |
| Activity tracking | `cargo bench -p strata-etw` |
| App pipeline | `cargo run --release -p strata-app --example pipeline_bench -- <folder>` |
| History store | `cargo test -p strata-store --release --test bench -- --ignored --nocapture` |
| UI rendering | `pnpm --dir ui fixtures:large`, then `pnpm --dir ui dev` and in another terminal `node ui/scripts/run-harness.mjs large [treemap\|sunburst\|icicle\|flame\|bubbles\|mindmap]` |

## Results

| Metric | Result | Target |
|---|---|---|
| **MFT scanner** | | |
| Parse 1M MFT records, parallel (fixups + parse + assemble) | 155–291 ms (3.4–6.4 M records/s) | ≤ 3 s per 1M files end to end |
| Parse 1M MFT records, single thread | 0.64–1.02 s | — |
| Scan pipeline end to end over an in-memory image (I/O thread, channel, parser, sink) | 0.85–1.1 M records/s | — |
| **App pipeline** (standard scanner, 4.96M-entry user profile, release build) | | |
| Scan start to first usable treemap | 151–168 ms | — |
| Finish after the walk (build, classify, remap) | 3.2–3.4 s | — |
| Treemap of a 103.5k-entry folder from the live index | 3.2 ms | — |
| Index memory, names excluded | 59.6 B per entry | — |
| **Standard scanner** | | |
| System volume, full accuracy (allocation pass on) | 20–23 s per 1M entries | ≤ 30 s per 1M |
| System volume, listing only (allocation pass off) | 2.5–3.0 s per 1M entries | — |
| Generated 1M-file tree, allocation pass on | 13.2 s | ≤ 30 s |
| Generated 1M-file tree, listing only | 0.34 s | — |
| Cancel to return | 48–180 ms | — |
| **Index** | | |
| Memory per entry, names excluded | 58.7 B at 1M, 59.2 B at 5M (lite: 42.7 B) | ≤ 64 B |
| Build, 1M entries (stage + finish) | 0.60 s | — |
| Live updates applied, 1M index, mixed batches of 1,000 | 216k–334k updates/s | — |
| Cache file, 1M entries | 75.2 MB; save 42–51 ms; load + full validation 26–43 ms | — |
| Children of a 100k-entry folder, first 200 by size | 8.0 ms | — |
| Top 100 largest files, 5M entries | 12.2 ms | — |
| **Search** (5M names) | | |
| First results | ≤ 1.1 ms | ≤ 50 ms |
| Complete scan, any query tried (substring, prefix, wildcard, path, regex, filters) | ≤ 28 ms | — |
| **Layout** | | |
| Treemap relayout, 100k-entry subtree | 3.2 ms | ≤ 50 ms |
| Treemap, 1M-entry tree, default level of detail | 6.8 ms (7.1 ms with cushions) | — |
| Sunburst / icicle / circle packing, 1M-entry tree | 0.39 / 1.8 / 0.70 ms | — |
| Pick (hit test), 503k-rect layout | 0.12 µs; 0.29 µs with ancestor chain | < 1 ms |
| Pick, single folder of 1M files | 0.20 µs | < 1 ms |
| Drill-down transition matching, 39k rects | 7.4 ms | — |
| **Live updates** (USN journal) | | |
| Journal record to UI change set | 251–265 ms | ≤ 1 s |
| Throughput into a 1M-entry index | ~316k journal records/s | — |
| 500k files created at once | drained in 1.9 s; slowest tick 66 ms; longest index lock 24 ms | UI stays responsive |
| Wake-ups while idle (3 s) | 0 | ≈ 0% CPU |
| **Duplicates** | | |
| Size grouping, 1M candidates | 76–86 ms | — |
| Hashing, data in OS cache, 1 / 2 / 4 threads | 2.7 / 4.4 / 5.6 GB/s | — |
| Rerun served from the hash cache | 0 bytes read, < 1 ms | — |
| **Activity tracking** (ETW consumer) | | |
| Decode + aggregate | 0.71–0.93 M events/s (1.05–1.26 µs per event) | — |
| CPU at 10k events/s | ~1.1% of one core | ≤ 2% (sampling above) |
| **IPC** | | |
| Named pipe, 1M scan records in 8,192-record batches | 3.20 M records/s (279 MB/s) | — |
| Wire size per scan record (postcard) | 85.8 B | — |
| Encode / decode per record | 170 ns / 280 ns | — |
| **Classifier** | | |
| Classification per entry, single thread | 150 ns | — |
| 3.83M-entry tree, 16 threads | 0.18 s | — |
| Compile 240 built-in rules | ~4 ms | — |
| **History store** (50,000 directories per snapshot) | | |
| Commit, first snapshot | 147–215 ms | < 1 s |
| Commit, later snapshots | 53–65 ms median | < 1 s |
| Storage per snapshot | 12.0 B per directory | — |
| Diff of two snapshots | 14–35 ms | — |
| **UI** (503,464 instances, LOD off) | | |
| Frame rate during hover, zoom and pan sweeps | 100 fps, no dropped frames (harness ceiling) | 60 fps |
| Frame time p95 / p99 | 10.1 / 10.2 ms | — |
| Draw-call CPU per frame | 0.03 ms | — |
| Pick in the UI, including DOM measurement | 18–26 µs | < 1 ms |
| Frame ingest (skip table, grids, GPU upload), once per layout | 15 ms | — |
| Entry bundle | 121.7 kB gzip | ≤ 130 kB (enforced by the build) |
| **Installer** | | |
| x64 NSIS installer | ~3 MB | ≤ 15 MB |
