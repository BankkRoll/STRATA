# strata-core

Shared vocabulary for every Strata crate. No Windows dependencies and no I/O, so it builds and
tests on any platform.

## Responsibilities

- `ScanRecord` (with `Sizes`, `Times`, `NameLink`, `AdsInfo`, `Reparse`): the one record type
  both scanners emit, so the index never knows which scanner ran.
- `FileRef`: a file's stable identity on a volume (record number + sequence, or synthetic).
- `WideName`: lossless UTF-16 names; unpaired surrogates are preserved, never case-folded.
- `FileTime` and the compact `EpochSecs` the index stores.
- `EntryFlags`: the packed per-entry flag word, including `ReparseKind` and `CloudState`.
- `Safety`, `Category`, `SizeMode`: product-level enums shared by the classifier, cleaner and UI.
- `known`: resolved known folders as plain data (resolution lives in `strata-win`).
- `win32`: attribute bits and reparse tags.

## Test

```powershell
cargo test -p strata-core
```
