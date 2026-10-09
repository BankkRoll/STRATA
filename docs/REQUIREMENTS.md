# Requirements

What Strata needs to run, what needs administrator rights, and what it needs to build.
For how the pieces fit together, see [ARCHITECTURE.md](ARCHITECTURE.md).

## System requirements

| Item | Requirement |
|---|---|
| OS | Windows 10 22H2 or Windows 11 |
| Architecture | x64 or ARM64, each with its own installer |
| Runtime | Microsoft Edge WebView2 Runtime. The installer downloads it from Microsoft if it is missing |
| Window backdrop | Mica on Windows 11 (build 22000 and later); solid background on Windows 10 |
| Display | Minimum window size 900 × 600 |

### Memory for the index

Strata keeps one in-memory index per scanned volume. Measured cost is about 59 bytes per entry
(files and folders) for the index structures, plus the names: about 16.5 bytes per entry in the
benchmark data, more for long or non-ASCII names.

| Entries | RAM for the index (estimate) |
|---|---|
| 500,000 | ~40 MB |
| 1,000,000 | ~75 MB |
| 5,000,000 | ~380 MB |

Estimates are extrapolated from the per-entry figures measured at 1M and 5M entries in
[BENCHMARKS.md](BENCHMARKS.md), where a real system volume had about 4.6 million entries. A
single volume is limited to about 4.29 billion entries (32-bit entry ids); Strata refuses a
larger volume instead of failing mid-scan.

History snapshots take about 12 bytes per stored folder. With the default settings (daily
snapshots, thinned to weekly after 30 days, kept 90 days), a volume with 50,000 recorded folders
uses roughly 24 MB (estimate).

## Administrator rights

The app window never runs elevated. Work that needs administrator rights runs in a separate
helper process ([ARCHITECTURE.md](ARCHITECTURE.md#process-model)). Declining the UAC prompt is
safe: Strata uses the standard scanner instead.

| Capability | Administrator | Why |
|---|---|---|
| Standard scan (directory walk) | No | Ordinary directory listings and attribute-only opens |
| Classification, search, history, all views | No | Run on the in-memory index |
| Cleanup of your own files (Recycle Bin or permanent) | No | Ordinary delete rights |
| Restore from the Recycle Bin | No | Moves your own Recycle Bin entries back |
| Fast scan (raw MFT read) | Yes | Opening a volume device (`\\.\C:`) for raw reads requires administrator rights |
| Live updates on NTFS (USN change journal) | Yes | Reading the journal and re-reading changed MFT records needs a volume handle |
| Activity tracking (ETW) | Yes | Windows lets only administrators start kernel trace sessions |
| Privileged deletes, delete on next restart | Yes | Items your account cannot delete, and `MOVEFILE_DELAY_UNTIL_REBOOT` |
| Shadow copy totals ("System Restore / Shadow copies") | Yes | The VSS WMI provider (`Win32_ShadowStorage`) refuses unelevated queries |
| Complete scans of other users' profiles | Yes | Their folders deny access to your account |
| DISM component cleanup | Yes | `DISM /Online` requires administrator rights |
| Service mode | Yes, to install | Installs the helper as an on-demand Windows service so fast scans need no UAC prompt per launch. Only accounts that are local administrators can use it |

Without elevation, folders the standard scanner cannot open are marked "Access denied" with
partial totals, and the gap appears in the volume's "Unaccounted / system reserved" block
([FAQ](FAQ.md#what-is-unaccounted--system-reserved)).

## Filesystems

| Filesystem | Unelevated | Elevated | Live updates |
|---|---|---|---|
| NTFS (fixed or removable) | Standard scanner | MFT scanner | USN change journal (elevated); folder watching after a standard scan |
| ReFS, including Dev Drive | Standard scanner | Standard scanner | None; rescan a folder or volume to refresh it |
| FAT32, exFAT | Standard scanner | Standard scanner | None; rescan a folder or volume to refresh it |
| Network shares (SMB), opt-in | Standard scanner | Standard scanner | None; rescan a folder or volume to refresh it |
| BitLocker volume, locked | Shown as locked; not scanned | Same | — |

- **Network drives are off by default** (Settings → Scanning → Include network drives). The
  scanner limits concurrency to 8 requests per share and gives each request a 30-second
  deadline, so a dead share cannot hang a scan. Mapped drives are enumerated from your own
  session, because an elevated process sees different drive mappings.
- **File identity.** Local NTFS and ReFS volumes use the filesystem's 64-bit file ids. Other
  volumes get synthetic ids, so their hardlinks are not merged, and items whose identity cannot
  be verified are not deleted until rescanned.
- **Cloud placeholders** (OneDrive and other cloud-files providers) are never downloaded by
  scanning. See the [FAQ](FAQ.md#will-scanning-download-my-onedrive-files).
- **Mount points** (a volume mounted in a folder) are boundaries: the inner volume is listed
  separately and never counted inside the outer one.

## Build requirements

| Tool | Version |
|---|---|
| Windows | 10 22H2 or 11; the code uses Win32 APIs and does not build for other platforms |
| Rust | Stable 1.90 or later (edition 2024) |
| Rust targets | `x86_64-pc-windows-msvc`; `aarch64-pc-windows-msvc` for ARM64 |
| MSVC | Visual Studio Build Tools, "Desktop development with C++" (plus the ARM64 tools for ARM64) |
| Node.js | 22 |
| pnpm | 10 |
| Tauri CLI | 2 (installed by `pnpm install`) |
| WebView2 | Runtime installed on the build machine, for `pnpm tauri dev` |

Build and test commands are in the [README](../README.md#build-from-source); release packaging
is in [RELEASING.md](RELEASING.md).
