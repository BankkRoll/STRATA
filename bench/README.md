# Elevated benchmark

`verify-elevated.ps1` measures Strata on a real volume from an elevated
session and, when WizTree is installed, runs the same scan with WizTree for a
head-to-head.

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
- **strata-helper smoke test**: signature status, `--version` and `--help`.
  No service is installed.

Strata and WizTree run alternately, Strata first in each round. Run 1 is the
first of the session; caches are not dropped, so later runs are warm. Wall time
is process start to exit; peak memory is the process's peak working set.

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
| `-NoBuild` | off | Don't run cargo; use existing binaries. |
| `-TimeoutSec N` | `900` | Per-run timeout; a run past it is stopped and marked failed. |
| `-Yes` | off | Skip the confirmation prompt. |
| `-DryRun` | off | No elevation, builds or scans: checks discovery and writes a results-shaped JSON with no measurements to a temp folder. |

Binaries are built with `cargo build --release` (`strata-cli`, `strata-helper`
and the `walkbench` example), honoring `CARGO_TARGET_DIR`.

## Output

- `bench/results/<date>-<disk>.json` (e.g. `2026-01-31-nvme.json`): every run,
  summaries, generic machine description and a `comparison` block in the exact
  shape of `site/data/benchmarks.json`. Never overwritten; a suffix is added.
- On the console: the `comparison` block and a Markdown table for
  `docs/BENCHMARKS.md`.

Results are machine-specific and git-ignored. Copy numbers into the site or
docs deliberately.

## Safety and privacy

- Read-only: it deletes, moves or modifies nothing except files it created in
  its own fresh temp folder, which it removes at the end.
- It writes only to `bench/results/` and that temp folder (plus Cargo's target
  directory when building).
- No services, settings or system changes; no processes are left running.
- Hardware is described generically: core and thread counts, RAM size, disk
  type and Windows version. No model names, serials, computer or user names,
  paths or volume GUIDs are written; the JSON is scrubbed before it is saved.
