# strata-clean

Safe deletion. A wrong delete is unacceptable, so every layer assumes the layer above it made a
mistake: the classifier's safety tiers are a hint, and this crate re-checks everything itself.

## Responsibilities

- `never` / `canon` / `guard`: the hard-coded never-delete list (`NeverList`), independent of
  rule packs, applied by `SafetyGuard` to the literal path, its 8.3 expansion and its
  handle-resolved form (junctions, symlinks, mount points, volume GUID paths).
- `preflight`: re-verifies each item right before deleting it (TOCTOU), checks locks and
  Recycle Bin capacity.
- `recycle`: Recycle Bin deletes and restore via `IFileOperation`, the default.
- `permanent`: handle-based permanent delete that never follows links; needs extra confirmation.
- `locks`: Restart Manager lock detection ("which process has this open?") and polite close.
- `apps`: running-app awareness ("close Chrome first").
- `privileged`: the delete requests the elevated helper accepts (by file id, re-validated).
- `tools`: launchers for built-in Windows tools and app uninstallers.
- `audit`: the write-ahead audit-log contract, persisted by `strata-store`.
- `flow`: `plan`, `preflight` and `execute`, the API the app calls.

`unsafe` is confined to the internal `win` module.

## Test

```powershell
cargo test -p strata-clean
```

Tests delete only inside temp folders they create. A proptest suite throws path spellings at
the never-list.

## See also

- The never-list must never be weakened: [CONTRIBUTING.md](../../CONTRIBUTING.md)
