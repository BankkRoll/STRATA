# FAQ

## Why do sizes differ from Explorer?

Explorer's "Size" is the **logical** size: what files claim to contain. Strata shows
**allocated** size by default: the space a file actually occupies on disk. Switch the size mode
in the top bar to see logical sizes. The two differ for:

- **Small files.** Very small files live inside their MFT record and occupy no clusters;
  everything else rounds up to whole clusters.
- **Compression.** NTFS-compressed and CompactOS (WOF) files occupy less than their logical size.
- **Sparse files.** A 100 GB sparse file with 1 MB written occupies 1 MB.
- **Cloud files.** Online-only OneDrive files report their full logical size but occupy almost
  nothing locally.
- **Hardlinks.** One file can have several paths. Explorer counts it at every path; Strata
  counts it once and badges the other paths "Hardlink (counted elsewhere)".
- **Alternate data streams and folder indexes.** Strata counts both; Explorer counts neither.

The full rules are in [ARCHITECTURE.md](ARCHITECTURE.md#size-accounting).

## What is "Unaccounted / system reserved"?

The difference between the volume's used space, as Windows reports it, and the sum of everything
Strata found. Strata shows it as a named block instead of hiding it. Typical causes are shadow
copy storage that could not be measured, folders the standard scanner could not open, NTFS
metadata the standard scanner cannot see, and space NTFS reserves for the MFT. A fast (elevated)
scan sees all files and metadata, so the block is much smaller.

## What is "System Restore / Shadow copies"?

Space used by Volume Shadow Copies: restore points and previous versions, stored in
`System Volume Information`. Strata reads the used and maximum sizes from Windows (elevated only)
and opens System Protection settings, where you can lower the limit or delete restore points.
Strata never deletes them itself.

## Why does a fast scan need administrator rights?

A fast scan reads the NTFS Master File Table directly from the volume device instead of listing
every folder, and Windows allows raw volume reads only to administrators. Strata keeps the app
window unelevated and does this in a separate helper process. If you decline the UAC prompt, the
standard scanner runs instead. See [REQUIREMENTS.md](REQUIREMENTS.md#administrator-rights).

## Will scanning download my OneDrive files?

No. The fast scan reads file records from the MFT and never opens file contents. The standard
scanner opens files for attributes only, with a flag that forbids recalling cloud content. The
duplicate finder never opens cloud placeholders.

## Can Strata delete Windows files?

No. A never-delete list is built into the code, independent of the editable rule packs: the
Windows folder, Program Files roots, user profile roots, known-folder roots, boot files,
`pagefile.sys`, `hiberfil.sys`, NTFS metadata, `System Volume Information` and more. Items in the
`never` tier have no delete action, and the app backend and the elevated helper each refuse them
however the path is spelled (short names, junctions, volume GUID paths). Specific temp and cache
folders inside the Windows folder are the only exceptions.
See [ARCHITECTURE.md](ARCHITECTURE.md#deletion-safety).

## Where do deleted files go, and how do I restore them?

To the Recycle Bin by default. Every cleanup is recorded in Strata's undo log, and recycled items
can be restored from inside Strata or from the Recycle Bin. A restore never overwrites a file that
has since taken the original path. Permanent delete is a separate choice with an extra
confirmation. Some items cannot be recycled: drives without a Recycle Bin, items larger than the
Recycle Bin, and paths too long for the Shell. Strata detects these before acting and asks.

## Why does Windows SmartScreen warn about the installer?

SmartScreen warns about downloads that are unsigned or have little download reputation. If the
installer you downloaded from the project's
[GitHub Releases](https://github.com/BankkRoll/STRATA/releases/latest) page has no signature,
choose **More info → Run anyway**. Each release lists SHA-256 checksums in `SHA256SUMS.txt`. You
can also [build from source](../README.md#build-from-source).

## Why is WinSxS so big, and can I clean it?

`C:\Windows\WinSxS` is the component store. Most files in it are hardlinked into `System32` and
elsewhere, so Explorer counts the same bytes twice. Strata counts each file once, so WinSxS
usually appears much smaller than in Explorer. It is in the `never` tier: deleting from it breaks
Windows updates and repairs. To remove superseded components, run
`DISM /Online /Cleanup-Image /StartComponentCleanup` from an elevated terminal, or from
Strata's Tools view.

## What about hiberfil.sys and pagefile.sys?

`hiberfil.sys` holds memory contents for hibernation and Fast Startup. To remove it, disable
hibernation with `powercfg /h off` in an elevated terminal (this also turns off Fast Startup).
`pagefile.sys` and `swapfile.sys` are virtual memory; change their size in System Properties →
Advanced → Performance → Virtual memory. Strata never deletes any of them.

## Does Strata send my data anywhere?

No. See [PRIVACY.md](PRIVACY.md).
