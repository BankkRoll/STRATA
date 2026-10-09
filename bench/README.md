# Elevated benchmark

`verify-elevated.ps1` measures Strata on a real volume from an elevated
session and compares it with WizTree (when installed) and with Windows File
Explorer.

## What it measures

Per volume (default `C:`):

- **Strata MFT scan** (`strata-cli scan X: --top 0`): wall time, peak working
  set, MFT records, files and directories, and the used-space reconciliation
  (used, accounted, unaccounted).
- **Standard scanner** (`walkbench` example of `strata-walk`): wall time, peak
  working set, entries and seconds per million entries. It runs in the same
  elevated session, so it is a reference point, not the unelevated experience.
- **WizTree** (if found under the registry Uninstall keys or Program Files,
  `WizTree64.exe` preferred): wall time and peak working set of a full scan via
  its command line (`/export=... /admin=1`, top-level folders only). WizTree is
  free for personal use; the script only measures it and deletes its export.
- **Windows File Explorer**: what a user does in Windows to see what fills a
  drive, timed. See [Explorer](#explorer) below.
- **strata-helper smoke test**: signature status, `--version` and `--help`.
  No service is installed.

The tools run alternately, Strata first in each round. Run 1 is the first of
the session; caches are not dropped, so later runs are warm. Wall time is
process start to exit; peak memory is the process's peak working set.

### Explorer

Properties on a drive root shows only the used and free space, so the script
does what a user does instead: **Select all** in the drive root, then
**Properties**. It opens that dialog through the shell
(`SHMultiFileProperties`, the same dialog and size count Explorer uses) on
exactly the items Select all picks, honouring the user's own *Show hidden
files* and *Hide protected operating system files* settings. It then reads the
dialog's **Size**, **Size on disk** and **Contains** fields with UI Automation
every 100 ms. The fields are found by their control IDs rather than their
labels, so this works in any display language.

- **Wall time** runs from opening the dialog to the last change of those
  fields. A total counts as settled once nothing has changed for 2 s; that
  window is not included.
- **Counts**: the size in bytes, the size on disk, and the file and folder
  counts the dialog shows are recorded for every run.
- **Peak memory** is not reported (`null`). The dialog is shell code running
  on a thread of a host process (normally `explorer.exe`, shared with the
  desktop), so no process peak belongs to the count alone.
- **Scope differs from Strata's.** Strata scans the entire volume; Explorer
  counts the selected top-level items, without the hidden ones unless the
  user's Explorer shows them. Every Explorer row in the results states both
  scopes and the number of items selected.
- The dialog is closed after each run (the same as **Cancel**, so nothing on
  it is applied). The script never stops or restarts `explorer.exe`.
- The dialog runs inside the elevated benchmark session, so it can count
  folders that an unelevated Explorer would skip or ask permission for.

Before measuring, the script checks the automation end to end on a few files
in its own temp folder. If the dialog can't be read (for example a Windows
build that lays it out differently), it times `cmd /c dir /s /a X:\` over the
entire volume instead and reports it as **Windows built-in (dir /s)**, never
as Explorer.

## Run

From the repository root, in PowerShell opened with **Run as administrator**:

```powershell
powershell -ExecutionPolicy Bypass -File bench\verify-elevated.ps1
```

It prints what it will do and asks once for confirmation. It refuses to run
unelevated (except with `-DryRun`).

| Parameter | Default | Meaning |
|---|---|---|
| `-Volume C:,D:` | `C:` | Volumes to scan (NTFS). The first names the results file. |
| `-Runs N` | `3` | Timed runs per tool and volume. |
| `-WalkRuns N` | `1` | Standard scanner runs per volume; `0` skips it. |
| `-WalkThreads N` | `0` | Standard scanner threads; `0` uses its default. |
| `-GoldenJson` | off | One extra `strata-cli --json` run per volume (time, memory, size; file deleted). |
| `-SkipWizTree` | off | Skip the WizTree comparison. |
| `-SkipExplorer` | off | Skip the Windows File Explorer comparison. |
| `-NoBuild` | off | Don't run cargo; use existing binaries. |
| `-TimeoutSec N` | `900` | Per-run timeout; a run past it is stopped and marked failed. |
| `-Yes` | off | Skip the confirmation prompt. |
| `-DryRun` | off | No elevation, builds or scans: checks discovery and writes a results-shaped JSON with no measurements to a temp folder. |
| `-ExplorerSelfTest <folder>` | off | Checks only the Explorer measurement, without elevation: Properties on the folder, then on Select all inside it, `-Runs` times each. Prints what it read; writes nothing. |

Binaries are built with `cargo build --release` (`strata-cli`, `strata-helper`
and the `walkbench` example), honoring `CARGO_TARGET_DIR`.

## Output

- `bench/results/<date>-<disk>.json` (e.g. `2026-01-31-nvme.json`): every run,
  summaries, generic machine description and a `comparison` block in the exact
  shape of `site/data/benchmarks.json`. Never overwritten; a suffix is added.
  Explorer adds two `comparison` rows per volume, *Total size of the drive*
  (median) and its first-of-session counterpart, each with Strata and Windows
  File Explorer side by side and both scopes in `detail`. It has no row in the
  peak memory comparison.
- On the console: the `comparison` block and a Markdown table for
  `docs/BENCHMARKS.md`.

Results are machine-specific and git-ignored. Copy numbers into the site or
docs deliberately.

## Safety and privacy

- Read-only: it deletes, moves or modifies nothing except files it created in
  its own fresh temp folder, which it removes at the end.
- It writes only to `bench/results/` and that temp folder (plus Cargo's target
  directory when building).
- No services, settings or system changes; no processes are left running and
  every Properties dialog it opens is closed.
- Hardware is described generically: core and thread counts, RAM size, disk
  type and Windows version. No model names, serials, computer or user names,
  paths or volume GUIDs are written; the JSON is scrubbed before it is saved.
