# Strata

Native Windows disk-space intelligence app (Tauri v2 + Rust + React).

**The spec is [docs/SPEC.md](docs/SPEC.md). It is the product definition and the standing
instructions. Read it before working.** Resume from [docs/PROGRESS.md](docs/PROGRESS.md).
Record decisions in [docs/DECISIONS.md](docs/DECISIONS.md) and blockers in
[docs/BLOCKERS.md](docs/BLOCKERS.md).

## Layout

- `crates/strata-core` — shared types (scan records, flags, times, tiers). Every crate depends on it.
- `crates/strata-*` — engine crates (see SPEC §3).
- `src-tauri/` — unelevated Tauri app.
- `ui/` — React + TypeScript frontend (pnpm workspace member).

## Commands

Run cargo from **PowerShell**, not Git Bash. Never run `cargo test --workspace` unqualified:
`tauri-build` writes a stub `msvcrt.lib` into its OUT_DIR, and cargo leaks that link path into
every workspace doctest, which then fails to link. Test `strata-app` separately (below).
Under Git Bash, doctests can also fail to link because the
VS2019 BuildTools `link.exe` on PATH lacks its CRT environment.

If your system drive is short on space, point `CARGO_TARGET_DIR` at another drive.

```powershell
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --exclude strata-app; cargo test -p strata-app
pnpm --dir ui typecheck; pnpm --dir ui lint; pnpm --dir ui test
pnpm tauri dev          # run the app
```

`tauri::generate_context!` needs `ui/dist` to exist; run `pnpm --dir ui build` once (or create
the folder) before a clean `cargo clippy`.

## Conventions

- No `todo!()`, `unimplemented!()`, stubs, or mock data in completed milestones.
- Parsers never panic on malformed input; bound-check everything.
- `unsafe` only in thin Windows I/O layers, each block with a `// SAFETY:` comment.
- JSDoc / rustdoc on every export. Comments explain *why*, never *what*.
- **Public repo: no machine-specific data.** Never commit real profile paths, account names,
  SIDs, volume GUIDs/serials or local checkout paths. Use placeholders (`C:\Users\me`,
  `S-1-5-21-1-2-3-1001`). `node scripts/check-privacy.mjs` enforces this in CI.
