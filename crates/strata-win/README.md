# strata-win

The shared Win32 layer. Every Windows system query that more than one crate needs lives here,
so no other crate duplicates FFI. `unsafe` is confined to thin wrappers, each with a
`// SAFETY:` comment, and every kernel handle is owned by an RAII type (`OwnedHandle`).

## Responsibilities

- `volume`: volume discovery (`discover_volumes`): mount points, filesystem, sizes, BitLocker,
  Dev Drive, network drives, and which scanner to use (`scanner_choice`).
- `watcher`: volume hot-plug notifications.
- `known`: known folders for the current user and every other profile (`known_folders`),
  filling `strata_core::known::KnownFolders`.
- `process`: elevation checks, privileges, launching the helper through UAC
  (`launch_elevated`), process identity.
- `signature`: Authenticode verification and signer comparison (`verify_signature`,
  `same_signer`) for the helper/app mutual check.
- `path`: verbatim paths, final paths, NT device ↔ DOS paths, volume GUID paths.
- `last_access`, `shadow`, `sid`: last-access policy, shadow copy storage, SID lookup.

## Test

```powershell
cargo test -p strata-win
```

Tests run against the real machine but assert invariants only, so they pass unelevated and in
CI.
