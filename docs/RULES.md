# Rule packs

Strata classifies every file and folder with **rule packs**: TOML files that say what data is,
which app owns it and how safe it is to remove. Built-in packs live in `rules/` and are embedded
in the binary. Users can add packs to their rules folder to override or extend them.

This document is the schema reference and the authoring guide. The engine is
`crates/strata-classify`.

## Pack file

```toml
schema_version = 1          # required; this build understands 1
pack = "dev"                # pack id; built-in rule ids must start with "dev."
version = 3                 # pack revision (informational)
description = "..."

[lists]                     # named string lists, usable in any list matcher as "@name"
js_build_dirs = [".next", ".nuxt", ".svelte-kit"]

disable = ["generic.zero_byte_file"]   # user packs only: turn built-in rules off

[[rule]]
# ...
```

Unknown keys are errors everywhere (a typo such as `match.dir_nam` is rejected, not ignored).
List names are global across all loaded packs and cannot be redefined.

## Rule

```toml
[[rule]]
id = "dev.node_modules"                  # dotted, lowercase, unique
name = "Node.js dependencies"            # short display name
category = "dev_build"                   # top-level category (below)
subcategory = "dev.dependencies"         # optional free-form sub-category
safety = "safe"                          # safe | probably | careful | never
regenerable = true                       # recreated automatically if deleted (default false)
explain = "Packages installed for a project. Run `npm install` to restore them."
action = "delete"                        # delete | open_tool | info_only
tool = "disk_cleanup"                    # required with open_tool; optional guidance with info_only
app = "Node.js / npm"                    # attribution label (optional)
inherit = "strong"                       # strong | weak | none (default strong)
match.dir_name = "node_modules"
match.requires_sibling = ["package.json"]
```

**Categories** (`strata_core::Category`, snake_case): `system`, `apps`, `games`, `ai_models`,
`dev_build`, `caches`, `temp`, `downloads`, `documents`, `media`, `archives`, `cloud`,
`recycle_bin`, `ntfs_metadata`, `unknown`.

**Actions.** `action` defaults to `delete`, or `info_only` for `never` rules. A `never` rule can
never offer `delete` (validation error).

**Tools:** `disk_cleanup`, `dism_component_cleanup`, `empty_recycle_bin`, `app_uninstaller`,
`launcher_uninstall`, `system_protection`, `hibernation_guidance`, `docker_prune`, `wsl_compact`.

**Inheritance.** A classification flows to every descendant:

| `inherit` | Descendants |
|---|---|
| `strong` (default) | inherit it; only path-class rules, or `never` rules, below can replace it |
| `weak` | inherit it as a fallback; any rule match below replaces it (used for `{DOWNLOADS}`, `{DOCUMENTS}`) |
| `none` | do not inherit it; they see what this entry inherited |

`never` rules must be `strong`.

## Matchers

All matchers present in a rule must hold. One of them is the **primary** matcher, which picks the
index the rule lives in and its precedence class.

| Matcher | Kind | Notes |
|---|---|---|
| `path` | primary | Exact path(s), case-insensitive. No wildcards. |
| `path_glob` | primary | Path globs, matched component by component: `*`, `?`, `[a-z]`, `{a,b}` within a component; `**` = zero or more components (also allowed as the first component: "anywhere"). A trailing `**` is rejected (descendants inherit anyway). |
| `path_regex` | primary | Case-insensitive regex over the full normalized path (uppercase, `\` separators, e.g. `C:\USERS\ME\...`). Requires a literal `under` root; it is only evaluated inside it. |
| `flag` | primary | `ntfs_metadata`, `cloud` (any placeholder), `cloud_online_only`. |
| `name` / `dir_name` / `file_name` | primary | Entry names (globs allowed). `dir_name` implies `applies_to = "dir"`, `file_name` implies `"file"`. |
| `name_regex` | primary | Case-insensitive regex over the name. |
| `ext` | primary | Extensions without the dot (`"log"`, `["exe", "msi"]`). |
| `magic` | primary | Sniffed content type (below). Only matches when the caller sniffed the file. |
| `any = true` | primary | Every entry; needs a size, age, `empty_dir` or `under` constraint. |
| `requires_sibling` | filter | Any of these names exists beside the entry. Globs allowed (`"*.csproj"`). Placeholders: `{stem}` (a folder named like the entry without its extension), `{original}` (a file named like the entry without its ` (N)` suffix). |
| `requires_child` | filter | Any of these names exists inside the (directory) entry. |
| `min_size` / `max_size` | filter | Bytes or text: `"10 MB"` (1000-based), `"1 GiB"` (1024-based). Directory sizes are subtree totals. |
| `older_than_days` | filter | The newest modification in the entry's subtree is older than N days. |
| `applies_to` | filter | `file`, `dir` or `both`. |
| `under` | filter | Entry is strictly inside one of these roots (path or glob). |
| `not_under` | filter | Entry is not inside any of these roots. |
| `volume_root` | filter | Entry sits directly in a volume root (`C:\pagefile.sys`). |
| `empty_dir` | filter | Directory with no children (never true for unreadable or partially scanned folders). |

A primary matcher may also be combined with other matchers as filters, e.g. a `path_glob` with a
`dir_name` list.

**Magic types:** `zip`, `seven_zip` (`7z`), `rar4`, `rar5` (`rar`), `pe`, `pdf`, `mp4`, `mov`,
`heic`, `mkv`, `webm`, `avi`, `wav`, `mp3`, `flac`, `ogg`, `png`, `jpeg`, `gif`, `webp`, `sqlite`,
`safetensors`, `gguf`, `vhd`, `vhdx`, `iso`.

## Path tokens

Paths start with a token, a drive (`D:\Data`), a UNC root (`\\nas\share`) or `**`.

| Token | Resolves to |
|---|---|
| `{USERPROFILE}`, `{LOCALAPPDATA}`, `{APPDATA}`, `{TEMP}`, `{DOWNLOADS}`, `{DOCUMENTS}`, `{DESKTOP}`, `{PICTURES}`, `{MUSIC}`, `{VIDEOS}` | per user: **every** resolved profile (all users when elevated) |
| `{PROGRAMDATA}`, `{WINDIR}`, `{PROGRAMFILES}`, `{PROGRAMFILES_X86}`, `{USERPROFILES}`, `{PUBLIC}` | machine |
| `{SYSTEMDRIVE}` | the drive holding `{WINDIR}` |
| `{STEAM_LIBRARY}`, `{EPIC_GAME}`, `{FIREFOX_PROFILE}`, `{OBS_RECORDINGS}`, `{WSL_DISTRO}`, `{HF_HOME}`, `{OLLAMA_MODELS}`, `{CARGO_HOME}`, `{RUSTUP_HOME}`, `{GOMODCACHE}` | discovered at runtime from app configuration (`strata_classify::discover`) |

Known folders come from `SHGetKnownFolderPath`, so localized and redirected folders (Documents on
`D:\`, OneDrive folder backup) work without English names in rules. A token with no resolution
(app not installed) simply makes that pattern match nothing.

## Matching semantics

- **Case-insensitive, Windows-correct.** Names fold with ordinal simple uppercase per UTF-16 unit
  (like NTFS `$UpCase`, never locale rules). `/` and `\` are equivalent, repeated separators
  collapse, and `\\?\`, `\\?\UNC\`, `\\.\` prefixes are stripped.
- **Extensions** are the text after the last dot; a leading dot (`.gitignore`) is not one.

## Precedence

Rules are grouped in precedence classes by primary matcher:

| Class | Matchers |
|---|---|
| 3 (path) | `path` > `path_glob` > `path_regex`, and `flag` |
| 2 (name) | `name`, `dir_name`, `file_name`, `name_regex` |
| 1 (extension) | `ext`, `magic` |
| 0 (any) | `any` |
| -1 | default (no rule), and anything inherited weakly |

**At one entry**, the best match maximizes, in order: class, kind (path > glob > regex), number
of literal path components ("longest path"), number of extra constraints, then the **stricter
safety tier**, then user rules over built-ins.

**Against the inherited classification**, the entry's own best match wins when its class is at
least the inherited class. So a deeper path rule beats an inherited path rule, a name rule beats
an inherited name rule, but an extension rule never overrides a folder matched by path. Two
safety invariants apply on top:

1. An own `never` match always wins over a less strict inherited tier.
2. An inherited `never` can only be replaced by a less strict tier through a **path-class** rule
   (an explicit carve-out). Name, extension, flag and `any` rules cannot weaken it.

Examples:

| Entry | Matches | Result |
|---|---|---|
| `{LOCALAPPDATA}\npm-cache` | `dev.npm_cache` (path) | safe |
| `C:\Windows\Temp\x.tmp` | own `generic.tmp_files` (ext, probably) vs inherited `windows.windows_temp` (path) | inherited wins: safe temp |
| `C:\Windows\Temp` | own `windows.windows_temp` (path) vs inherited `windows.windir` (never) | path carve-out: safe |
| `C:\Windows\System32\a.tmp` | own `generic.tmp_files` (ext) vs inherited never | stays never (invariant 2) |
| `{TEMP}\repo\.git` | own `dev.git` (never, name) vs inherited safe temp | never (invariant 1) |
| `{DOWNLOADS}\setup.exe`, 60 days old | `downloads.old_installer` (ext) vs weak `userdata.downloads` | own match: probably |
| `Chrome\User Data\Default\Cache` | `browsers.chrome.cache` (glob, 6 literals) and `apps.electron_cache` (glob `**\*`, 0 literals) | Chrome rule: more literal components |
| `D:\proj\node_modules`, idle 400 days | `dev.node_modules` and `dev.node_modules_stale` (one more constraint) | stale variant |

**Defaults.** Everything under `{WINDIR}`, `{PROGRAMFILES}` and `{PROGRAMFILES_X86}` that no rule
carves out is `never` (rules `windows.windir`, `windows.program_files`). NTFS metadata is `never`.
Anything no rule covers is `Unknown` with **no safety claim**, which the cleaner treats as
`careful`.

## Overrides and safety policy

User rules are loaded from every `*.toml` file in the user rules folder, in file-name order.

- A user rule with the id of a built-in rule **replaces** it.
- `disable = ["id", ...]` turns built-in rules off.
- A built-in `never` rule can be **neither relaxed nor disabled**: an override must keep
  `safety = "never"` (it may reword the explanation), and disabling it is refused. The never tier
  is what keeps Windows, installed programs, browser profiles and git history out of every delete
  path; a rules file must not be able to remove that protection.
- New user rules cannot relax a built-in `never` either: when user rules are loaded the engine
  also evaluates built-ins alone, and if they say `never` the result is clamped to `never`
  (`Classification::clamped`). User rules may freely relax or tighten the other tiers.
- A broken user file never blocks the built-ins; problems are listed in `RuleSet::report()`.

## Testing a rule: "why is this classified as X?"

`Classifier::explain(path, probe)` walks the path from its volume root and returns an
`Explanation`: the winning rule (id, pack, source, explanation, app), the ancestor it matched at,
every step on the way, every candidate rule for the final entry with the matcher that failed,
and the decision taken. `Explanation::to_text()` renders it:

```
C:\Windows\System32\a.tmp
  Category: System   Safety: Never
  Rule: windows.windir (Windows system files) from built-in pack `windows`
  inherited from C:\Windows by path
  Why: Part of Windows. ...
  Candidates:
    - generic.tmp_files [ext, Probably] matched
  Decision: the location is `never`; only explicit path rules can carve exceptions
```

The settings UI passes a probe backed by the index; `StdFsProbe` reads the real filesystem.

## Authoring checklist

1. Verify the path on a real machine. Prefer tokens over literal paths.
2. Pick the least specific matcher that is still unambiguous, and add `requires_sibling` /
   `requires_child` for generic names (`dist`, `build`, `target`, `bin`).
3. Data folders are `never` or `careful`; carve caches out with deeper `path` / `path_glob` rules.
4. When unsure, choose the stricter tier.
5. Add a fixture case to `crates/strata-classify/tests/fixtures/rules.toml` with positive paths
   and near misses. `cargo test -p strata-classify` fails if any built-in rule has no fixture.
