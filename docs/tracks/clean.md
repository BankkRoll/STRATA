# Track: cleanup (`strata-clean`)

Owner scope: `crates/strata-clean/`, this file. Status: the safety core for M7 is
done and tested on the dev machine (Windows 11 26200, unelevated). The app/UI
wiring, the store adapter and the helper pipe come next.

## What's done (by SPEC section)

| SPEC | Done | Where |
|---|---|---|
| §15.1 never-list (hard-coded, independent of rules) | Yes | `never.rs`, `canon.rs`, `guard.rs` |
| §15.2 step 4 permanent delete by handle | Yes | `permanent.rs` |
| §15.2 step 5 pre-flight (TOCTOU, locks, bin fit) | Yes | `preflight.rs`, `expect.rs` |
| §15.2 steps 1-6 plan / review / execute / retry API | Yes | `flow.rs` |
| §15.2 step 7 undo log (write-ahead contract) + Restore | Yes | `audit.rs`, `recycle.rs` |
| §15.3 Restart Manager lock detection, polite close | Yes | `locks.rs`, `consent.rs` |
| §15.3 delete on next reboot (helper only) | Yes, validation tested | `privileged.rs` |
| §15.4 `IFileOperation` Recycle Bin, long paths, too-large detection | Yes | `recycle.rs`, `win/shell.rs`, `volume.rs` |
| §15.5 running-app awareness | Yes | `apps.rs` |
| §15.6 built-in tools | Yes | `tools.rs` |
| §15.7 privileged by-id delete request | Yes | `privileged.rs` |
| §21 Recycle Bin unavailable / item too large / locked between pre-flight and action / app crash mid-delete | Yes | see tests below |
| §21 removable drive yanked mid-delete | Typed I/O failure per item (`CleanError::Os`/`NotFound`, retryable); not exercised on hardware | `permanent.rs`, `flow.rs` |
| §22 safety tests and race tests | Yes | `tests/` |

## The never-list

Compiled by `NeverList::new` from a resolved `KnownFolders` (from `strata-win`),
the volume map, the machine's names and Strata's install folder(s). `{WINDIR}`,
`{PROGRAMFILES}` and `{USERPROFILES}` are required; without them construction
fails rather than running with a partial list. `NeverList::entries()` returns
this table at runtime.

A request is refused when it **is**, **contains** (is an ancestor of), or, for
subtree rules, **is inside** a protected location.

| Location | Scope | Why |
|---|---|---|
| Any volume or share root (`C:\`, `\\?\Volume{..}\`, `\\srv\share`) | item | Deleting a root wipes the volume |
| Every folder mount point of another volume (`C:\mnt\data`) | item | It is another volume's root |
| `<volume>\System Volume Information` | subtree | Restore points, shadow copies, indexer |
| `<volume>\$Recycle.Bin`, `RECYCLER`, `RECYCLED` | subtree | Empty via `SHEmptyRecycleBinW`; deleting `$R` without `$I` corrupts the bin |
| `<volume>\$MFT $MFTMirr $LogFile $Volume $AttrDef $Bitmap $Boot $BadClus $Secure $UpCase $Extend` | subtree | NTFS metadata |
| `<volume>\Boot`, `EFI`, `Recovery` | subtree | Needed to start Windows / WinRE |
| `<volume>\pagefile.sys`, `hiberfil.sys`, `swapfile.sys` | item | Paging / hibernation; info + `powercfg` guidance only |
| `<volume>\bootmgr`, `BOOTNXT`, `BOOTSECT.BAK`, `boot.ini`, `ntldr` | item | Boot files |
| `<volume>\Documents and Settings` | item | Compatibility junction to the profiles root |
| `<volume>\Program Files*` (any top-level name starting so) | item | Covers `(x86)`, `(Arm)` and installs on other drives |
| `{WINDIR}` | subtree, **except** contents of `Temp`, `SoftwareDistribution\Download`, `SoftwareDistribution\DeliveryOptimization`, `ServiceProfiles\NetworkService\AppData\Local\Microsoft\Windows\DeliveryOptimization\Cache`, `Minidump`, `LiveKernelReports`, `Logs\CBS`, `Logs\DISM`, `Prefetch`, `ServiceProfiles\LocalService\AppData\Local\FontCache`, and the file `MEMORY.DMP` | The OS; the exceptions are the temp/cache rules of §12.2 (the folders themselves stay protected) |
| `{PROGRAMFILES}`, `{PROGRAMFILES_X86}` and their `Common Files` | item | Spec: roots only; contents default to "never" in the classifier |
| `{PROGRAMFILES}\WindowsApps` | subtree | System-managed packages |
| `{USERPROFILES}` | item | Holds every profile |
| Every direct child of `{USERPROFILES}` | item | Every profile root, including ones not resolvable unelevated, `Public`, `Default`, `All Users` |
| Per profile: `{USERPROFILE}`, `{LOCALAPPDATA}`, `{APPDATA}`, `AppData\LocalLow`, `{DOWNLOADS}`, `{DOCUMENTS}`, `{DESKTOP}`, `{PICTURES}`, `{MUSIC}`, `{VIDEOS}` (localized/redirected paths as resolved) | item | Known-folder roots; contents deletable individually |
| `{PROGRAMDATA}`, `{PUBLIC}` | item | Known-folder roots |
| Strata's install folder(s) | subtree | Self-protection |
| Any top-level item with both `SYSTEM` and `HIDDEN` | item | Spec rule; needs attributes, so checked on the handle |
| UNC paths to this machine (`localhost`, `127.*`, `::1`, its NetBIOS/DNS names) | all | Use the local path; loopback shares bypass path rules |

`{TEMP}` is the only known folder whose root is not protected
(`KnownFolder::root_is_protected`).

## Canonicalization and resolution

`canon.rs` (pure, fuzzed) turns any Win32 spelling into `CanonicalPath { root,
components }`:

1. Reject empty input, NUL, control characters, `< > " | ? *`, and any `:`
   beyond the drive colon (`file::$DATA`, `dir:$I30:$INDEX_ALLOCATION`).
2. `/` becomes `\`. Prefixes: `\\?\` and `\??\` are verbatim, `\\.\` is a
   device path (normalized), `\\` is UNC, `\Device\...` is an NT device. Under
   a prefix: `X:`, `UNC\srv\share`, `Volume{GUID}` (validated) or
   `GLOBALROOT\Device\X`. Anything else (pipes, `PhysicalDrive0`, `CON`) is
   refused. Drive-relative (`C:foo`) and rooted-relative (`\x`) paths are
   refused.
3. Repeated separators collapse. `.` and `..` resolve; in verbatim paths they
   are refused (NTFS never stores them). Climbing above the root is refused.
4. Non-verbatim paths get Win32 trimming: the last segment loses all
   trailing dots and spaces, the others lose one trailing dot. Reserved DOS
   device names (`NUL`, `COM1`, `LPT¹`...) are refused.
5. Comparison uses an ordinal one-to-one uppercase fold per scalar (like the
   NTFS `$UpCase` table; `ß` stays `ß`). Over-folding only ever over-matches.

`NeverList::check` then tests every **interpretation** and refuses if any
matches: the canonical form; its alias with all trailing dots/spaces stripped
(so `\\?\C:\Windows.\x` and `C:\Windows \x` cannot reach the
`C:\Windows\Temp` allow-list); `\\host\C$\x` as `C:\x`; and, for volume-GUID
or NT-device roots, every mount point of that volume from the volume map. A
GUID/device root missing from the map is matched root-agnostically (its
components against every rule), so an unknown root can never hide `\Windows`.

`SafetyGuard::check_path` (and every action) additionally:

1. Expands 8.3 names with `GetLongPathNameW` when a component has `~`.
2. Opens the **parent** following links and checks `final(parent) + leaf`
   (catches `link\pagefile.sys` that cannot itself be opened).
3. Opens the item with `FILE_FLAG_OPEN_REPARSE_POINT | BACKUP_SEMANTICS |
   OPEN_NO_RECALL` and checks `GetFinalPathNameByHandleW` in DOS and GUID
   form, `VOLUME_NAME_NONE == "\"` (a volume root reached by any route), and
   the top-level system+hidden rule.
4. Compares the handle's `(volume serial, 128-bit file id)` with the
   identities of every absolute protected location (both link and target)
   and every ancestor of one.
5. For a symlink/junction/mount point, opens the target and refuses if the
   target is protected.
6. Actions reopen the checked object relative to its handle (`NtCreateFile`,
   empty name) for `DELETE`, so delete access is never held on anything
   unchecked and no path lookup happens between check and act.

## Public API

```rust
// Build once per session (rebuild after volumes change).
let guard = SafetyGuard::new(GuardConfig { known, install_dirs })?;

// Flow (what the Tauri commands call)
let plan: Plan = flow::plan(&guard, queue /* Vec<QueueItem{id, path, expected, safety}> */);
let verdicts: Vec<ItemVerdict> = flow::preflight(&guard, &plan, &acks, &cfg);
let report: ExecutionReport = flow::execute(&guard, &plan, &Decision { method, acks }, &cfg,
                                            &mut audit /* impl AuditLog */, &mut |p: Progress| .., &cancel);
let retry_plan = plan.retry(&report);

// Restore from the undo log
let ticket = recycle::RestoreTicket::from_blob(&blob)?;
recycle::restore(&ticket)?;                       // never overwrites
recycle::find_in_recycle_bin(path)?;              // crash recovery

// Locks and apps
locks::who_locks(path, locks::DEFAULT_MAX_FILES)?;
locks::close_politely(&holder, Prompt::new(holder.close_request()).confirm())?;
apps::running_app_warnings(path, &holders)?;

// Helper (elevated) requests
privileged::verify_and_delete_by_id(&guard, &PrivilegedDeleteRequest { .. }, &cancel)?;
privileged::schedule_delete_on_reboot(&guard, &DelayedDeleteRequest { .. }, consent)?;

// Tools
let spec = tools::build(&ToolContext::from_known(&known)?, &ToolAction::DiskCleanup { drive: Some('C') })?;
tools::run(&spec)?;                               // after the user saw spec.command_line
tools::empty_recycle_bin(tools::empty_recycle_bin_prompt(Some("C:\\"))?.confirm())?;
```

Lower-level pieces are public too: `permanent::delete_permanently`,
`recycle::recycle`, `preflight::preflight_item`, `volume::volume_info`,
`canon::CanonicalPath`, `never::NeverList`. Every error is a serializable
`CleanError` with `message()` for the UI and `is_retryable()`.

**Restore blob** (`RestoreTicket::to_blob`, JSON, `version: 1`):
`original_path`, `recycled_path` (`$R…`), `info_path` (`$I…`), `deleted_at`
(FILETIME), `size`, `identity` (volume serial + file id). The store keeps it
as an opaque `BLOB` next to the undo-log row.

**AuditLog** (`audit.rs`): `begin_action` → per item `item_started` (before
acting; if it fails the item is skipped) → `item_finished` → `finish_action`.
Skipped items get a finish record without a start record. After a crash, rows
with a start but no finish are "interrupted"; `find_in_recycle_bin` recovers
their tickets. `MemoryAuditLog` implements it for tests and for running
before the store adapter exists.

## Tests

`cargo test -p strata-clean`: **119 tests** (57 unit, 58 integration, 4 doc),
all green. `cargo clippy -p strata-clean --all-targets -- -D warnings` is clean.
Integration tests only create files under `D:\strata-clean-tests\` (falling
back to `%TEMP%\strata-clean-tests\`), unlink junctions before cleanup, and
restore or purge exactly the Recycle Bin entries they create.

Safety tests:
- `never.rs` unit tests (12): every rule, allow-list edges, dot/space tricks,
  GUID/device roots, admin and loopback shares, messages.
- `tests/never_fuzz.rs` (proptest, 4000 cases for the main property): random
  spellings (prefix, case, separators, `x\..` detours, trailing dots/spaces,
  admin share) of 31 protected paths are never allowed; random tails under
  protected roots are refused; arbitrary strings never panic; ordinary data
  paths and `C:\Windows\Temp\*` stay allowed.
- `tests/guard.rs` (11): ~35 real spellings of protected paths (including
  `\\?\Volume{C}\Windows`, `\Device\HarddiskVolumeN\...`, `C:\PROGRA~1`), a
  junction to `C:\Windows` (link refused as `LinkTarget`, `link\System32` and
  more refused as `Inside`), junction chains into `C:\Users`, a junction to
  `C:\` (`link\pagefile.sys` refused), symlinks when creatable, loopback
  `\\localhost\C$`, ADS / device / relative spellings.
- `tests/never_routes.rs` (4) + `privileged::tests::ids_of_protected_objects_are_refused`:
  every protected path through permanent delete, recycle, a **forged plan with
  every confirmation** (classifier said "safe"), and the helper request types;
  the file ids of real system objects are refused whatever path is claimed.

Race tests (§22):
- `permanent::race_dir_swapped_for_junction_is_refused`: pre-flight a
  directory, swap it for a junction to a temp dir with a sentinel; refused
  (id mismatch), sentinel intact. Also into `C:\Windows\System32` (refused).
- `recycle::race_swapped_dir_is_not_recycled`: same swap on the Recycle Bin route.
- `privileged::path_swap_cannot_redirect_the_delete`, `renamed_file_is_refused`,
  `modified_file_is_refused`.
- `permanent::race_file_replaced_or_modified_is_refused`.
- `permanent::race_file_locked_after_preflight_is_a_typed_failure`: a child
  PowerShell opens the file exclusively after pre-flight; `Locked`, file intact.
- `permanent::locked_child_stops_the_walk_with_partial`.
- `flow::locked_item_fails_with_holders_then_retry_succeeds`: the failure
  names the process (Restart Manager), then `Plan::retry` succeeds.

Recycle Bin: file and folder round-trips through the blob, missing parent
recreated, batch with a locked middle item (the rest still recycled), restore
never overwrites, tampered ticket refused, `find_in_recycle_bin`, long paths,
and `recycle::tests::sink_stops_shell_permanent_delete`.

## Decisions

- **Restore = parse `$I` + move `$R` back**, not the Shell "undelete" verb.
  The `$I` format is documented and stable (v2 since Windows 10), the move is
  a same-volume rename with no UI, conflicts are reported instead of prompted,
  and tests behave like the app. The `$R` path comes from the progress sink's
  `psiNewlyCreated`; a `$I` scan is the fallback.
- **The Shell silently permanently deletes paths ≥ `MAX_PATH`** even with
  `FOFX_RECYCLEONDELETE` (observed: `PreDeleteItem` arrives without
  `TSF_DELETE_RECYCLE_IF_POSSIBLE`). Three layers stop silent nukes: plan and
  pre-flight report `PathTooLong` / `TooLarge` / `Unavailable`; the sink
  aborts any item lacking that flag (`WouldDeletePermanently`); and
  `FOF_WANTNUKEWARNING` makes Windows ask instead as a last resort.
- **`IFileOperation` runs on a dedicated STA thread per batch**
  (`COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE`), so the caller's
  apartment (Tauri/tokio threads) never matters. `FOFX_EARLYFAILURE` stops a
  batch at the first failure; items not reached are retried in a follow-up
  operation automatically.
- **Recycle TOCTOU**: ancestors are held open without `FILE_SHARE_DELETE` for
  the operation, the Shell gets the handle-resolved path, and afterwards the
  item's own handle must resolve inside `$Recycle.Bin`; if something else was
  recycled in its place it is moved straight back.
- **Links whose target is protected are refused** even though unlinking is
  harmless. When in doubt, refuse; it also keeps the compatibility junctions in
  profiles (`Application Data` …) out of reach. Link *children* of a folder
  being deleted are unlinked, never followed.
- **`$Recycle.Bin` is protected as a subtree**, not only its root (the spec
  says root); deleting payloads by hand corrupts the bin.
- **Every child of the profiles root is a profile root**, so profiles we cannot
  resolve unelevated are still protected.
- **Loopback UNC paths are refused outright.**
- **Reopen through `NtCreateFile` relative to the handle with an empty name**,
  not `ReOpenFile`: on 26200 `ReOpenFile` fails on directory handles (access
  denied) and rejects `FILE_FLAG_OPEN_NO_RECALL`.
- **By-id deletes re-open the verified path**: NTFS refuses to delete through
  a handle opened by file id (no name context). The re-opened handle passes the
  guard again and must have the same `(serial, id)`.
- **Directory walks stop at the first failure** (`CleanError::Partial` with a
  count) rather than continuing and leaving scattered holes; pre-flight's lock
  check prevents most of these.
- **Delete-on-reboot is limited to plain, single-link files**: Windows deletes
  by path at boot, after our checks.
- **Consent is a type**: `Consent<A>` is minted only by `Prompt::confirm`, is
  not `Clone`/`Send`/`Deserialize`, expires after 120 s, and emptying the bin
  also requires the item count and size to be unchanged since the prompt.
- **Directory `Expected.size` is not verified** (it is the subtree total, used
  for capacity and the large-delete threshold); files verify size and mtime.
- `windows-core = "0.62.2"` is a direct dependency because `#[implement]`
  expands to `windows_core::` paths; it must match the `windows` 0.62 line
  (crates.io latest is 0.100, which would not interoperate).

## Blockers and limits

- **Elevation**: scheduling delete-on-reboot, DISM, deleting other users'
  files and creating volume mount points need admin. The dev session is
  unelevated; these are validation- or construction-tested only.
- **Too large for the Recycle Bin with a real `MaxCapacity`** is not tested
  end to end: it would mean changing the owner's per-volume setting. The
  pre-flight comparison is unit-tested, and the sink backstop is proven by the
  long-path case, which uses the same Shell signal.
- **Removable and network drives**: none attached; classification is
  unit-tested (`volume::tests::removable_and_network_have_no_recycle_bin`).
- **Polite close** (`close_politely`) is not run against a real app in tests,
  because it would close the owner's programs.

## Core change requests

- Add `KnownFolder::LocalAppDataLow` (`FOLDERID_LocalAppDataLow`); the
  never-list currently derives `AppData\LocalLow` from `LocalAppData`'s parent.
  Also worth adding: `OneDrive`, `SavedGames`, `Favorites`, `Links`,
  `ProgramFilesArm`, `ProgramFilesCommon`.
- `FileRef` is 64-bit; ReFS ids are 128-bit. Matching falls back to the 64-bit
  `nFileIndex` the walker records. A `FileId128` in core would make ReFS exact.
- The walker's synthetic ids (`FileRef::SYNTHETIC_BIT`) can never be verified,
  so such items are refused (`Change::SyntheticReference`). The walker should
  record the real file index for anything the UI can queue, or the UI must
  re-stat an item before queueing it.

## Next steps

- Tauri commands over `flow::{plan, preflight, execute}` and `recycle::restore`,
  with `Progress` on a Channel and `CancelToken` per run.
- `AuditLog` adapter over `strata-store` (SQLite), including the
  "interrupted" recovery pass with `find_in_recycle_bin`.
- Helper pipe: `PrivilegedDeleteRequest` / `DelayedDeleteRequest` as protocol
  messages in `strata-ipc`, served by `strata-helper` with its own
  `SafetyGuard`.
- Review screen: tier totals, careful ticks, `CannotRecycle` decisions,
  "Close X first", lock holders with Close/Skip.
- Re-run the suite elevated and on removable/network/ReFS volumes.
