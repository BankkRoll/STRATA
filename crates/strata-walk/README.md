# strata-walk

The standard scanner: a parallel, unelevated directory walk that emits the same
`strata_core::ScanRecord`s as the MFT scanner. Strata uses it for NTFS without elevation, ReFS
and Dev Drive, FAT/exFAT, and network shares. It walks a system drive at 20–23 seconds per
million entries.

## Responsibilities

- Work-stealing traversal (`Walker`, `WalkOptions`), one rayon task per directory.
- Listing via `ListingMethod::DirectoryInfo` (default) or `ListingMethod::FindFirstFile`.
- `\\?\` paths throughout; names stay raw UTF-16. Reparse points are never followed.
- An allocation pass that opens each file attribute-only (never recalling cloud files) for
  exact allocation, compressed/WOF size, link count and alternate data streams.
- Hardlinks merged into one record per file id.
- Access-denied, vanished and replaced entries handled without failing the walk; incomplete
  directories are flagged `PARTIAL`.
- Network shares: bounded concurrency, per-request timeouts, cancellation (`CancelToken`).

Results arrive in batches through a sink (`FnSink` or your own).

## Test

```powershell
cargo test -p strata-walk
cargo run --release -p strata-walk --example walkbench -- <root>
```
