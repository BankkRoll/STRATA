# Changelog

All notable changes to Strata are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Fast scanning of NTFS drives by reading the Master File Table directly, with a
  standard scanner for other file systems and for scans without administrator rights.
- An interactive treemap of every drive, plus sunburst, icicle, bubble and mind-map
  views, a sortable list and a detail panel for the selected item.
- Live updates from the NTFS change journal, so the map stays current without rescanning.
- Explanations for what files and folders are, which installed app owns them and
  whether they are safe to remove, driven by editable rule packs.
- Instant filename search with filters across the whole index.
- Safe cleanup: the Recycle Bin by default, lock detection that names the process
  holding a file, and a protected list of system locations that can never be deleted.
- Scan history with snapshots and diffs that show what grew over time.
- A per-machine installer for x64 and ARM64 Windows, background updates verified
  against the project's release key, and an offer to reinstall the previous version
  if an update fails to start. The installers are not code-signed, so Windows
  SmartScreen may ask you to confirm before running them.
