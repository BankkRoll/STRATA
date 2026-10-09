# rules

The built-in rule packs. Each TOML file is a pack of rules that say what a file or folder is,
which app owns it, its category, and how safe it is to remove (`safe`, `probably`, `careful`,
`never`). They are embedded into the binary by
[`strata-classify`](../crates/strata-classify) at build time.

| Pack | Covers |
|---|---|
| `windows.toml` | Windows system files, caches, dumps and the never-delete defaults |
| `userdata.toml` | Known personal folders and content-type categories |
| `browsers.toml` | Chromium-family and Firefox profiles and caches |
| `dev.toml` | Package caches, dependency folders, build output, SDKs, Docker/WSL disks |
| `ai.toml` | Model stores and caches (Hugging Face, Ollama, LM Studio, ...) |
| `claude.toml` | Claude Code and Claude desktop |
| `apps.toml` | Chat, editor and media app caches, generic Electron caches |
| `games.toml` | Game launchers and libraries |
| `media.toml` | Video, streaming and photo editor caches |
| `downloads.toml` | Old installers, extracted archives, partial downloads |
| `generic.toml` | Temp files, logs, dumps, backups, empty and old/large files |

Users can add their own packs to override or extend these, but a user pack can never relax a
built-in `never` rule. The cleaner also keeps its own hard-coded never-delete list, independent
of any pack.

## Adding or changing a rule

1. Read [docs/RULES.md](../docs/RULES.md): schema, matchers, precedence, authoring checklist.
2. When unsure, choose the stricter tier.
3. Add a fixture case (matches and near misses) to
   `crates/strata-classify/tests/fixtures/rules.toml`.
4. `cargo test -p strata-classify`. It fails if any built-in rule has no fixture.
