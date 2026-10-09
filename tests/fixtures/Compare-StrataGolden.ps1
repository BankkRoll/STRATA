#Requires -Version 5.1
<#
.SYNOPSIS
    Compares an MFT scan (strata-golden/1) with the filesystem's own view
    (strata-expected/1) and with a stored golden file.

.DESCRIPTION
    Every difference must be explained by one of the rules below; anything
    else fails the comparison.

    Rules, per path reported by the Win32 walk:
    - The path exists in the scan, with the same kind.
    - Files: logical size equal; link count equal; named streams equal (name
      and logical size); reparse tag equal when the walk reports one.
    - Allocated size:
        compressed or sparse  -> equals GetCompressedFileSizeW
        WOF (reparse hidden)  -> equals GetCompressedFileSizeW within one cluster
        scan reports 0        -> allowed when FILE_STANDARD_INFO reports at most
                                 one MFT record (resident data, which NTFS
                                 reports rounded to 8 bytes)
        otherwise             -> equals FILE_STANDARD_INFO.AllocationSize
    Paths only in the scan are allowed when flagged ntfs-metadata, or for the
    root itself. \System Volume Information and \$RECYCLE.BIN are skipped on
    both sides (ACL-protected, contents vary).

    Golden regression: the scan's entries (minus metadata and skipped trees)
    are normalized and compared with -Golden. -UpdateGolden (or a missing
    golden file) writes it instead.

.PARAMETER Expected
    strata-expected/1 file from Export-StrataExpected.ps1.

.PARAMETER Actual
    strata-golden/1 file from `strata-cli scan X: --json`.

.PARAMETER Golden
    Stored golden file to compare against (optional).

.PARAMETER UpdateGolden
    Write -Golden from -Actual instead of comparing.

.EXAMPLE
    .\Compare-StrataGolden.ps1 -Expected out\ntfs-4k.expected.json -Actual out\ntfs-4k.mft.json -Golden golden\ntfs-4k.json
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Expected,
    [Parameter(Mandatory)][string]$Actual,
    [string]$Golden,
    [switch]$UpdateGolden
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$COMPRESSED = 0x800
$SPARSE = 0x200
$skipPrefixes = @('\System Volume Information', '\$RECYCLE.BIN')

function Test-Skipped([string]$Path) {
    foreach ($p in $skipPrefixes) {
        if ($Path -eq $p -or $Path.StartsWith($p + '\', [StringComparison]::OrdinalIgnoreCase)) { return $true }
    }
    return $false
}

function Read-Json([string]$Path) {
    return Get-Content -LiteralPath $Path -Raw -Encoding utf8 | ConvertFrom-Json
}

$exp = Read-Json $Expected
$act = Read-Json $Actual
if ($exp.format -ne 'strata-expected/1') { throw "$Expected is not strata-expected/1" }
if ($act.format -ne 'strata-golden/1') { throw "$Actual is not strata-golden/1" }
$cluster = [int64]$act.volume.cluster_size
$recordSize = [int64]$act.volume.record_size

# NOTE: PowerShell hashtables ignore case; NTFS names (and case-sensitive
# directories) need ordinal keys.
$byPath = [Collections.Generic.Dictionary[string, object]]::new([StringComparer]::Ordinal)
foreach ($a in $act.entries) { $byPath[$a.path] = $a }
$seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)

$failures = [Collections.Generic.List[string]]::new()
$explained = [ordered]@{ resident = 0; compressed_or_sparse = 0; wof = 0; metadata_only_in_scan = 0; skipped = 0 }

foreach ($e in $exp.entries) {
    if (Test-Skipped $e.path) { $explained.skipped++; continue }
    if ($e.error) { $failures.Add("$($e.path): walk could not open it: $($e.error)"); continue }
    if (-not $byPath.ContainsKey($e.path)) { $failures.Add("$($e.path): missing from the MFT scan"); continue }
    $a = $byPath[$e.path]
    [void]$seen.Add($e.path)
    if ($a.kind -ne $e.kind) { $failures.Add("$($e.path): kind $($a.kind) vs $($e.kind)"); continue }
    if ($e.reparse_tag -and (-not $a.reparse -or $a.reparse.tag -ne $e.reparse_tag)) {
        $failures.Add("$($e.path): reparse tag $(if ($a.reparse) { $a.reparse.tag } else { 'none' }) vs $($e.reparse_tag)")
    }
    if ($e.kind -ne 'file') { continue }
    if ([int64]$a.logical -ne [int64]$e.logical) {
        $failures.Add("$($e.path): logical $($a.logical) vs $($e.logical)")
    }
    if ([int64]$a.link_count -ne [int64]$e.link_count) {
        $failures.Add("$($e.path): link count $($a.link_count) vs $($e.link_count)")
    }
    $mft = [int64]$a.allocated
    $isWof = $a.reparse -and $a.reparse.kind -eq 'wof'
    if (($e.attributes -band ($COMPRESSED -bor $SPARSE)) -ne 0) {
        if ($mft -ne [int64]$e.compressed_size) {
            $failures.Add("$($e.path): allocated $mft vs compressed size $($e.compressed_size)")
        } else { $explained.compressed_or_sparse++ }
    } elseif ($isWof) {
        if ([math]::Abs($mft - [int64]$e.compressed_size) -gt $cluster) {
            $failures.Add("$($e.path): WOF allocated $mft vs compressed size $($e.compressed_size)")
        } else { $explained.wof++ }
    } elseif ($mft -eq 0 -and [int64]$e.allocation_size -le $recordSize) {
        $explained.resident++
    } elseif ($mft -ne [int64]$e.allocation_size) {
        $failures.Add("$($e.path): allocated $mft vs AllocationSize $($e.allocation_size)")
    }
    $want = @($e.streams | ForEach-Object { '{0}={1}' -f $_.name, $_.logical } | Sort-Object -CaseSensitive)
    $got = @($a.ads | ForEach-Object { '{0}={1}' -f $_.name, $_.logical } | Sort-Object -CaseSensitive)
    if (($want -join '|') -ne ($got -join '|')) {
        $failures.Add("$($e.path): streams [$($got -join ', ')] vs [$($want -join ', ')]")
    }
}

foreach ($a in $act.entries) {
    if ($seen.Contains($a.path) -or $a.path -eq '\' -or (Test-Skipped $a.path)) { continue }
    if ($a.flags -contains 'ntfs-metadata') { $explained.metadata_only_in_scan++; continue }
    $failures.Add("$($a.path): only in the MFT scan")
}

if ($Golden) {
    $normalized = @($act.entries |
        Where-Object { $_.flags -notcontains 'ntfs-metadata' -and -not (Test-Skipped $_.path) } |
        ForEach-Object {
            [ordered]@{
                path          = $_.path
                kind          = $_.kind
                logical       = $_.logical
                allocated     = $_.allocated
                ads_logical   = $_.ads_logical
                ads_allocated = $_.ads_allocated
                dir_overhead  = $_.dir_overhead
                link_count    = $_.link_count
                flags         = ($_.flags -join ',')
                reparse       = $(if ($_.reparse) { $_.reparse.kind } else { $null })
                cloud         = $_.cloud
            }
        })
    if ($UpdateGolden -or -not (Test-Path -LiteralPath $Golden)) {
        # NOTE: an object wrapper, because Windows PowerShell 5.1's ConvertFrom-Json
        # returns a top-level array as one unenumerated object.
        [ordered]@{ format = 'strata-golden-normalized/1'; entries = $normalized } |
            ConvertTo-Json -Depth 5 -Compress | Set-Content -LiteralPath $Golden -Encoding utf8
        Write-Output "Golden written: $Golden ($($normalized.Count) entries)"
    } else {
        $old = (Read-Json $Golden).entries
        $key = { param($x) ($x.path, $x.kind, $x.logical, $x.allocated, $x.ads_logical, $x.ads_allocated,
                $x.dir_overhead, $x.link_count, $x.flags, $x.reparse, $x.cloud) -join [char]0x1F }
        $oldSet = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
        foreach ($o in $old) { [void]$oldSet.Add((& $key $o)) }
        $newSet = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
        foreach ($n in $normalized) { [void]$newSet.Add((& $key ([pscustomobject]$n))) }
        foreach ($k in $newSet) { if (-not $oldSet.Contains($k)) { $failures.Add("golden: new or changed entry $($k -replace [char]0x1F, ' | ')") } }
        foreach ($k in $oldSet) { if (-not $newSet.Contains($k)) { $failures.Add("golden: missing entry $($k -replace [char]0x1F, ' | ')") } }
    }
}

Write-Output ("Compared {0} walk entries with {1} scan entries." -f $exp.entries.Count, $act.entries.Count)
Write-Output ("Explained differences: " + (($explained.GetEnumerator() | ForEach-Object { "$($_.Key)=$($_.Value)" }) -join ', '))
if ($failures.Count -gt 0) {
    $failures | Select-Object -First 200 | ForEach-Object { Write-Output "FAIL $_" }
    if ($failures.Count -gt 200) { Write-Output "... and $($failures.Count - 200) more" }
    exit 1
}
Write-Output 'PASS'
exit 0
