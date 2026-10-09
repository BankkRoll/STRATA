# Track: classifier (`strata-classify`, `rules/`)

Status: M6 classifier and attribution engine done; Apps view / category view / color modes are UI
work for the app track.

## What's done

- **Rule schema** (TOML, `schema_version = 1`), validation with unknown-key rejection, `@lists`,
  user packs with override/disable, and the never-tier policy. Reference: `docs/RULES.md`.
- **Engine** (`engine.rs`): path rules compiled into one component trie run as an NFA (`*`,
  globs, `**`), name/extension hash maps, scoped glob/regex sets, sibling/child "interest" bitsets
  computed once per directory. Precedence, inheritance (strong/weak/none) and the two never
  invariants (own never wins; only path rules carve out of an inherited never), plus clamping of
  user rules against built-in never.
- **Defaults:** unmatched `{WINDIR}` / Program Files → never; NTFS metadata → never (flag and
  volume-root names); everything else → `Classification::UNKNOWN` (no claim, careful).
- **240 built-in rules** in 11 packs (below), every one with a fixture test.
- **Explanations** (`explain.rs`): `Classifier::explain(path, &dyn FsProbe)` with candidate
  trace and decision; `StdFsProbe` for the real filesystem.
- **Installed-apps catalog** (`catalog/`): HKLM, WOW6432Node, HKCU and other loaded `HKU\<sid>`
  hives (safe `windows-registry` API, no `unsafe`), AppX via
  `PackageManager::FindPackagesByUserSecurityId("")`, folder heuristics with fuzzy matching
  (tokens, camelCase split, noise words, vendor aliases, edit distance), rule labels, ETW evidence
  (`add_evidence`), launcher games, confidence (exact/high/heuristic) + evidence list, orphan
  detection (first-level app-data folders, no match, idle ≥ 90 days).
- **Discovery** (`discover.rs`): Steam `libraryfolders.vdf` / `appmanifest_*.acf` (KeyValues
  parser), Epic `.item` manifests, Firefox `profiles.ini`, OBS `basic.ini`, Ollama manifests
  (blob → model), WSL `Lxss` registry, cache env vars → `DynamicRoots` tokens.
- **Content sniffing** (`sniff.rs`): all 26 requested types incl. VHD tail footer, ISO at 0x8001,
  GGUF/safetensors; `mismatch()` → "Detected: GGUF model (claims .bin)". Property-tested for
  panics.

## Public API (what the index / app calls)

```rust
// Startup (off the UI thread)
let kf: KnownFolders = /* strata-win */;
let found = strata_classify::discover::discover(&kf);              // DynamicRoots + games + WSL
let rules = RuleSet::load(&read_user_dir(user_rules_dir)?)?;       // report(): user-pack problems
let classifier = Classifier::new(&rules, &kf, &found.roots)?;      // Sync; share across threads

// Top-down walk (per volume, parallel over subtrees)
let root: DirScope = classifier.root(r"C:\", children_of_root);   // ChildRef { name, is_dir }
for each child of a directory with scope `s`:
    file: let cls = classifier.classify_file(&s, &Entry { name, flags, size, newest_mtime, magic });
    dir:  let s2 = classifier.enter_dir(&s, &entry, grandchildren); let cls = s2.classification();
// Store per entry: PackedClass::pack(&cls).0  (u32)

// Details
classifier.rule(cls.rule?)  -> &Rule (id, name, explain, app, action, tool, ...)
classifier.explain(path, &probe) -> Explanation   // settings UI "why?"
classifier.classify_path(path, &entry)           // one-off lookups (no sibling facts)

// Attribution
let (mut apps, warnings) = AppCatalog::read_system(&kf);
apps.extend(found.games.iter().map(InstalledApp::from_game));
let catalog = AppCatalog::new(apps, &kf);
catalog.attribute(path, rule_label) -> Option<Attribution { label, apps, confidence, evidence }>
catalog.orphan_check(folder, rule_app, idle_days) -> Option<Orphan>
catalog.add_evidence(prefix, app, weight)        // ETW track
sniff::sniff_with_tail(head, tail) / sniff::mismatch(name, detected)
```

Facts the index must pass: for directories, `size` = subtree total and `newest_mtime` = newest
modification in the subtree (SPEC §13); for files use the **logical** size (resident files have 0
allocated bytes and would look empty). Mark folders whose children are unknown with
`ACCESS_DENIED`/`PARTIAL` so they never look empty. `Classification::user` is the profile index
in `KnownFolders::users`.

## Rule counts per pack

| Pack | Rules | | Pack | Rules |
|---|---|---|---|---|
| windows | 37 | | dev | 54 |
| userdata | 15 | | games | 18 |
| browsers | 19 | | media | 10 |
| apps | 32 | | downloads | 6 |
| ai | 19 | | generic | 9 |
| claude | 21 | | **total** | **240** |

## Paths checked on the dev machine (read-only)

Verified present: `{TEMP}`, `{WINDIR}\Temp`, `{WINDIR}\SoftwareDistribution\Download`,
`{WINDIR}\Prefetch`, `{WINDIR}\Logs\CBS`, `{WINDIR}\Logs\DISM`, `{WINDIR}\WinSxS`,
`{WINDIR}\Installer`, `{WINDIR}\Minidump`, `{PROGRAMDATA}\Microsoft\Windows\WER`,
`{LOCALAPPDATA}\CrashDumps`, `{LOCALAPPDATA}\D3DSCache`, `{LOCALAPPDATA}\NVIDIA\{DXCache,GLCache}`,
`{APPDATA}\NVIDIA`, Explorer `thumbcache_*.db`/`iconcache_*.db`, `C:\$Recycle.Bin`; Chrome
`User Data` with `Default`, `Profile N`, `Guest Profile`, `System Profile` and per-profile
`Cache`, `Code Cache`, `GPUCache`, `Service Worker\CacheStorage`, root `ShaderCache`/`GrShaderCache`;
Edge `User Data\Default\Cache` and root caches; Firefox `profiles.ini` (relative `Profiles/…`) with
roaming profiles and local `cache2`/`startupCache`/`thumbnails`; Discord roaming caches +
`{LOCALAPPDATA}\Discord` (Squirrel `Update.exe`, `packages`); VS Code roaming caches, `User`,
`~\.vscode\extensions`; new Teams package; `{APPDATA}\Zoom`; Telegram `tdata`; Thunderbird folders;
`{LOCALAPPDATA}\pip\cache`, both `npm-cache`s, `pnpm\store`, `pnpm-cache`, `{APPDATA}\npm\node_modules`,
`~\.cargo\{registry,git}`, `~\.rustup\toolchains`, `~\go\pkg\mod`, `{LOCALAPPDATA}\go-build`,
`~\.gradle\caches`, `~\.nuget\packages`, `{LOCALAPPDATA}\NuGet\v3-cache`,
`{LOCALAPPDATA}\Android\Sdk\system-images`, `{PROGRAMDATA}\Microsoft\VisualStudio\Packages`,
`{LOCALAPPDATA}\ms-playwright`, `{LOCALAPPDATA}\nvm`, `{LOCALAPPDATA}\*-nodejs`,
`{LOCALAPPDATA}\Docker\wsl\disk\docker_data.vhdx`, `{LOCALAPPDATA}\Docker\wsl\main\ext4.vhdx`
(registered in `Lxss` as `docker-desktop`), a Canonical Ubuntu Store package; Steam in
`{PROGRAMFILES_X86}\Steam` (`steamapps\{common,downloading,shadercache,temp,workshop}`,
`libraryfolders.vdf`, `appmanifest_*.acf`), `{LOCALAPPDATA}\Steam\htmlcache`, `{SYSTEMDRIVE}\XboxGames`,
`{LOCALAPPDATA}\Roblox`, `{APPDATA}\obs-studio` (recordings path from `basic.ini`), `{DOWNLOADS}`;
Documents redirected into OneDrive (covered by the redirected-profile fixtures).

**Claude (verified):** Claude Code native install `~\.local\bin\claude.exe`,
`~\.local\share\claude\versions`, `~\.local\state\claude`; `~\.claude\{projects, file-history,
sessions, session-env, cache, paste-cache, shell-snapshots, debug, downloads, backups, plugins
(cache, marketplaces, data), skills, ide, state}`, `settings.json`, `CLAUDE.md`,
`.credentials.json`, `history.jsonl`, `stats-cache.json`, `settings.json.bak-*`; `~\.claude.json`;
`{LOCALAPPDATA}\claude-cli-nodejs\Cache` (per-project MCP logs);
`~\.vscode\extensions\anthropic.claude-code-*`; `{APPDATA}\npm\node_modules\@anthropic-ai` (empty);
`~\.copilot`, `{APPDATA}\TabNine`. Largest sub-purpose on this machine: session transcripts
(`projects`), then skills and file history.

**Unverified on dev machine** (app not installed or access denied unelevated; rules kept, layout
from vendor documentation): Claude desktop (`{LOCALAPPDATA}\AnthropicClaude`, `{APPDATA}\Claude`,
MSIX `Packages\Claude_*`); Brave, Opera/Opera GX, Vivaldi, Arc, Chromium; Slack, classic Teams,
Cursor, Spotify, Notion, Figma, Obsidian; Hugging Face, Ollama, LM Studio, ComfyUI, A1111/Forge,
torch hub, Whisper, Keras, GPT4All, `{LOCALAPPDATA}\github-copilot`; Yarn, Bun, conda, Poetry, uv,
Maven, Android AVDs, vcpkg, Unity/Unreal projects, Unreal shared DDC, JetBrains; Delivery
Optimization cache and font cache (exist but access denied unelevated), `MEMORY.DMP`,
`LiveKernelReports`, `{SYSTEMDRIVE}\{NVIDIA,AMD}`, `Windows.old`/`$WINDOWS.~BT`/`~WS`/`$GetCurrent`,
AMD/Intel shader caches; Epic, Battle.net, EA app, Ubisoft Connect, GOG; Adobe media/disk cache,
DaVinci Resolve, CapCut, Lightroom.

## Catalog on the dev machine (read-only, `examples/machine_report.rs`)

459 records: 162 HKLM, 121 WOW6432Node, 10 HKCU, 164 AppX, 2 Steam games (registry + AppX read in
~0.5 s warm, ~2.5 s cold). Sample attributions (paths tokenized):

| Path | Result |
|---|---|
| `{LOCALAPPDATA}\Discord\app-…` | Discord, exact (InstallLocation) |
| `{LOCALAPPDATA}\Programs\Microsoft VS Code\Code.exe` | VS Code (User), exact |
| `{LOCALAPPDATA}\Packages\MSTeams_8wekyb3d8bbwe\LocalCache` | Microsoft Teams, exact (AppX data dir) |
| `{LOCALAPPDATA}\Google\Chrome\User Data\Default` | Google Chrome, high (rule label) |
| `~\.claude\projects` | Claude Code, high (rule label) |
| `{LOCALAPPDATA}\Docker\wsl` | Docker Desktop, high (folder = publisher) |
| `{LOCALAPPDATA}\NVIDIA\DXCache` | NVIDIA Corporation (vendor), high |
| `{PROGRAMFILES_X86}\Steam\steamapps\common\<game>` | the game when Steam has a manifest, else Steam (high, binary location) |

Orphan candidates: 119 first-level folders under `{LOCALAPPDATA}`, `{APPDATA}`, `{PROGRAMDATA}`
idle ≥ 90 days with no matching app (e.g. data of uninstalled browsers and games). They are a
`careful` hint, not a cleanup suggestion.

## Tests

- 45 unit tests, 19 doctests, 4 integration tests.
- `tests/rule_fixtures.rs` + `tests/fixtures/rules.toml`: 240 cases, **1,480 assertions**, walking a
  synthetic tree through the real API with two profiles (standard `alice`; `bob` with Documents on
  `D:\Docs`, Downloads on `D:\Téléchargements`, Temp on `D:\Temp\bob`, Desktop/Pictures in OneDrive,
  localized Music/Videos). Per-user paths are checked for both profiles, including the attributed
  profile index. Every rule needs a fixture (`every_builtin_rule_has_a_fixture`); auto near-miss
  `<path>_nearmiss` plus explicit near misses (`dist` without `package.json`, etc.).
- `no_builtin_never_rule_can_be_relaxed`: overrides and disables of all built-in never rules are
  refused.
- `cargo fmt`, `cargo clippy -p strata-classify --all-targets -D warnings`: clean.

## Performance (`examples/bench_classify.rs`, release, best of 5)

Synthetic 3.83M-entry tree (Windows tree, 240 projects with node_modules, caches, Chrome profiles,
Steam library, user data); 240 rules compile in ~4 ms.

| | Time | Per entry |
|---|---|---|
| Single-threaded | 0.57 s | **150 ns** (≈ 0.75 s per 5M) |
| Parallel, 16 threads (machine ~70 % busy with other work) | 0.18 s | |

The main win was keeping globset off the hot path (its `Candidate` allocates per call):
`prefix*suffix` name globs use string checks, `*` names skip matching, and `under`-scoped
glob/regex rules run only inside their roots.

## Decisions

- **Rule engine is pure**: takes `KnownFolders` + `DynamicRoots`; only `catalog`/`discover` touch
  the registry, AppX and the filesystem.
- **Precedence by class, then depth implicitly**: an own match replaces the inherited one when its
  class ≥ the inherited class. Simpler to reason about than comparing raw depths and gives
  "longest path" for free.
- **Only path rules carve exceptions out of never**; own never always wins. Flag rules can't lower
  a tier.
- **User rules are clamped against built-in never** by evaluating built-ins alone in parallel
  (shadow inheritance), so a new user rule under `{WINDIR}` cannot relax it either.
- **`weak` inheritance** for personal folders so Downloads/Documents content rules still apply.
- **Games are `never` + `launcher_uninstall`**: deleting game folders desyncs launchers.
- **Claude Code installed versions are `careful`**, not "old versions = probably": the launcher may
  run an older file when updates lag; safety first.
- **Orphans need 90 idle days**; without it every recently used CLI cache was flagged.
- **`windows-registry` crate** instead of raw Win32 registry calls: no `unsafe` in the crate.
- **`PackedClass` (u32)** for index storage; origin/profile recomputed by `explain`.

## Blockers

None.

## Core change requests

Two folders are currently addressed through `{USERPROFILE}` with their default on-disk names, which
breaks if they are relocated. Please add them to `KnownFolder` (resolved by `strata-win` via
`FOLDERID_LocalAppDataLow` and `FOLDERID_SavedGames`):

```diff
--- a/crates/strata-core/src/known.rs
+++ b/crates/strata-core/src/known.rs
@@ pub enum KnownFolder {
     /// `FOLDERID_LocalAppData` (per user).
     LocalAppData,
+    /// `FOLDERID_LocalAppDataLow` (per user).
+    LocalAppDataLow,
+    /// `FOLDERID_SavedGames` (per user).
+    SavedGames,
@@ impl KnownFolder {
-    pub const ALL: [Self; 16] = [
+    pub const ALL: [Self; 18] = [
         Self::UserProfile,
         Self::LocalAppData,
+        Self::LocalAppDataLow,
+        Self::SavedGames,
@@ pub const fn is_per_user(self) -> bool {
             Self::UserProfile
                 | Self::LocalAppData
+                | Self::LocalAppDataLow
+                | Self::SavedGames
@@ pub const fn token(self) -> &'static str {
             Self::LocalAppData => "{LOCALAPPDATA}",
+            Self::LocalAppDataLow => "{LOCALAPPDATALOW}",
+            Self::SavedGames => "{SAVEDGAMES}",
```

After it lands: `windows.gpu_shader_caches` uses `{LOCALAPPDATALOW}\Intel\ShaderCache` and
`userdata.saved_games` uses `{SAVEDGAMES}`.

## Next steps

- App track: Apps view (footprint = install + data + caches by location, registry estimate vs
  measured), category view, color modes, "why" panel from `Explanation`.
- Content spot-check for `downloads.extracted_archive` (list archive entries vs the folder) when the
  detail panel opens; installed-version check for `downloads.old_installer`.
- Feed Ollama blob → model map and Steam per-game sizes into the detail panel.
- Index track: call `Classifier` during the post-scan aggregate pass and re-classify the parents of
  changed entries on USN updates (sibling facts change).
