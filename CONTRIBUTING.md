# Contributing

Strata is provided as-is and not actively maintained. Pull requests are welcome, but they may
not be reviewed quickly, or at all. Forking is encouraged.

## Build and test

Run cargo from PowerShell.

```powershell
pnpm install
pnpm --dir ui build      # once: tauri::generate_context! needs ui/dist
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --exclude strata-app; cargo test -p strata-app
pnpm --dir ui typecheck; pnpm --dir ui lint; pnpm --dir ui test
```

CI runs all of these. `strata-app` is tested separately because `tauri-build` leaks a stub
`msvcrt.lib` into every workspace doctest link. If your system drive is short on space, point
`CARGO_TARGET_DIR` at another drive.

## Conventions

- No stubs, `todo!()`, `unimplemented!()` or mock data in merged work.
- Parsers never panic on malformed input; bound-check everything.
- `unsafe` only in thin Windows I/O layers, each block with a `// SAFETY:` comment.
- Rustdoc / JSDoc on every export. Other comments explain *why* (a workaround, an edge case, a
  platform quirk), never *what* the next line does.
- [Conventional commits](https://www.conventionalcommits.org/): `feat:`, `fix:`, `docs:`,
  `chore:`, `refactor:`, `test:`, `perf:`, `ci:`. Imperative subject; the body says why.
- Rule-pack changes follow [docs/RULES.md](docs/RULES.md), including a fixture case for every
  rule.

## Privacy

Don't commit machine-specific paths or identifiers (profile paths, account names, SIDs, volume
GUIDs, serials, computer names). Use placeholders such as `C:\Users\me` and
`S-1-5-21-1-2-3-1001`.

## Deletion safety

Never weaken the never-delete list in `crates/strata-clean`, the `never` rules in `rules/`, or
the pre-flight checks. PRs that relax them will be closed. Adding protection is always welcome.

Security issues go through [SECURITY.md](SECURITY.md), not public issues or PRs.
