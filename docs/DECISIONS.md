# Decisions

Format: date — decision — why.

- 2026-10-09 — **Tauri 2.12 (stable), not 3.0.** 3.0 is alpha. The spec pins v2.
- 2026-10-09 — **TypeScript ~6.0**, not 7.0. typescript-eslint 8.71 supports `<6.1`.
- 2026-10-09 — **Added `strata-core` crate** (not in the spec layout). The scanners, index,
  classifier, cleaner and IPC all share the scan-record and flag types. Putting them in any one
  of those crates would force unrelated dependencies (e.g. the walker depending on the NTFS
  parser).
- 2026-10-09 — **One `ScanRecord` type for both scanners.** The index never knows which scanner
  ran. Walker-specific gaps are flags (`ALLOC_ESTIMATED`, `ACCESS_DENIED`, `PARTIAL`).
- 2026-10-09 — **Names stored as raw UTF-16 (`WideName`)**, never case-folded. Lossless for
  unpaired surrogates; matches NTFS on-disk form with zero conversion cost in the scanner.
- 2026-10-09 — **`EntryFlags` packs reparse kind (4 bits) and cloud state (2 bits)** into the
  per-entry `u32`, keeping index memory within the 64-byte-per-entry budget.
- 2026-10-09 — **`WofCompressedData` is excluded from ADS totals**; its allocation is reported
  as the file's `allocated` so it is counted exactly once.
- 2026-10-09 — **Native window decorations for now.** A custom title bar that preserves Windows
  11 snap layouts needs `WM_NCHITTEST` handling for the maximize button; scheduled for M4/M15.
- 2026-10-09 — **Main window starts hidden** and is shown after the backdrop is applied, so a
  transparent frame is never visible.
- 2026-10-09 — **Release profile keeps unwinding** (no `panic = "abort"`) so a panic in a Tauri
  command cannot take down the UI process.
