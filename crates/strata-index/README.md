# strata-index

The in-memory index of one volume: struct-of-arrays storage with `u32` ids at under 64 bytes per
entry (names excluded). Pure Rust, no I/O besides its cache file, `#![forbid(unsafe_code)]`.

## Responsibilities

- `IndexBuilder`: builds from `ScanRecord`s in any order, resolves parents, hardlinks, reparse
  points, orphans, cycles and NTFS metadata, then aggregates sizes bottom-up in parallel.
- `Index`: the storage, addressed by `EntryId`.
- Live updates: `Index::upsert`, `Index::remove` and `Index::apply` return a `ChangeSet` after
  O(depth) aggregate maintenance.
- Queries (`query`): sorted child pages (`ChildQuery`, `SortKey`), paths, top-N, `Filter`s and
  `Breakdown`s.
- Search (`search`): `Query::parse` for the query language
  (`*.gguf size:>1gb node_modules\react`) and `Index::search`, a parallel streaming name search
  with cancellation. First results arrive in about a millisecond over 5 million names.
- Cache file: `Index::save` / `Index::load` with per-section checksums.

## Test

```powershell
cargo test -p strata-index
cargo bench -p strata-index --bench index
```

Live updates are property-tested: random change sequences applied live must equal a fresh
build.
