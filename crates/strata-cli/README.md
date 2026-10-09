# strata-cli

Command-line front end for the MFT scanner. It prints scan stats, totals, the largest files and
a reconciliation of used space against the sum of allocations, and can write golden JSON for
fixture comparisons.

## Usage

```powershell
strata-cli scan C:                       # a drive: elevated terminal
strata-cli scan disk.img --json out.json # an NTFS image: any terminal
strata-cli --help
```

From source: `cargo run --release -p strata-cli -- scan C:`.

Options: `--json <file>`, `--top <N>`, `--chunk-mib <N>`, `--no-buffering`, `--sequential`,
`--mft-bitmap`.

Exit codes: `0` success, `1` failure, `2` not elevated (drive targets), `64` usage error.

## Code

- `args`: argument parsing (`Command`, `ScanArgs`, `Target`).
- `run`: opens the target, scans with `strata-ntfs`, prints the report.
- `golden`: the `strata-golden/1` JSON format (see [tests/fixtures](../../tests/fixtures)).
- `paths`: path reconstruction from parent references.

## Test

```powershell
cargo test -p strata-cli
```
