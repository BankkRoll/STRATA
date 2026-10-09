# strata-store

SQLite persistence (bundled `rusqlite`). A store is a directory with two databases, split so a
corrupt history never costs settings or the undo log:

| File | Contents |
|---|---|
| `history.db` | snapshots, ETW activity, duplicate hash cache (derived, rebuildable) |
| `state.db` | settings, undo/audit log |

## Responsibilities

- Snapshots of directory aggregates, diffs and retention thinning (`Store::begin_snapshot`,
  `Store::diff`, `Store::apply_retention`). A 50k-directory snapshot commits in well under a
  second.
- Typed, versioned `Settings` with export/import.
- The write-ahead undo/audit log for deletes (`Store::begin_action`), which Recycle Bin restore
  depends on.
- ETW hourly activity rollups and the duplicate finder's hash cache.

Entry point: `Store::open(dir)`. It returns a cheap `Clone + Send + Sync` handle; calls block,
so make them off the UI thread. Nothing panics: a damaged or too-new database is reported by
`Store::health` and per-call errors while the other database keeps working.

## Test

```powershell
cargo test -p strata-store
```

Includes corruption, concurrency and timing tests.
