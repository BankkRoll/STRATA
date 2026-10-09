#Requires -Version 5.1
<#
.SYNOPSIS
    End-to-end fixture run: create VHDX volumes, populate edge cases, scan with
    strata-cli, compare with the filesystem's view and the stored goldens.

.DESCRIPTION
    For each configuration:
      1. New-StrataVhd creates and mounts a dynamic VHDX.
      2. Add-StrataEdgeCases.ps1 populates it (manifest records skips).
      3. Export-StrataExpected.ps1 records the Win32 view.
      4. NTFS only: strata-cli scan X: --json produces the MFT view, and
         Compare-StrataGolden.ps1 checks it against the Win32 view and
         golden\<config>.json.
      5. The VHDX is detached and deleted (unless -Keep).
    exFAT and ReFS volumes are populated and exported for the fallback
    walker; the MFT scanner rejects them by design.

    Must run elevated. See README.md.

.PARAMETER OutDir
    Working folder for VHDX files and JSON outputs (needs a few GB free).

.PARAMETER Cli
    Path to strata-cli.exe. Defaults to the release build in the cargo
    target directory ($env:CARGO_TARGET_DIR or <repo>\target).

.PARAMETER Config
    Which configurations to run (default: all).

.PARAMETER UpdateGolden
    Rewrite golden\<config>.json from this run instead of comparing.

.PARAMETER Keep
    Leave the VHDX files mounted for inspection.

.EXAMPLE
    .\Invoke-StrataFixtures.ps1 -OutDir D:\strata-fixtures -Config ntfs-4k
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$OutDir,
    [string]$Cli,
    [ValidateSet('ntfs-512', 'ntfs-4k', 'ntfs-64k', 'ntfs-2m', 'exfat', 'refs')]
    [string[]]$Config = @('ntfs-512', 'ntfs-4k', 'ntfs-64k', 'ntfs-2m', 'exfat', 'refs'),
    [switch]$UpdateGolden,
    [switch]$Keep
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'StrataFixtures.psm1') -Force
Assert-StrataElevated

$repo = Resolve-Path (Join-Path $PSScriptRoot '..\..')
if (-not $Cli) {
    $target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repo 'target' }
    $Cli = Join-Path $target 'release\strata-cli.exe'
}
if (-not (Test-Path -LiteralPath $Cli)) {
    throw "strata-cli not found at $Cli. Build it: cargo build --release -p strata-cli"
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$goldenDir = Join-Path $PSScriptRoot 'golden'
New-Item -ItemType Directory -Force -Path $goldenDir | Out-Null

$configs = @{
    'ntfs-512' = @{ FileSystem = 'NTFS'; ClusterSize = 512; SizeMB = 2048 }
    'ntfs-4k'  = @{ FileSystem = 'NTFS'; ClusterSize = 4096; SizeMB = 2048 }
    'ntfs-64k' = @{ FileSystem = 'NTFS'; ClusterSize = 65536; SizeMB = 4096 }
    'ntfs-2m'  = @{ FileSystem = 'NTFS'; ClusterSize = 2097152; SizeMB = 16384 }
    'exfat'    = @{ FileSystem = 'exFAT'; ClusterSize = 4096; SizeMB = 2048 }
    'refs'     = @{ FileSystem = 'ReFS'; ClusterSize = 4096; SizeMB = 4096 }
}

$results = [ordered]@{}
foreach ($name in $Config) {
    $c = $configs[$name]
    $vhd = Join-Path $OutDir "$name.vhdx"
    $manifest = Join-Path $OutDir "$name.manifest.json"
    $expected = Join-Path $OutDir "$name.expected.json"
    $actual = Join-Path $OutDir "$name.mft.json"
    $report = Join-Path $OutDir "$name.scan.txt"
    Write-Output "=== $name ($($c.FileSystem), $($c.ClusterSize)-byte clusters) ==="
    if (Test-Path -LiteralPath $vhd) { Remove-Item -LiteralPath $vhd }
    try {
        $letter = New-StrataVhd -Path $vhd -SizeMB $c.SizeMB -FileSystem $c.FileSystem `
            -ClusterSize $c.ClusterSize -Label 'STRATA'
    } catch {
        Write-Warning "$name skipped: cannot create the volume: $($_.Exception.Message)"
        $results[$name] = 'skipped (volume creation failed)'
        continue
    }
    $root = "$($letter):\"
    try {
        & (Join-Path $PSScriptRoot 'Add-StrataEdgeCases.ps1') -Root $root -ManifestPath $manifest
        & (Join-Path $PSScriptRoot 'Export-StrataExpected.ps1') -Root $root -OutFile $expected
        if ($c.FileSystem -ne 'NTFS') {
            $results[$name] = 'exported for the fallback walker (not NTFS)'
            continue
        }
        & $Cli scan "$($letter):" --json $actual --top 50 | Tee-Object -FilePath $report
        if ($LASTEXITCODE -ne 0) { throw "strata-cli exited with $LASTEXITCODE" }
        $compare = @{
            Expected = $expected
            Actual   = $actual
            Golden   = (Join-Path $goldenDir "$name.json")
        }
        if ($UpdateGolden) { $compare.UpdateGolden = $true }
        & (Join-Path $PSScriptRoot 'Compare-StrataGolden.ps1') @compare
        $results[$name] = if ($LASTEXITCODE -eq 0) { 'PASS' } else { 'FAIL' }
    } finally {
        if (-not $Keep) {
            Dismount-StrataVhd -Path $vhd
            Remove-Item -LiteralPath $vhd -ErrorAction SilentlyContinue
        }
    }
}

Write-Output ''
$results.GetEnumerator() | ForEach-Object { Write-Output ("{0,-10} {1}" -f $_.Key, $_.Value) }
if ($results.Values -contains 'FAIL') { exit 1 }
exit 0
