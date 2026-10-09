# strata-ntfs

Reads the NTFS Master File Table directly and emits one merged `strata_core::ScanRecord` per
file, with exact size accounting. Parsing is safe Rust that never panics on malformed input;
the only I/O is positioned reads through the `ReadAt` trait. In memory it parses a million MFT
records in under 300 ms.

## Responsibilities

- `BootSector`: volume geometry from sector 0.
- `apply_fixups`, `parse_record`, `AttrIter`, `decode_runlist`: record-level parsing.
- `assemble`: base + extension records into one `ScanRecord` (hardlinks, ADS, WOF, sparse,
  compression, reparse points).
- `NtfsVolume`: `$MFT` bootstrap, `read_record`, the parallel `scan` pipeline and `$Bitmap`
  reconciliation.
- `io`: `ReadAt` for memory, image files and raw volumes (`RawVolume`, `IoMode`), plus
  `QueuedReader`, an overlapped handle the scan uses to keep several reads in flight. Its
  `OVERLAPPED` plumbing (the `overlapped` module) is the crate's only `unsafe` code.
- `usn`: `USN_RECORD_V2/V3/V4` parsing (`parse_usn_buffer`).
- `test_image` (feature `test-image`): builds synthetic NTFS images for tests.

Opening a raw volume (`\\.\C:`) requires administrator rights; images and buffers don't.

## Test

```powershell
cargo test -p strata-ntfs
cargo bench -p strata-ntfs --bench parse
```

Unit and property tests run on synthetic images. For I/O benchmarks, write a large image and
scan it unbuffered:

```powershell
cargo run --release -p strata-ntfs --features test-image --example perf_image -- D:\perf\mft5m.img
cargo run --release -p strata-cli -- scan D:\perf\mft5m.img --no-buffering --top 0
```

Real-NTFS coverage is the VHDX suite in [tests/fixtures](../../tests/fixtures) (elevated
shell). Fuzz targets live in `fuzz/`, a separate workspace; run them on Linux or WSL.
