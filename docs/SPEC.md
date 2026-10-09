# STRATA — Master Build Prompt
### A native Windows disk-space intelligence app (Tauri v2 + Rust)

> **Working title: "Strata."** Rename freely; search-and-replace the name across the repo.
>
> **How to use this file:** put it in the repo root as `docs/SPEC.md` (and reference it from `CLAUDE.md`). It is both the product spec and your standing instructions. Work through the milestones in order until every acceptance criterion in this document passes on a real Windows machine.

---

## 0. Your role and how you work

You are the sole engineer building Strata end to end: the scanning engine, the elevated helper, the UI, the installer, the tests, and the docs. Your standard is "a paid product someone would trust with their C: drive."

### Rules of engagement

1. **No stubs left behind.** Never leave `todo!()`, `unimplemented!()`, `// TODO`, mock data, or placeholder UI in a milestone you mark complete. If something is genuinely blocked, write it in `docs/BLOCKERS.md` with the exact reason, then continue with everything else.
2. **Keep `docs/PROGRESS.md` current.** It holds the milestone checklist, what's done, what's next, known issues, and benchmark numbers. Update it at the end of every work session, so any fresh session can resume from it.
3. **Verify, don't assume.** Windows paths, API behavior, crate APIs, and version numbers must be checked against the actual machine and current docs. Where this spec names a path or API detail you can't confirm, verify it on the machine before relying on it.
4. **Every feature ships with tests** (section 22). Run `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test`, the frontend typecheck and lint, and the frontend tests before declaring anything done.
5. **Correctness beats speed beats features.** A fast scanner with wrong totals is worthless. Wrong deletes are unacceptable.
6. **Safety is non-negotiable.** Nothing is deleted without explicit user confirmation. Nothing in the "never" tier can be deleted through the UI, period.
7. **Small, reviewable commits** with clear messages. One concern per commit where practical.
8. When a decision isn't covered here, pick the option that is safest for user data, then fastest, then simplest. Record the decision in `docs/DECISIONS.md`.

---

## 1. What we're building (product summary)

Strata is a Windows desktop app that shows **everything** taking up space on your drives as an interactive visual map. It explains **what** each thing is, **who** (which app) put it there, **when** it was created, changed, or touched, and **whether it's safe to remove**. Then it lets you clean up safely.

What sets it apart from WizTree, WinDirStat, SpaceSniffer, and TreeSize:

| Capability | Strata |
|---|---|
| Scan speed | Raw NTFS MFT parsing, so millions of files in seconds |
| Live | USN Change Journal tailing: the map updates as files change, with no rescans |
| Understanding | Rule-pack classifier that labels data (cache, temp, build artifact, model weights, game, installer, etc.) with safety tiers |
| Attribution | Maps files and folders to installed apps ("Claude: 3.2 GB across 6 locations") |
| Live process attribution | Optional ETW kernel file tracing: "which process wrote what, right now" |
| History | Snapshots and diffs: "what grew this week" |
| Search | Instant, Everything-style filename search over the in-memory index |
| Safety | Tiered cleanup, Recycle Bin by default, lock detection, undo log |

**Target user:** a power user or developer whose disk keeps filling up with caches, node_modules, AI models, game installs, temp files, downloads, and app junk, and who wants to see and understand all of it.

---

## 2. Hard requirements and performance targets

These are acceptance criteria. Benchmark them and record results in `PROGRESS.md`.

| Metric | Target |
|---|---|
| Initial MFT scan, 1M files, NVMe | ≤ 3 s to full tree with sizes |
| Initial MFT scan, 5M files, NVMe | ≤ 12 s |
| Fallback walker, 1M files, NVMe | ≤ 30 s |
| Index memory | ≤ 64 bytes per entry average, excluding name bytes |
| Cold app start to first paint | ≤ 600 ms |
| Treemap interaction (pan, zoom, drill, hover) | 60 fps with 1M+ entries indexed |
| Drill-down relayout of a 100k-entry subtree | ≤ 50 ms |
| Filename search over 5M entries | first results ≤ 50 ms |
| Live USN update to UI reflection | ≤ 1 s |
| Installer size | ≤ 15 MB |
| Idle CPU (live mode on, ETW off) | ≈ 0% (< 0.5% avg) |
| Correctness | MFT scan totals match the fallback walker's totals for the same volume, with documented, explained differences only (hardlinks, ADS, system metadata) |

Also benchmark against WizTree on the same machine and record the comparison.

Platform: Windows 10 22H2+ and Windows 11. Ship builds for both **x64 and ARM64**.

---

## 3. Tech stack (pinned decisions)

- **Shell:** Tauri v2 (WebView2). Check the current stable versions before starting.
- **Core:** Rust (stable), as a Cargo workspace.
- **Windows APIs:** the `windows` crate (Microsoft's official crate). Use `windows-sys` only where it is lighter and sufficient.
- **Parallelism:** `rayon` for CPU work, and dedicated threads with overlapped or large sequential I/O for disk reads.
- **ETW:** `ferrisetw`, or direct `windows` crate bindings if it falls short (evaluate and decide).
- **Hashing:** `xxhash-rust` (xxh3) for partial hashes, `blake3` for full hashes.
- **Persistence:** SQLite via `rusqlite` (bundled) for snapshots, history, settings, and the undo log. Use a custom compact binary format for full-index cache files.
- **Serialization over IPC:** binary buffers through Tauri v2 Channels for bulk data (layout rects, rows). Use JSON only for small control messages.
- **Frontend:** TypeScript + React + Vite, with Zustand for state, TanStack Virtual for long lists and tables, and **raw WebGL2** (instanced quads) for the treemap and other heavy views. Use Canvas2D only for overlays and labels. No heavy chart libraries in hot paths.
- **Installer:** Tauri bundler (NSIS and/or MSI), signed.
- **Signing:** Azure Trusted Signing (or an equivalent code-signing certificate). Unsigned elevated apps get blocked by SmartScreen.
- **Updates:** Tauri updater plugin with signed update manifests.

### Repo layout

```
/
├─ CLAUDE.md                 # points to docs/SPEC.md, build/test commands
├─ docs/ SPEC.md PROGRESS.md DECISIONS.md BLOCKERS.md RULES.md BENCHMARKS.md
├─ crates/
│  ├─ strata-ntfs/           # raw MFT parser + USN journal reader (pure, fuzzable)
│  ├─ strata-walk/           # fallback parallel directory walker
│  ├─ strata-index/          # in-memory index, aggregation, search
│  ├─ strata-classify/       # rule engine + app attribution
│  ├─ strata-layout/         # treemap/sunburst/etc. layout engines
│  ├─ strata-etw/            # ETW file-activity tracing
│  ├─ strata-clean/          # deletion, recycle bin, lock detection, undo log
│  ├─ strata-store/          # SQLite snapshots/history/settings
│  ├─ strata-ipc/            # helper<->app protocol types + pipe transport
│  └─ strata-helper/         # elevated helper binary / Windows service
├─ src-tauri/                # Tauri app (unelevated), wires crates to UI
├─ ui/                       # React/TS frontend
├─ rules/                    # rule packs (TOML), versioned, documented
├─ tests/fixtures/           # VHDX build scripts + golden outputs
├─ bench/                    # benchmark harness
└─ scripts/                  # build, sign, fixture creation, release
```

---

## 4. Process architecture and privileges

```
┌──────────── Strata UI process (UNELEVATED) ─────────────┐
│ WebView2 frontend  ⇄  Tauri Rust backend                │
│ index · layout · classify · search · store · clean*     │
└───────────────▲─────────────────────────────────────────┘
                │ named pipe (ACL-restricted, versioned protocol)
┌───────────────┴──── strata-helper (ELEVATED) ───────────┐
│ raw volume open · MFT read · USN tail · ETW session ·   │
│ privileged deletes (validated)                          │
└─────────────────────────────────────────────────────────┘
```

- **The UI never runs elevated.** Elevated WebView2 is a security risk and breaks drag-and-drop.
- **The helper** comes in two modes:
  1. **On-demand:** launched via `ShellExecuteExW` with the `runas` verb when the user clicks "Fast scan (admin)". This shows a UAC prompt each session.
  2. **Service mode (opt-in, installed by the installer):** a Windows service, so there's no UAC prompt per launch. It starts on demand and exits when idle.
- **If the user declines elevation,** fall back to the unelevated walker. The UI clearly shows "Standard scan — some system folders hidden. [Enable fast scan]".
- **Pipe security:**
  - Restrict the DACL to the current interactive user's SID plus SYSTEM.
  - Verify the client's process image path and Authenticode signature match Strata's.
  - Version-handshake the protocol and reject mismatches.
  - Rate-limit requests.
  - The helper never trusts paths from the client blindly (see section 15.7).
- **Helper crash or disconnect:** the UI detects it, shows a non-blocking banner, keeps the last index usable, and offers to reconnect.
- **Privileges the helper enables as needed:** `SeBackupPrivilege`, `SeRestorePrivilege` (only if required), and `SeManageVolumePrivilege`. Drop any privilege not in use.

---

## 5. Volume discovery

- Enumerate volumes with `FindFirstVolumeW`/`FindNextVolumeW`, `GetVolumePathNamesForVolumeNameW`, `GetVolumeInformationW`, `GetDriveTypeW`, and `GetDiskFreeSpaceExW`.
- **Show volumes with no drive letter** (mounted in folders, recovery partitions where readable) and **volume mount points** inside other volumes. A mount point is a boundary: don't count the inner volume inside the outer volume's totals; show it as a linked child.
- For each volume, record:
  - Filesystem (NTFS, ReFS, FAT32, exFAT, Dev Drive/ReFS, network).
  - Cluster size, total, used, and free space.
  - Serial number and label.
  - BitLocker state (locked volumes are unscannable: show the lock state and skip).
  - Removable/fixed/network status.
  - Whether it's the system volume.
- **Choosing a scanner:**
  - **NTFS + elevated** → MFT scanner.
  - Everything else, including ReFS, Dev Drive, FAT/exFAT, network shares, and NTFS without elevation → the fallback walker.
- **Hot-plug:** listen for `WM_DEVICECHANGE` (volume arrival and removal). Removed volume: mark its index stale and grey it out. New volume: list it and don't auto-scan unless the user enabled "auto-scan removable".
- **Network drives:** opt-in only, with a warning that scans may be slow. Handle disconnects and timeouts gracefully.
- **Explained totals:** "Used space" (from the volume) vs. "sum of everything we found." The difference is shown as a named block, **"Unaccounted / system reserved,"** with an explanation: NTFS metadata, shadow copies we couldn't enumerate, files we lacked permission to read, the MFT zone, and so on. Never silently hide a gap.

---

## 6. MFT scanner (`strata-ntfs`): the core

Pure Rust, `#![forbid(unsafe_code)]` where possible except the thin I/O layer, and **fuzzed**.

### 6.1 Reading
1. Open `\\.\X:` with `FILE_READ_DATA`, `FILE_SHARE_READ | FILE_SHARE_WRITE`, and `FILE_FLAG_NO_BUFFERING` (sector-aligned buffers) or `FILE_FLAG_SEQUENTIAL_SCAN`. Benchmark both.
2. Parse the NTFS boot sector: bytes per sector, sectors per cluster, MFT start LCN, and MFT record size. Clusters-per-record is a **signed** value: negative means 2^|n| bytes. Records are usually 1024 bytes, or 4096 on 4Kn drives. Handle both.
3. Read MFT record 0 (`$MFT` itself). Decode its unnamed `$DATA` runlist to get **every fragment** of the MFT. **The MFT is often fragmented**; handle many runs.
4. Optionally read `$MFT:$BITMAP` to skip unused records quickly.
5. Read the MFT in large sequential chunks (4–16 MB, tuned by benchmark) per fragment on an I/O thread. Hand chunks to a `rayon` pool for parsing. Pipeline I/O and CPU.

### 6.2 Record parsing (every edge case)
- Validate the `FILE` signature. Treat `BAAD` or garbage as corrupt: count it and skip it.
- **Apply update sequence array (fixup) corrections** to every sector before parsing. A fixup mismatch means a torn or corrupt record: skip it and count it.
- Skip records without the in-use flag. Note the directory flag.
- Track the **sequence number** of each record. Parent references are (record number, sequence number), and a parent whose sequence doesn't match means a stale or orphaned reference.
- **Extension records** (base-record reference ≠ 0) hold overflow attributes for a base record. Merge them into the base. They may arrive before the base record in read order, so buffer and merge.
- **`$ATTRIBUTE_LIST` (0x20)** can be resident or non-resident. If non-resident, read its runs from disk. Use it to locate attributes held in extension records.
- **`$STANDARD_INFORMATION` (0x10):** created, modified, MFT-changed, and accessed times (FILETIME), plus file attributes (hidden, system, readonly, compressed, encrypted, sparse, temporary, offline, not-content-indexed, reparse, pinned/unpinned for cloud files, recall-on-open/recall-on-data-access).
- **`$FILE_NAME` (0x30):** parent reference, name, and namespace.
  - Namespaces: POSIX (0), Win32 (1), DOS (2), Win32&DOS (3). **Skip DOS-only (2) names**; they are 8.3 duplicates.
  - **Multiple non-DOS `$FILE_NAME` attributes mean hardlinks.** One record can appear in several directories (section 7.2).
  - `$FILE_NAME` timestamps and sizes are often stale. Use `$STANDARD_INFORMATION` times and `$DATA` sizes as the truth, but keep `$FILE_NAME` created-time available for forensics display (advanced panel).
- **`$DATA` (0x80):**
  - **Unnamed stream** = file content.
  - **Named streams** = Alternate Data Streams. Record each one's name and sizes. Common ones like `Zone.Identifier` are tiny but countable; `WofCompressedData` is special (below).
  - **Resident data:** the logical size is the value length, and allocated size is 0 extra clusters (it lives in the MFT record).
  - **Non-resident data:** read real size, allocated size, initialized size, and, **if compressed or sparse, the "compressed size" field** (total allocated clusters actually used). Use it for on-disk size.
  - A `$DATA` attribute can be **split across multiple attribute instances** (different VCN ranges, in extension records). Take sizes from the instance with starting VCN 0, and merge runlists if they're needed.
- **`$REPARSE_POINT` (0xC0):** read the reparse tag.
  - `IO_REPARSE_TAG_SYMLINK` and `IO_REPARSE_TAG_MOUNT_POINT` (junctions/mount points) → **do not traverse.** Record the target for display.
  - `IO_REPARSE_TAG_WOF` → Windows Overlay Filter (CompactOS / `compact /exe`). The real on-disk bytes live in the named stream `WofCompressedData`. Report allocated = that stream's allocation, and flag "Compressed (WOF)".
  - `IO_REPARSE_TAG_CLOUD*` (OneDrive and other cloud-files providers) → placeholder. Its logical size may be large while local allocation is ~0. Flag "Cloud — online-only" vs. "Cloud — locally available" vs. "Always keep on device", using the pinned/unpinned/recall attributes.
  - `IO_REPARSE_TAG_DEDUP` → data deduplication (Windows Server). Allocated size is unreliable; flag it.
  - `IO_REPARSE_TAG_APPEXECLINK` → app execution aliases (WindowsApps). ~0 bytes.
  - `IO_REPARSE_TAG_LX_SYMLINK` and other WSL tags → flag them.
  - Unknown tags → record the raw tag and flag it as "reparse point (unknown)".
- **Directories:** `$INDEX_ROOT` (0x90) and `$INDEX_ALLOCATION` (0xA0). The B-tree allocation **is real disk usage**; count it as the directory's own allocated bytes (shown as "directory index overhead").
- **Encrypted files (EFS):** sizes are readable from the MFT. Flag "Encrypted".
- **Per-directory case sensitivity** (WSL interop): store names exactly. Never case-fold names for identity; use the file reference number as identity.
- **System metadata files** (records 0–15ish plus `$Extend` children): `$MFT`, `$MFTMirr`, `$LogFile`, `$Volume`, `$AttrDef`, root, `$Bitmap`, `$Boot`, `$BadClus` (sparse; don't count its logical size), `$Secure` (`$SDS` stream), `$UpCase`, `$Extend\$UsnJrnl` (`$J` is **sparse**, so count allocated, not logical), `$Extend\$ObjId`, `$Quota`, `$Reparse`, and `$RmMetadata`. Group them under a virtual **"NTFS metadata"** node at the volume root with plain-English explanations.
- **Orphans:** records whose parent doesn't exist or whose sequence doesn't match. Attach them under a virtual **"Orphaned entries"** node; don't drop them.
- **Cycles:** guard against corrupt parent chains forming loops (detect during tree build; break the loop and attach to Orphaned).
- **Name decoding:** UTF-16LE, which may contain **unpaired surrogates**. Store raw UTF-16 or WTF-8 losslessly and display with replacement characters. Never panic and never lose identity.
- **Never panic on malformed input.** Every length, offset, and runlist must be bounds-checked. Fuzz it (section 22).

### 6.3 Runlist decoding
- Header nibbles give the length-field and offset-field sizes. The offset is **signed and relative** to the previous LCN.
- Offset size 0 = **sparse run** (no clusters on disk).
- Guard against runs pointing past the volume end, zero-length runs, and absurd counts.

### 6.4 Output
Emit a stream of compact records (file reference, parent reference(s), name(s), flags, sizes, times, reparse info, ADS list) into `strata-index`. The helper streams them over the pipe in batches using a binary framing format.

---

## 7. Size accounting rules (get these exactly right)

Every entry carries:
- **Logical size:** what Explorer's "Size" shows (unnamed stream real size).
- **Allocated size:** actual clusters on disk (respecting compression, sparse, WOF, and resident data).
- **ADS bytes:** logical and allocated sizes of named streams.
- **Directory overhead:** index allocation for directories.

Users can toggle the **size mode** globally: *Allocated (default, "what it actually costs")* or *Logical ("what files claim to be")*. Every view respects it.

### 7.1 Aggregation
Each directory stores subtree totals: logical, allocated, file count, directory count, newest modified time, oldest modified time, and the largest descendant. Compute bottom-up in parallel after the scan, and **incrementally** on live updates (walk up the parent chain applying deltas, O(depth)).

### 7.2 Hardlinks
- One MFT record with N names, data counted **once**.
- Policy: attribute the bytes to the **first discovered** path. Every other path shows the file with a "hardlink (counted elsewhere)" badge and 0 contributed bytes.
- Exception setting: "split hardlink bytes evenly across paths." It's off by default; document it.
- **WinSxS:** most of its apparent size is hardlinked into System32 and elsewhere. The WinSxS node shows an explainer card. Its "actual unique" size = bytes only reachable through WinSxS.

### 7.3 Reparse points
Never traverse symlinks, junctions, or mount points. The target is displayed and clickable ("jump to target"); bytes belong to the target's real location.

### 7.4 Cloud placeholders
Counted at **allocated** size (usually ~0). Show their logical "cloud size" separately in tooltips and in the Cloud view.

### 7.5 Volume reconciliation
`volume used` = Σ allocated (files + ADS + dir overhead + NTFS metadata) + unaccounted. Show "unaccounted" explicitly with known likely causes:
- Volume Shadow Copy storage. Query it via WMI `Win32_ShadowStorage` and show it as its own virtual block **"System Restore / Shadow copies"**, with its used and max sizes and a link to System Protection settings.
- Free-space bitmap rounding.
- Inaccessible items (fallback walker only).

---

## 8. Fallback walker (`strata-walk`)

- A work-stealing parallel traversal (one queue per worker, rayon-style). Each directory is listed with `FindFirstFileExW(FindExInfoBasic, FIND_FIRST_EX_LARGE_FETCH)` **or** `NtQueryDirectoryFile` with large buffers (benchmark and pick the faster).
- Use the `\\?\` prefix for **all** paths (long paths > 260 chars, trailing dots and spaces, reserved names like `CON`, `NUL`, `AUX`, `COM1`).
- Don't follow reparse points (check `FILE_ATTRIBUTE_REPARSE_POINT` and the reparse tag from find data).
- Allocated size: `GetFileInformationByHandleEx(FileStandardInfo)` (AllocationSize) needs a handle per file, which is expensive. Strategy: logical size from find data during the walk, then a **background pass** that opens files with `FILE_READ_ATTRIBUTES` only (no data access, minimal antivirus interference) to fill allocated size, compression, and hardlink count/IDs (`FileIdInfo` for dedup of hardlinks). Compressed files: `GetCompressedFileSizeW`.
- Permission denied → record the directory as "Access denied (N items unknown)", show a lock icon, and continue.
- Files deleted or renamed mid-scan → ignore gracefully.
- Network paths → bounded concurrency (configurable), per-request timeout, and cancellation.
- **Cancellation everywhere:** every scan is cancellable instantly. Partial results remain browsable and are flagged "Partial".

---

## 9. In-memory index (`strata-index`)

### 9.1 Layout: struct-of-arrays with u32 IDs

```
EntryId = u32  (index into arrays)
parent:          Vec<u32>
first_child:     Vec<u32>
next_sibling:    Vec<u32>
name_offset:     Vec<u32>   // into one interned name buffer
name_len:        Vec<u16>
flags:           Vec<u32>   // dir, hidden, system, compressed, sparse, encrypted, reparse kind, cloud state, hardlink-secondary, ads-present, orphan, access-denied, deleted-pending...
logical:         Vec<u64>
allocated:       Vec<u64>
mtime/ctime/atime/mftchange: Vec<u32>  // seconds since a custom epoch (2000-01-01) fits to 2136; keep full FILETIME only in an on-demand detail fetch
file_ref:        Vec<u64>   // NTFS file reference (record + sequence)
category:        Vec<u16>   // classifier result
owner_app:       Vec<u32>   // app attribution id (0 = unknown)
ext_id:          Vec<u16>   // interned extension
// dir-only aggregates in a side table keyed by dir id
```

- Names: one big buffer (WTF-8 or UTF-16), with no per-name heap allocation.
- `file_ref → EntryId` hash map (needed for USN updates and hardlinks).
- Volumes over ~4B entries are out of scope, but **detect and refuse gracefully** above `u32::MAX - margin`.
- **Low memory:** if available RAM is low, warn. Offer a "lite index" mode (drop per-file times, keep directory aggregates) and a disk-backed index cache.
- Index **cache file** per volume: mmap-friendly binary format with a header (magic, version, volume serial, USN journal ID, last USN, build version). On launch, load the cache and **catch up from the USN journal** instead of rescanning (section 10.3).

### 9.2 Queries the index must answer fast
- Children of X, sorted by any column (size, count, name, modified, category).
- Path of X (walk the parents; cache recent results).
- Top-N largest files and folders globally or under X (use a heap, or maintain incrementally).
- Filter by extension, category, owning app, date range, size range, attributes, or cloud state. Filters combine.
- Extension and file-type breakdown under X.
- Search (section 17).

---

## 10. Live updates: USN Change Journal

### 10.1 Reading
- `FSCTL_QUERY_USN_JOURNAL` gets the journal ID, first USN, and next USN. If the journal is **not active**, show "Live updates unavailable — journal disabled" with an option to enable it (`FSCTL_CREATE_USN_JOURNAL`, user-confirmed, helper only).
- Tail with `FSCTL_READ_USN_JOURNAL` using `BytesToWaitFor`/timeout so it **blocks** instead of polling. Handle both USN_RECORD_V2 and V3 (128-bit file IDs, which ReFS uses) and V4 range records if encountered.

### 10.2 Applying changes
- Reasons to handle: FILE_CREATE, FILE_DELETE, RENAME_OLD_NAME/RENAME_NEW_NAME (pair them; moves between directories change both parents' aggregates), DATA_OVERWRITE/EXTEND/TRUNCATION (re-read sizes), HARD_LINK_CHANGE, REPARSE_POINT_CHANGE, STREAM_CHANGE (ADS), BASIC_INFO_CHANGE (attributes and times), COMPRESSION_CHANGE, ENCRYPTION_CHANGE, and CLOSE (coalesce).
- USN records don't include sizes. For changed files, fetch fresh metadata through the helper: open by file ID (`OpenFileById`) with read-attributes only, or re-read that MFT record. **Debounce and coalesce:** a file written 10,000 times in a second is re-measured once per tick (e.g., 250 ms).
- Apply deltas up the ancestor chain and push minimal update events to the UI (changed IDs plus new aggregates). The UI animates affected blocks.

### 10.3 Edge cases
- **Journal wrapped** (our saved USN < FirstUsn) → changes were lost. Do a full rescan automatically (MFT scan is fast) and tell the user why, unobtrusively.
- **Journal ID changed** (deleted and recreated) → full rescan.
- **Volume dismounted or removed while tailing** → stop cleanly and mark stale.
- **Sleep/resume** → on resume, catch up from the saved USN.
- **Massive bursts** (e.g., `npm install` or extracting 200k files) → batch, keep the UI responsive, and show a "Catching up… 48,210 changes" indicator.
- Non-NTFS volumes have no USN journal (ReFS has one; support it if V3 works). Otherwise offer a manual "Rescan," plus optional `ReadDirectoryChangesW` watching of the currently viewed subtree only.

---

## 11. Live process attribution: ETW (`strata-etw`)

Opt-in, helper-only, clearly labeled **"Activity tracking (advanced)"**.

- Start a real-time ETW session with the `Microsoft-Windows-Kernel-File` provider (or the NT Kernel Logger FileIo class; evaluate which gives reliable create/write/delete with process ID and file name at low overhead). Use a **unique, fixed session name**. On start, if a stale session with that name exists from a crash, stop it first.
- Map events to (process ID → image path, start time) via process start/stop events or a snapshot. **Process IDs get reused**; key on (process ID, process start time).
- Map file objects and keys to paths using create/name events. Normalize device paths (`\Device\HarddiskVolume3\...`) to drive-letter paths with `QueryDosDeviceW`.
- Aggregate in rolling windows: bytes written, files created, and files deleted per process per directory. Persist hourly rollups to SQLite (configurable retention, default 30 days).
- Outputs:
  - **Activity view:** top writers now, in the last hour, and today.
  - **Per-entry "Who touched this":** last writer process and time, when known.
  - Feeds **app attribution** (section 12.3).
- **Overhead guard:** measure CPU cost. If it exceeds the threshold (e.g., 2% sustained), auto-throttle (sample) or suggest disabling.
- Stop the session cleanly on exit. A crash-safety check on next launch kills orphaned sessions.
- **Privacy:** activity data stays local, is never transmitted, and can be cleared with one click.

---

## 12. Understanding the data: classifier and attribution (`strata-classify`)

### 12.1 Rule packs
- Rules are **data**, not code: TOML files in `/rules`, embedded in the binary, versioned, and user-extensible (a user rules folder that overrides built-ins). Document the schema in `docs/RULES.md`.
- Rule schema (illustrative):

```toml
[[rule]]
id = "node.node_modules"
name = "Node.js dependencies"
category = "dev.dependencies"
match.dir_name = "node_modules"          # or: path_glob, path_regex, ext, env_root, magic, min_size
match.requires_sibling = ["package.json"] # optional context checks
app = "Node.js / npm"                     # attribution label (optional)
safety = "safe"                           # safe | probably | careful | never
regenerable = true
explain = "Packages installed for a project. Deleting is safe; run `npm install` to restore."
action = "delete"                          # delete | open_tool | info_only
```

- **Path roots:** rules use known-folder tokens resolved per user via `SHGetKnownFolderPath`: `{LOCALAPPDATA}`, `{APPDATA}`, `{PROGRAMDATA}`, `{TEMP}`, `{USERPROFILE}`, `{DOWNLOADS}`, `{WINDIR}`, `{PROGRAMFILES}`, `{PROGRAMFILES_X86}`, and so on. **Handle multiple user profiles** on the machine: when elevated, enumerate `C:\Users\*` and resolve per profile, attributing to that user.
- **Precedence:** the most specific match wins (longest path match > glob > name > extension). Ties go to the stricter safety tier. Document it.
- Every classified entry shows *why*: the rule ID, name, and explanation.

### 12.2 Built-in rule coverage (minimum; verify each path on the real machine)

**Windows system**
- `%TEMP%`, `C:\Windows\Temp` → safe (skip files in use)
- Windows Update download cache `C:\Windows\SoftwareDistribution\Download` → probably (better: offer "Run Windows Disk Cleanup")
- Delivery Optimization cache → safe
- Windows Error Reporting (`ProgramData\Microsoft\Windows\WER`), crash dumps (`C:\Windows\MEMORY.DMP`, `Minidump`, `%LOCALAPPDATA%\CrashDumps`) → safe
- Thumbnail and icon caches (`thumbcache_*.db`, `iconcache_*.db` in Explorer's local folder) → safe (regenerated)
- DirectX shader cache, NVIDIA/AMD shader caches → safe
- GPU driver installer leftovers (`C:\NVIDIA`, `C:\AMD`) → probably
- `Windows.old`, `$WINDOWS.~BT`, `$WINDOWS.~WS`, `$GetCurrent` → careful (recommend the built-in removal path)
- `hiberfil.sys` → never via delete; info + "disable hibernation" guidance (`powercfg /h off`, explained, user-run)
- `pagefile.sys`, `swapfile.sys` → never; info only
- `C:\Windows\WinSxS` → never; explainer + offer to run component cleanup (`DISM /Online /Cleanup-Image /StartComponentCleanup`) as a user-confirmed elevated action with output shown
- `C:\Windows\Installer` → never (orphan detection is too risky to automate; explain why)
- `System Volume Information` / shadow copies → never via delete; link to System Protection
- `$Recycle.Bin` (per user SID, map SID to account name) → safe ("Empty Recycle Bin" via `SHEmptyRecycleBinW`)
- Prefetch → careful (info: Windows manages it)
- CBS / DISM logs → probably
- Font cache → careful
- Everything else under `C:\Windows`, `C:\Program Files*` not matched by a rule → **never** by default

**Browsers** (per profile; detect all profiles): Chrome, Edge, Brave, Opera/Opera GX, Vivaldi, Arc, Firefox (profiles.ini). Cache, Code Cache, GPUCache, Service Worker CacheStorage → safe (warn if the browser is running). History, cookies, and bookmarks → **never** (not junk).

**Electron / chat / productivity app caches:** Discord, Slack, Teams (new and classic), VS Code, Cursor, Spotify (Storage), Zoom, Notion, Figma, Obsidian. Cache, GPUCache, and Code Cache folders → safe. Data folders → never.

**AI / ML (often huge):**
- Hugging Face cache (`~/.cache/huggingface`) → careful ("model weights — re-downloadable but large")
- Ollama models (`~/.ollama/models`) → careful; attribute per model if parseable from manifests
- LM Studio models, ComfyUI `models/` and `output/`, Automatic1111/Forge models, torch hub cache (`~/.cache/torch`), pip wheels
- **Claude:** detect and attribute the Claude desktop app (verify actual install and data locations on this machine; likely candidates are under `%LOCALAPPDATA%` and `%APPDATA%`), Claude Code (`~/.claude`, global npm package location, and any caches or logs it creates). Break down by sub-purpose (app binaries, updates/old versions, logs, caches, project/session data) with per-folder safety tiers. **Session and project data is "careful"**, not safe.
- Other AI tools present on the machine (Cursor, Copilot caches, etc.) via the same pattern.

**Developer tooling:**
- `node_modules` (with `package.json` sibling) → safe, regenerable; **also detect stale ones** (project not modified in N days)
- npm cache (`%LOCALAPPDATA%\npm-cache`), Yarn cache, pnpm store, Bun cache → safe
- pip cache, conda pkgs, Poetry cache, uv cache, `__pycache__`, `.venv`/`venv` (with `pyvenv.cfg`) → safe/probably
- Cargo registry and git cache (`~/.cargo/registry`), Rust `target/` (with `Cargo.toml` sibling) → safe; rustup toolchains → careful
- Go module cache, Gradle (`~/.gradle/caches`), Maven `~/.m2`, NuGet packages cache → safe
- .NET `bin/` and `obj/` (with `.csproj` sibling) → safe
- `.next`, `.nuxt`, `.svelte-kit`, `dist`, `build`, `.turbo`, `.parcel-cache`, `.vite` (only with a project-file sibling, to avoid false positives) → safe
- Android SDK system images and AVD emulators → careful
- Visual Studio caches (`.vs`, ComponentModelCache), VS installer packages cache → probably
- vcpkg buildtrees and downloads → safe
- Unity `Library/`, Unreal `DerivedDataCache`, `Intermediate`, `Saved` → safe/probably
- **Docker Desktop disk image** (`docker_data.vhdx` / `ext4.vhdx`) → careful; explain that you prune through Docker, and offer to show the Docker-reported size if the `docker` CLI exists
- **WSL distros** (`ext4.vhdx` under `%LOCALAPPDATA%\Packages\*` or custom paths) → careful; show which distro (via `HKCU\...\Lxss` registry); explain that space freed inside WSL doesn't shrink the vhdx without compaction (show guidance)
- `.git` folders → never auto-delete; show size, and flag "large pack" repos

**Games / launchers:** Steam (parse `libraryfolders.vdf` and `appmanifest_*.acf` → per-game names and sizes, shader cache, `downloading` leftovers), Epic (manifests in ProgramData), Battle.net, EA app, Ubisoft Connect, GOG, Xbox / `WindowsApps` (`XboxGames` folders). Games → info + "uninstall through launcher" action. Shader caches and leftover download folders → safe.

**Media / creator:** OBS recordings folder (from OBS config), Premiere/After Effects media cache and disk cache, DaVinci Resolve cache, CapCut cache → probably/safe for caches; recordings → careful (user content).

**Downloads folder intelligence:**
- Installers (`.exe`, `.msi`, `.msix`) older than N days → probably ("already installed?" check against installed apps by name/version when possible)
- Archives (`.zip`, `.7z`, `.rar`) **whose extracted folder exists beside them** (name match + content spot-check) → probably ("already extracted")
- ISO/VHD images → careful
- Duplicates of files elsewhere → link to the duplicates view
- `(1)`, `(2)` re-downloads → flagged

**Generic:**
- `*.tmp`, `~$*` Office owner files (only when not in use), `*.log` over N MB in app folders, `*.dmp`, `*.bak` → probably
- Files with zero bytes, empty folders → info (cleanup action available)
- Very old, untouched large files (configurable) → info "Old & large"

### 12.3 App attribution ("who owns this")
Build an **installed-apps catalog** at startup (and refresh on demand) from:
1. Registry Uninstall keys: `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall`, the WOW6432Node variant, and **per-user** `HKCU\...\Uninstall` (plus other loaded user hives when elevated). Read DisplayName, Publisher, InstallLocation, DisplayIcon, EstimatedSize, UninstallString, and InstallDate.
2. AppX/MSIX packages via the Windows package APIs (`PackageManager`): family name, install path, and data under `%LOCALAPPDATA%\Packages\<family>`.
3. Folder-name heuristics: `Program Files\<Vendor>\<App>`, `%LOCALAPPDATA%\<Vendor or App>`, `%APPDATA%\<App>`, `ProgramData\<App>`. Fuzzy-match against the catalog names and publishers.
4. Rule packs (explicit `app = ...`).
5. ETW observations ("this folder is only ever written by `claude.exe`") as supporting evidence, **with confidence scores**.

Every attribution carries a **confidence** (exact / high / heuristic) and the evidence list. The **Apps view** shows each app's total footprint = install dir + data + caches + logs + updates, broken down by location, with **"Uninstall" (launch its UninstallString, user-confirmed) and "Clean caches"** actions. Show apps whose estimated size (from the registry) differs wildly from the measured size, and leftover folders from **uninstalled** apps ("orphaned app data" → careful).

### 12.4 Content sniffing
For files with no or misleading extensions, sniff the first bytes (only when the user opens the detail panel, or in a background pass limited to large files) to detect the real type: zip, 7z, rar, PE executable, PDF, media containers, SQLite, safetensors/GGUF (AI model formats), VHD/VHDX, ISO. Show "Detected: GGUF model (claims .bin)".

### 12.5 Categories (top-level, color-coded)
System · Apps · Games · AI models · Dev & build · Caches · Temp · Downloads · Documents · Media (photo/video/audio) · Archives & disk images · Cloud placeholders · Recycle Bin · NTFS metadata · Unknown. Each has a fixed accessible color plus a pattern option for color-blind users.

---

## 13. Timestamps and "when was it touched"

- Show created, modified, accessed, and MFT-changed times. **Accessed times are unreliable:** read `fsutil behavior query disablelastaccess` (or the registry) and show a notice in the UI if last-access updates are disabled or system-managed. Never present accessed time as authoritative.
- Directory "last activity" = the newest modified time in its subtree (maintained aggregate).
- **Age heatmap color mode:** color treemap blocks by last-modified age (today → years).
- Filters: "modified in the last 24h / 7d / 30d", "untouched for 1y+", or a custom range.
- Time zone and DST correctness (FILETIME is UTC; display local). Clamp and flag absurd timestamps (before 1990 or in the future) as "suspicious timestamp" instead of crashing sorts.
- With ETW on, show "last writer: `process.exe` at time" when known.

---

## 14. Duplicates

- Pipeline:
  1. Group by logical size (ignore below a configurable minimum, default 1 MB; exclude 0-byte files).
  2. Partial hash (first + middle + last 64 KB, xxh3).
  3. Full hash (BLAKE3, streamed, parallel, I/O-throttled).
  4. Optional byte-compare before any delete.
- **Exclude hardlinks** (same file reference = same file, not a duplicate) and cloud placeholders (never trigger a hydration download!). Open files with flags that **don't recall cloud content**, and skip offline/recall attributes.
- Runs in the background, cancellable and resumable, with progress and ETA. Cache hashes keyed by (file reference, size, mtime) in SQLite, invalidated by USN changes.
- UI: groups sorted by wasted bytes, with a smart "keep" suggestion (oldest path / shortest path / not in Downloads / user rule). Bulk select with guardrails (you can never select every copy in a group for deletion).
- Optional advanced action: **replace duplicates with hardlinks** (same volume only, user-confirmed, with clear warnings that editing one changes all).

---

## 15. Cleanup and deletion (`strata-clean`): safety first

### 15.1 Safety tiers (from the classifier)
- **Safe:** regenerable, no user data.
- **Probably:** likely junk, review recommended.
- **Careful:** user data or large re-downloads; requires an extra confirmation.
- **Never:** the UI offers no delete action at all, only info and, where relevant, an official tool (Disk Cleanup, DISM, app uninstaller, launcher). **Also hard-blocked in the backend and the helper** (defense in depth).

Hard-coded never-delete list (in code, not only rules): volume roots, `C:\Windows` (except the specific temp/cache rules), `Program Files*` roots, `C:\Users` root and profile roots, `System Volume Information`, `$Recycle.Bin` root (emptying goes through the API), NTFS metadata, `pagefile.sys`/`hiberfil.sys`/`swapfile.sys`, the boot folders, anything flagged system+hidden at the top level, Strata's own install folder, and the user's known folders' roots (Documents, Desktop, Pictures, etc.; their *contents* can be deleted individually by explicit user choice).

### 15.2 Cleanup flow
1. User adds items to the **Cleanup Queue** (from any view, context menu, or a recommendation).
2. The queue shows totals by tier, with each item's explanation and safety badge.
3. **Review screen:** grouped list, expandable, with deselect. "Careful" items need a checkbox acknowledgment.
4. Choose a method: **Move to Recycle Bin (default)** or Delete permanently (extra confirmation, and disabled for items above N GB unless the user confirms again). Note: some volumes (removable, network) don't support the Recycle Bin; show that clearly before the action.
5. **Pre-flight check:** re-verify each item still exists and its path still resolves to the same file reference (TOCTOU), and check for locks (15.3).
6. Execute with progress and per-item results. Failures are listed with reasons and retry options.
7. Write the **undo log** (SQLite): what was deleted, from where, the method, size, and time. For Recycle Bin deletes, offer **"Restore"** (via Shell recycle bin item restore) within the app.

### 15.3 Locked files: "why can't I delete this?"
- Use the **Restart Manager** API (`RmStartSession`, `RmRegisterResources`, `RmGetList`) to name the processes holding a file or folder. Show "In use by: Discord.exe (PID 1234) — [Close app] [Skip]". Closing is a **polite** close (WM_CLOSE / RmShutdown with the user's consent), never a silent kill.
- Option "Delete on next reboot" (`MoveFileExW` with `MOVEFILE_DELAY_UNTIL_REBOOT`, helper-only, user-confirmed) for stubborn temp files.

### 15.4 Recycle Bin implementation
`IFileOperation` with `FOFX_RECYCLEONDELETE` (plus flags to avoid UI prompts, since we have our own), batched. Handle paths over 260 chars and items too big for the Recycle Bin (Windows will want to permanently delete them; **we must detect this and ask the user** instead of silently hard-deleting).

### 15.5 Running-app awareness
Warn before cleaning caches of running apps (browser and Electron app caches). Offer "Close X first."

### 15.6 Built-in tool actions (user-confirmed, output shown)
- Empty Recycle Bin (`SHEmptyRecycleBinW`)
- Launch Disk Cleanup (`cleanmgr`) / Storage Sense settings page
- DISM component cleanup (elevated)
- Open System Protection (shadow copy limits)
- Hibernation guidance
- Open app uninstaller
- `compact /compactos` info (show status; changing it is advanced and confirmed)

### 15.7 Privileged deletes via the helper (hardening)
- The helper accepts delete requests only as (volume, file reference number, expected path, expected size, expected mtime). It reopens **by file ID** and verifies all of them match before acting, which defeats path-swap and symlink-race attacks.
- Open with `FILE_FLAG_OPEN_REPARSE_POINT` so a reparse point itself is deleted, never its target.
- Enforce the never-list inside the helper independently of the UI.
- Directory deletes are recursive **in the helper, by handle**, re-checking each child. Never follow reparse points during recursion.
- Log every privileged action to the undo/audit log.

---

## 16. Views and visualization (`strata-layout` + `ui/`)

All views share selection, the current root, the size mode, color mode, and filters. Switching views keeps context.

### 16.1 Treemap (default, hero view)
- **Squarified treemap** layout in Rust, nested with padding and header strips for directories (name + size) when there's room.
- **Level of detail:** don't emit rects under ~1 device pixel; aggregate them into a hatched "N small items" block per directory. Increase depth as the user zooms.
- Rendering: WebGL2 instanced quads (position, size, color, flags) with optional **cushion shading** (classic SequoiaView/WinDirStat look, a toggle) and flat modern styling (default). Labels in a Canvas2D overlay only for rects above a size threshold, ellipsized, with collision culling.
- **Picking:** Rust provides a spatial index for hit-testing (or the GPU ID buffer); hover shows a rich tooltip (name, size in both modes, %, items, category, app, modified, safety). Under 1 ms.
- Interactions:
  - Click selects. Double-click (or Enter) drills in, with a smooth animated zoom. Backspace or mouse-back goes up. A breadcrumb bar can jump to any ancestor.
  - Wheel zooms around the cursor (visual zoom) without changing root.
  - Right-click opens a context menu: Open, Reveal in Explorer, Copy path, Properties (Windows dialog via `SHObjectProperties`), Add to cleanup, Show in list, Explain, Exclude from view, Open terminal here.
  - Drag-select multiple blocks (marquee) to add to the queue.
- **Color modes:** by category (default) / by file type / by age / by owning app / by safety tier / by "changed recently" (pulses when live updates hit).
- **Live animation:** blocks grow and shrink smoothly when USN changes arrive, and new blocks fade in, with "recently changed" highlighting that decays.
- High DPI and per-monitor DPI changes (re-layout on DPI change).

### 16.2 Other views
- **Sunburst:** radial hierarchy, drill by clicking a sector, with hover arcs.
- **Flame / icicle:** horizontal hierarchy bars.
- **Bubbles:** circle packing.
- **Mind map:** radial node-link tree for the current root, depth-limited and expandable.
- **List / table** (always available, split-pane with any visual view): virtualized tree-table with columns name, size (allocated/logical), % of parent (inline bar), items, modified, created, accessed, category, app, safety, attributes, and path. Sortable and resizable, with chooseable columns, keyboard navigable, and copy/export.
- **Largest files:** global top-N (configurable, default 1000) with filters.
- **File types:** breakdown by extension and detected type (bar + table), click to filter.
- **Apps:** section 12.3.
- **Categories:** totals per category with drill-through.
- **Timeline / History:** section 18.
- **Activity:** section 11 (if enabled).
- **Duplicates:** section 14.
- **Recommendations ("Free up space"):** ranked actionable findings with one-click add-to-queue: "Caches you can safely clear: 23.4 GB", "Stale node_modules (untouched 90+ days): 11.2 GB in 48 projects", "Downloads installers older than 30 days: 4.1 GB", "Recycle Bin: 6 GB", "Old Windows update files: run cleanup", "Duplicates: 3.3 GB wasted". Each recommendation is explainable and previewable.
- **Volumes overview (home screen):** every volume as a capacity bar (used/free, category-colored), filesystem badge, scan state (never / scanning / live / stale / partial), and a big **Scan** button. Shows the elevation state.

### 16.3 Detail panel (right side, for the selection)
Full path (copyable; long paths wrap), icon (Shell icon via `SHGetFileInfoW`, cached by extension), sizes (logical, allocated, ADS, compression ratio), all timestamps, attributes, reparse info + target, cloud state, hardlink names (all paths), ADS list, detected type, category + rule explanation, app attribution + confidence + evidence, safety tier + why, last writer (ETW), history sparkline for directories, and actions.

### 16.4 Global UI requirements
- **Dark and light themes,** following the Windows setting by default, with Mica/acrylic window backdrop where supported (Windows 11) and a solid fallback on Windows 10.
- Custom title bar with proper snap layouts support.
- Full **keyboard navigation**, visible focus, screen-reader labels on all controls, an accessible table alternative to every visual view, and color-blind-safe palettes + patterns.
- Responsive to window sizes down to 900×600; panels collapse.
- Number formatting: binary units by default (KiB/MiB/GiB, displayed as "KB/MB/GB" per Windows convention, with a setting for SI). Locale-aware separators.
- Every long operation shows progress, ETA where possible, and **Cancel**.
- Empty states, error states, and partial-result states are designed, not afterthoughts.
- No blocking modals during scans; the app stays interactive while scanning (progressively rendering the tree as data streams in).

---

## 17. Search (Everything-style)

- An instant search box (Ctrl+F / Ctrl+K command palette), searching names across all indexed volumes.
- Matching: substring (default), prefix, wildcard (`*.gguf`), regex (toggle), case-insensitive by default, and path-component search (`node_modules\react`).
- Filters syntax: `size:>1gb`, `ext:mp4`, `modified:<30d`, `app:claude`, `cat:cache`, `safe:yes`, `dir:` / `file:`, `vol:D`.
- Implementation: a parallel scan over the name buffer with SIMD-friendly substring search is enough for 5M names in tens of ms. Add a trigram index only if benchmarks miss the target.
- Results stream into a virtualized list with live re-query on typing (debounced ~30 ms). Selecting a result jumps the treemap to it.
- The command palette also runs actions ("Scan D:", "Open settings", "Empty Recycle Bin", "Show largest files").

---

## 18. History, snapshots and diffs (`strata-store`)

- After each full scan, and periodically while live (configurable, default daily), store a **snapshot of directory aggregates** (not every file): path hash → (allocated, logical, count) for directories above a minimum size, plus volume totals. Keep it compact.
- Retention: configurable (default 90 days, with daily snapshots thinned to weekly after 30 days).
- Views:
  - **Volume usage over time** (line chart).
  - **"What changed" diff between any two snapshots:** top grown and shrunk directories, new large folders, and deleted large folders.
  - **"Since last scan" banner** on the home screen: "+8.2 GB since Monday — biggest: `%LOCALAPPDATA%\...\models` +6.1 GB".
  - Per-directory sparkline in the detail panel.
- Database integrity: WAL mode, migrations versioned and tested, and corruption detection with "reset history" recovery that never crashes the app.

---

## 19. Settings

- Scan: default size mode, exclude paths (globs), include network drives, auto-scan on launch, auto-scan removable, fallback walker concurrency, follow-nothing policy display.
- Live: enable USN live updates (default on), update tick, auto-rescan on journal loss.
- Activity tracking (ETW): off by default, retention, CPU cap.
- Helper: on-demand vs. service mode (install/uninstall the service from settings, elevated).
- Cleanup: default method (Recycle Bin), large-delete thresholds, stale thresholds (node_modules days, installers days), duplicates minimum size.
- Appearance: theme, color mode default, treemap style (flat/cushion), units, compact density.
- Rules: view built-in rules (read-only), open the user rules folder, reload rules, test a path against the rules ("Why is this classified as X?").
- Data: clear history, clear activity data, clear caches, export settings.
- Startup: launch at login (off by default), start minimized to tray.
- Tray icon (optional): free space glance, "space dropped below X GB" notification (Windows toast), quick scan.
- About: version, licenses (generate third-party license list), update channel.

---

## 20. Licensing, distribution and updates

Per the product plan: paid one-time license, key activation. The key is purchasable on the website or inside the app; the website hosts the download.

- **License keys:** Ed25519-signed license payloads (email/ID, product, edition, issue date). Verify offline with an embedded public key; activation works offline. Optional online activation endpoint to bind a seat count. Keep the server contract minimal and documented in `docs/LICENSING.md`. Never block the app from viewing existing scan results because of a license-server outage.
- **Unlicensed behavior is a config flag** (decide with the owner before release and record it in `DECISIONS.md`: e.g., full scanning and visualization free, with cleanup/history/ETW requiring a license; or a time-limited trial). Implement the gating mechanism generically so the policy can change without refactors.
- **In-app purchase flow:** open the checkout URL in the default browser, then paste or auto-receive the key via a custom URL scheme (`strata://activate?key=...`; register the protocol in the installer and validate inputs strictly).
- **Installer:** per-machine install (needed for service mode), Start menu shortcut, uninstaller that removes the service, ETW session, protocol handler, and (optionally, asked) user data.
- **Auto-update:** signed manifests, background download, apply on restart, and roll back if the new version fails to start.
- **Signing:** sign every binary (app, helper, installer). The helper verifies the app's signature and vice versa (section 4).
- **Telemetry:** none by default. An optional, explicit opt-in crash reporting setting can exist, but scan data, paths, and file names are never transmitted.

---

## 21. Robustness: the edge-case checklist (each needs handling and a test)

- [ ] Paths > 260 chars, deep nesting (> 1000 levels), names with trailing dots/spaces, reserved device names, leading spaces, emoji, RTL characters, unpaired surrogates
- [ ] Case-sensitive directories (WSL) containing `A.txt` and `a.txt`
- [ ] Hardlinks across directories; hardlink count > 1000 (NTFS max 1024)
- [ ] Symlinks, junctions, and mount points creating apparent cycles
- [ ] Volume mounted in a folder of another volume
- [ ] Sparse files (logical 100 GB, allocated 1 MB)
- [ ] NTFS-compressed files and folders; WOF/CompactOS-compressed system files
- [ ] EFS-encrypted files
- [ ] Files with many ADS; huge ADS
- [ ] OneDrive Files On-Demand: online-only, locally available, and pinned; a scan must never trigger hydration downloads
- [ ] Fragmented `$MFT` with many runs; MFT records needing attribute lists; non-resident attribute lists
- [ ] 4Kn disks (4096-byte MFT records); cluster sizes from 512 B to 2 MB
- [ ] Corrupt or torn MFT records (fixup mismatch), `BAAD` records, orphans, parent cycles
- [ ] USN journal disabled / wrapped / ID changed / volume dismounted mid-tail
- [ ] Massive change bursts (500k files created in a minute)
- [ ] Files deleted or renamed during scan; directory replaced by a file with the same name
- [ ] Access denied folders (fallback walker), with partial totals clearly shown
- [ ] BitLocker-locked volume; volume unlocked while app is open
- [ ] Removable drive yanked mid-scan or mid-delete
- [ ] Network drive disconnect and timeout
- [ ] Sleep/hibernate and resume during scan and during live tailing
- [ ] Low memory (index on 50M-file synthetic volume): graceful lite mode, no OOM crash
- [ ] Antivirus slowing or blocking handle opens (the fallback walker must keep going)
- [ ] Multiple Windows user profiles; scanning another user's profile requires elevation (label clearly)
- [ ] Multiple app instances: single-instance enforcement (focus the existing window and pass arguments)
- [ ] Helper crash mid-scan; app crash mid-delete (the undo log must be consistent: write-ahead entries before acting)
- [ ] Clock changes / DST; timestamps from the future or before 1601
- [ ] High DPI, mixed-DPI multi-monitor, window moved between monitors
- [ ] Windows 10 vs. 11 differences (backdrop, APIs); ARM64 build parity
- [ ] Non-English Windows (localized folder display names; known-folder resolution via API, never hard-coded English names)
- [ ] User folders redirected (Documents on D:, OneDrive folder backup)
- [ ] Very small screens and window sizes; keyboard-only usage; screen reader pass
- [ ] Recycle Bin unavailable on the target volume; item too large for the Recycle Bin
- [ ] Deleting a file that becomes locked between pre-flight and action
- [ ] ReFS / Dev Drive volumes (fallback walker, plus USN V3 if available)
- [ ] exFAT/FAT32 USB drives (fallback, no live updates; manual rescan)

---

## 22. Testing strategy

- **Unit tests** for every crate. Parsers get exhaustive tests on hand-crafted byte buffers (fixups, runlists, attributes, every reparse tag, namespaces).
- **Fuzzing:** `cargo-fuzz` targets for MFT record parsing, runlist decoding, attribute lists, and USN record parsing. Run them in CI for a bounded time, with longer runs locally. Keep the corpus in the repo. Zero panics allowed.
- **Fixture volumes:** PowerShell scripts in `tests/fixtures/` that create and mount VHDX images (NTFS with 512 B and 4 KB clusters, ReFS if available, exFAT) and populate them with every edge case in section 21: hardlinks, junctions, symlinks, sparse, compressed, ADS, long paths, case-sensitive dirs, deep trees, and many small files. Golden expected outputs are stored as JSON (per-path logical/allocated, counts, flags). Tests compare the MFT scanner, the fallback walker, and the golden data, with all differences explained by rule.
- **Property tests** (proptest) for the index: random create/rename/delete sequences applied via the "live update" path must equal a fresh scan.
- **Live update soak test:** script heavy churn (npm install, unzip large archives, rename trees) while tailing. Assert that after quiescence the live index equals a fresh MFT scan.
- **Safety tests:** attempt to delete never-tier paths through the UI command, the backend API, and a raw helper pipe message. All must be refused. Race tests: swap a path for a junction between pre-flight and delete; the helper must refuse.
- **Layout tests:** treemap invariants (area proportional to size within tolerance, no overlaps, rects inside parent bounds, deterministic output), plus performance tests on 1M-node trees.
- **Frontend tests:** component tests (Vitest) and E2E via tauri-driver/WebdriverIO: scan a fixture volume, drill, search, queue, restore from the Recycle Bin.
- **Benchmarks** (criterion + a real-volume harness) for scan time, memory per entry, layout time, search latency, and USN apply throughput. Record them in `docs/BENCHMARKS.md`, including a WizTree comparison on the same machine.
- **CI:** GitHub Actions on `windows-latest` (x64) and an ARM64 build job. Run fmt, clippy, tests, bounded fuzz, frontend checks, and bundle builds. Fixture tests requiring VHDX mount run on a self-hosted runner or locally (document how).

---

## 23. Milestones (do them in order; each ends with all its acceptance criteria green)

- **M0 — Foundations.** Workspace, Tauri v2 shell, CI, lint config, docs skeleton, PROGRESS.md. App opens with a themed empty home screen. *Accept:* CI green on x64 + ARM64 builds.
- **M1 — MFT scanner CLI.** `strata-ntfs` + a `strata-cli scan C:` that prints totals and the top-50 largest. Fixups, runlists, attribute lists, hardlinks, reparse tags, ADS, WOF, sparse/compressed, metadata files, and orphans. *Accept:* fixture golden tests pass; fuzzers run clean; ≤3 s per 1M files; totals reconcile with the volume's used space (with explained gaps).
- **M2 — Fallback walker + reconciliation.** `strata-walk`. *Accept:* matches MFT results on fixtures within explained differences; access-denied handling; long paths.
- **M3 — Index + helper + IPC.** `strata-index`, `strata-helper` (on-demand elevation), the pipe protocol with security checks, streaming results into the app. *Accept:* the UI receives a full C: index without blocking; memory target met; helper security tests pass.
- **M4 — Treemap + list + detail panel.** WebGL treemap with LOD, drill, breadcrumbs, tooltips, context menu, split list view, detail panel, and volumes home screen. *Accept:* 60 fps on 1M+ entries; layout ≤50 ms per 100k; accessibility pass on list view.
- **M5 — Live updates.** USN tailing, coalescing, incremental aggregates, animations, cache file + catch-up on launch, and wrap/ID-change handling. *Accept:* soak test equality; ≤1 s reflection; near-zero idle CPU.
- **M6 — Classifier + app attribution.** Rule engine, full built-in rule packs (section 12.2), installed-apps catalog, Apps view, category view, color modes, and "why" explanations. *Accept:* every built-in rule has a fixture test; attribution confidence shown; Claude/AI/dev/browser/game coverage verified on the real machine.
- **M7 — Cleanup.** Queue, review, Recycle Bin/permanent delete, Restart Manager lock detection, undo log + restore, built-in tool actions, and helper-hardened privileged deletes. *Accept:* all safety tests pass, including race tests; no path in the never-list can be deleted by any route.
- **M8 — Search + command palette.** *Accept:* ≤50 ms on 5M entries; filter syntax fully working.
- **M9 — Other views.** Sunburst, flame, bubbles, mind map, largest files, file types, recommendations. *Accept:* shared selection and filters across views; performance targets.
- **M10 — Duplicates.** *Accept:* never hydrates cloud files; hardlinks excluded; resumable; guardrails enforced.
- **M11 — History + timeline.** *Accept:* snapshots, diffs, sparklines, retention, migrations tested.
- **M12 — ETW activity tracking.** *Accept:* attribution works for common apps; overhead under the cap; clean session lifecycle including crash recovery.
- **M13 — Settings, tray, notifications, service mode.** *Accept:* every setting works and persists; service install/uninstall is clean.
- **M14 — Licensing, installer, signing, updater.** *Accept:* signed installer installs and uninstalls cleanly (no leftovers unless chosen); offline activation works; update + rollback tested.
- **M15 — Polish + hardening.** Full section 21 checklist green, the accessibility pass, performance re-benchmarks, the WizTree comparison written up, third-party licenses, and user docs (`docs/USER_GUIDE.md`). *Accept:* zero known crashes, zero clippy warnings, all tests green, and every checkbox in this document satisfied.

---

## 24. Definition of done (the whole product)

Strata is done when, on a real Windows 11 x64 machine and on Windows 10, plus a working ARM64 build:

1. Scanning C: elevated produces a full, reconciled map in seconds, and the treemap is smooth.
2. Every byte of used space is either shown as a file or folder or explained in a named virtual block.
3. Live changes appear within a second, with no rescans, and survive restarts via cache + journal catch-up.
4. Any file or folder can be explained: what it is, which app owns it (with confidence), when it was touched, and whether it's safe to remove (and why).
5. Cleanup is safe by construction: Recycle Bin by default, restore works, locks are explained, and the never-list is unbreakable.
6. Search is instant; history shows what grew; duplicates are found without downloading cloud files.
7. The app is signed, installs and updates cleanly, collects no data, and passes every test, fuzz target, benchmark target, and checklist item in this spec.

Until all seven are true, keep going. Update `docs/PROGRESS.md` after every session.
