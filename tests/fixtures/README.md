# Fixture volumes

Real-volume tests for the MFT scanner: create VHDX volumes, fill them with every filesystem
edge case that can be created, scan them with `strata-cli`, and compare the result with the
filesystem's own view and with stored goldens. Synthetic-image tests in
`crates/strata-ntfs/tests` cover the parser byte by byte; these scripts cover what only a real
NTFS driver produces (allocation decisions, compression, WOF, `$Extend`, real fragmentation).

## Requirements

- An **elevated** PowerShell (Windows PowerShell 5.1 or PowerShell 7). Creating and mounting
  VHDX files and opening `\\.\X:` both need administrator rights.
- `New-VHD`/`Mount-VHD` (Hyper-V module) if available; otherwise `diskpart` is used, so Windows
  Home works too.
- A release build of the CLI:

  ```powershell
  # Optional: $env:CARGO_TARGET_DIR = 'E:\strata-target'   (default is <repo>\target)
  cargo build --release -p strata-cli
  ```

- A few GB free for the VHDX files (dynamic disks; the 2 MiB-cluster volume is the largest).
- Optional: the WSL feature (for the case-sensitive directory), Developer Mode or elevation
  (symlinks; elevation is already required), an edition with EFS (Pro and above).

## Run everything

```powershell
cd tests\fixtures
.\Invoke-StrataFixtures.ps1 -OutDir D:\strata-fixtures
```

This runs, for each configuration (`ntfs-512`, `ntfs-4k`, `ntfs-64k`, `ntfs-2m`, `exfat`,
`refs`):

1. `New-StrataVhd` (in `StrataFixtures.psm1`) creates, partitions, formats and mounts a VHDX.
2. `Add-StrataEdgeCases.ps1` populates it and writes `<config>.manifest.json`, recording every
   step as `created` or `skipped: <reason>` (e.g. no ADS on exFAT, no EFS on Home).
3. `Export-StrataExpected.ps1` walks the volume with the Win32 APIs and writes
   `<config>.expected.json` (`strata-expected/1`).
4. NTFS only: `strata-cli scan X: --json <config>.mft.json` (the text report goes to
   `<config>.scan.txt`), then `Compare-StrataGolden.ps1` compares the scan with the walk and with
   `golden\<config>.json`.
5. The VHDX is detached and deleted (`-Keep` leaves it mounted).

exFAT and ReFS are populated and exported for testing the standard scanner (`strata-walk`); the
MFT scanner rejects non-NTFS volumes by design.

Useful switches:

```powershell
.\Invoke-StrataFixtures.ps1 -OutDir D:\strata-fixtures -Config ntfs-4k -Keep
.\Invoke-StrataFixtures.ps1 -OutDir D:\strata-fixtures -UpdateGolden     # accept new goldens
```

Exit code 0 means every NTFS configuration passed.

## Run steps by hand

```powershell
Import-Module .\StrataFixtures.psm1
$x = New-StrataVhd -Path D:\f\ntfs.vhdx -SizeMB 2048 -FileSystem NTFS -ClusterSize 4096
.\Add-StrataEdgeCases.ps1 -Root "$($x):\" -ManifestPath D:\f\ntfs.manifest.json
.\Export-StrataExpected.ps1 -Root "$($x):\" -OutFile D:\f\ntfs.expected.json
strata-cli scan "$($x):" --json D:\f\ntfs.mft.json
.\Compare-StrataGolden.ps1 -Expected D:\f\ntfs.expected.json -Actual D:\f\ntfs.mft.json
Dismount-StrataVhd -Path D:\f\ntfs.vhdx
```

`Add-StrataEdgeCases.ps1` and `Export-StrataExpected.ps1` also work unelevated on an empty
folder of an existing NTFS volume, which is how they are smoke-tested without a VHDX.

## Edge cases created

| Step | What | Checks |
|---|---|---|
| `basic` | empty, resident, one-cluster and 5 MiB files | sizes, resident = 0 allocated |
| `hardlinks` | one file with three links in different directories | one entry per path, same record |
| `hardlinks-1024` | one file with 1024 links (NTFS maximum) | attribute lists, link count |
| `junction` | junction to a folder; junction to its own ancestor (apparent cycle) | not traversed |
| `symlinks` | file and directory symlinks | targets, not traversed |
| `sparse` | 100 GiB logical, 1 MiB written | allocated = 1 MiB |
| `ntfs-compression` | compressed file; compressed folder with an inheriting file | total-allocated |
| `wof-compression` | `compact /exe:lzx` file | `WofCompressedData` → allocated |
| `ads` | 200 small streams on one file, `Zone.Identifier`, one 64 MiB stream | ADS totals |
| `efs` | `cipher /e` file | encrypted flag, sizes |
| `long-paths` | file beyond 260 characters | path reconstruction |
| `deep-tree` | 1100 nested directories | depth handling |
| `odd-names` | trailing dot/space, leading space, `CON`, `nul.txt`, `COM1.log`, `AUX`, accents, emoji, RTL, CJK | names stored verbatim |
| `unpaired-surrogate` | names with lone high and low surrogates | lossless identity |
| `case-sensitive` | `A.txt` and `a.txt` in a case-sensitive directory | case never folded |
| `many-small-files` | 20,000 files in 20 directories | volume of records |

## Comparison rules (`Compare-StrataGolden.ps1`)

Every difference must be explained by a rule; anything else fails:

- Every walked path exists in the scan with the same kind.
- Files: logical size, link count, named streams (name and logical size) and reparse tag are
  equal.
- Allocated size: compressed or sparse files equal `GetCompressedFileSizeW`; WOF files equal it
  within one cluster; a scan value of 0 is accepted for resident files (NTFS reports their
  allocation rounded to 8 bytes, at most one MFT record); everything else equals
  `FILE_STANDARD_INFO.AllocationSize`.
- Scan-only paths are allowed when flagged `ntfs-metadata`, and for the root.
- `\System Volume Information` and `\$RECYCLE.BIN` are skipped on both sides.

The golden file (`golden\<config>.json`, `strata-golden-normalized/1`) stores the scan's
non-metadata entries; a later run must reproduce them exactly.

## Formats

`strata-golden/1` (written by `strata-cli --json`):

```json
{
  "format": "strata-golden/1",
  "source": "mft",
  "volume": { "cluster_size": 4096, "record_size": 1024, "total_bytes": 0,
              "used_bytes": 0, "other_attr_allocated": 0, "serial": "..." },
  "totals": { "files": 0, "dirs": 0, "logical": 0, "allocated": 0, "ads_logical": 0,
              "ads_allocated": 0, "dir_overhead": 0, "ntfs_metadata_allocated": 0,
              "hardlinked_files": 0, "reparse_points": 0 },
  "entries": [
    { "path": "\\links\\a\\original.bin", "record": 41, "kind": "file",
      "logical": 70000, "allocated": 73728, "ads_logical": 0, "ads_allocated": 0,
      "dir_overhead": 0, "link_index": 0, "link_count": 3,
      "flags": ["has-ads"], "reparse": { "kind": "symlink", "tag": "0xA000000C", "target": "..." },
      "cloud": "online-only", "ads": [ { "name": "Zone.Identifier", "logical": 26, "allocated": 0 } ] }
  ]
}
```

One entry per path: a file with N hardlinks has N entries with the same `record`;
`link_index` 0 is the link that carries the bytes (first-discovered policy). Paths
are volume-relative with `\` separators; unpaired surrogates are written as U+FFFD; a record
whose parent chain is broken is written as `<orphan>\name`. Entries are sorted by path, then
link index.

`strata-expected/1` (written by `Export-StrataExpected.ps1`): `path`, `kind`, `logical`,
`allocation_size`, `compressed_size`, `attributes`, `reparse_tag`, `link_count`, `streams`
(`name`, `logical`), `error`.

## Goldens

`golden\` holds one normalized golden file per NTFS configuration, written by an elevated run
with `-UpdateGolden`. Review the diff before committing new goldens: a changed golden means the
scanner's output changed.
