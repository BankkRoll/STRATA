#Requires -Version 5.1
<#
.SYNOPSIS
    Elevated verification and benchmark run for Strata: MFT scan, standard
    scanner, WizTree head-to-head and a helper smoke test.

.DESCRIPTION
    Run once from an elevated PowerShell. Measures, per volume:
      - strata-cli MFT scan: wall time, peak working set, records, files,
        directories and the used-space reconciliation (used vs. accounted).
      - the standard scanner (strata-walk walkbench example), for reference.
      - WizTree (if installed): wall time and peak working set of a full scan
        with its command-line export.
    Then writes a results file and prints a docs/BENCHMARKS.md table.

    READ-ONLY GUARANTEES
      - Never deletes, moves or modifies anything except files this script
        created itself, inside its own fresh temp folder, which is removed at
        the end.
      - Writes only to bench\results\ and that temp folder (plus Cargo's normal
        build output in the target directory when it builds the binaries).
      - Installs nothing, starts no services, changes no settings, and leaves
        no processes running: a timed-out child it started is stopped.
      - Results describe hardware generically (core/thread counts, RAM size,
        disk type). No model names, serials, computer name, user name, paths
        or volume GUIDs are written.

.PARAMETER Volume
    Volumes to scan, e.g. 'C:' or 'C:','D:'. The first one names the results file.

.PARAMETER Runs
    Timed runs per tool and volume. Run 1 is the first of the session
    (cold-ish: no cache is dropped); later runs are warm.

.PARAMETER WalkRuns
    Runs of the standard scanner per volume. 0 skips it.

.PARAMETER WalkThreads
    Thread count for the standard scanner. 0 uses its default.

.PARAMETER GoldenJson
    Also do one extra strata-cli run per volume with --json into the temp
    folder (reports time, memory and size; the file is then deleted).

.PARAMETER SkipWizTree
    Skip the WizTree comparison.

.PARAMETER NoBuild
    Don't invoke cargo; use existing binaries.

.PARAMETER TimeoutSec
    Per-run timeout. A run that exceeds it is stopped and recorded as failed.

.PARAMETER Yes
    Skip the Y/N confirmation.

.PARAMETER DryRun
    Skip elevation, builds and scans. Detects binaries, WizTree and hardware,
    and writes a results-shaped JSON (no measurements) into a temp folder.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File bench\verify-elevated.ps1

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File bench\verify-elevated.ps1 -Volume C:,D: -Runs 5
#>
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '', Justification = 'Interactive console report.')]
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingEmptyCatchBlock', '', Justification = 'Best-effort sampling and cleanup.')]
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSReviewUnusedParameter', 'TimeoutSec', Justification = 'Read in script scope by Invoke-Measured.')]
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseShouldProcessForStateChangingFunctions', '', Justification = 'New-* functions only build in-memory objects.')]
[CmdletBinding()]
param(
    [string[]]$Volume = @('C:'),
    [ValidateRange(1, 50)][int]$Runs = 3,
    [ValidateRange(0, 10)][int]$WalkRuns = 1,
    [ValidateRange(0, 1024)][int]$WalkThreads = 0,
    [switch]$GoldenJson,
    [switch]$SkipWizTree,
    [switch]$NoBuild,
    [ValidateRange(10, 7200)][int]$TimeoutSec = 900,
    [switch]$Yes,
    [switch]$DryRun
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

$SchemaVersion = 'strata-bench/1'
$RepoRoot = Split-Path -Parent $PSScriptRoot
$ResultsDir = Join-Path $PSScriptRoot 'results'

# -----------------------------------------------------------------------------
# Small helpers
# -----------------------------------------------------------------------------

function Write-Step([string]$Text) { Write-Host "`n== $Text" -ForegroundColor Cyan }
function Write-Note([string]$Text) { Write-Host "   $Text" }
function Write-Warn([string]$Text) { Write-Host "   ! $Text" -ForegroundColor Yellow }

function Test-Elevated {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    return ([Security.Principal.WindowsPrincipal]$id).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-Median([double[]]$Values) {
    if (-not $Values -or $Values.Count -eq 0) { return $null }
    $s = @($Values | Sort-Object)
    $m = [math]::Floor($s.Count / 2)
    if ($s.Count % 2) { return $s[$m] }
    return ($s[$m - 1] + $s[$m]) / 2
}

function ConvertTo-Rounded([object]$Value, [int]$Digits = 2) {
    if ($null -eq $Value -or "$Value" -eq '') { return $null }
    return [math]::Round([double]$Value, $Digits)
}

function ConvertTo-Drive([string]$Text) {
    if ($Text -notmatch '^\s*([A-Za-z]):?\\?\s*$') { throw "Not a drive letter: '$Text' (expected e.g. C:)" }
    return ($Matches[1].ToUpperInvariant() + ':')
}

# NOTE: reads the peak working set from the process handle after exit, which
# catches short spikes that polling misses. Polling is kept as a fallback.
if (-not ('StrataBench.Native' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace StrataBench {
    public static class Native {
        [StructLayout(LayoutKind.Sequential)]
        struct Counters {
            public uint cb; public uint PageFaultCount;
            public UIntPtr PeakWorkingSetSize; public UIntPtr WorkingSetSize;
            public UIntPtr QuotaPeakPagedPoolUsage; public UIntPtr QuotaPagedPoolUsage;
            public UIntPtr QuotaPeakNonPagedPoolUsage; public UIntPtr QuotaNonPagedPoolUsage;
            public UIntPtr PagefileUsage; public UIntPtr PeakPagefileUsage;
        }
        [DllImport("kernel32.dll", EntryPoint = "K32GetProcessMemoryInfo", SetLastError = true)]
        static extern bool GetProcessMemoryInfo(IntPtr process, ref Counters counters, uint cb);
        public static long PeakWorkingSet(IntPtr process) {
            var c = new Counters();
            c.cb = (uint)Marshal.SizeOf(typeof(Counters));
            if (!GetProcessMemoryInfo(process, ref c, c.cb)) return -1;
            return (long)c.PeakWorkingSetSize.ToUInt64();
        }
    }
}
'@
}

<#
Starts a process and waits for it, measuring wall time (process start to
exit) and peak working set. Optionally captures stdout/stderr. A process that
exceeds the timeout is stopped; this only ever touches the process started here.
#>
function Invoke-Measured {
    param(
        [Parameter(Mandatory)][string]$FilePath,
        [string]$Arguments = '',
        [switch]$Capture,
        [int]$Timeout = $TimeoutSec
    )
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $FilePath
    $psi.Arguments = $Arguments
    $psi.UseShellExecute = $false
    $psi.WorkingDirectory = $script:TempDir
    if ($Capture) {
        $psi.CreateNoWindow = $true
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
    }
    $p = New-Object System.Diagnostics.Process
    $p.StartInfo = $psi
    $sw = [Diagnostics.Stopwatch]::StartNew()
    [void]$p.Start()
    $script:Running = $p
    $handle = $p.Handle
    $outTask = $null; $errTask = $null
    if ($Capture) {
        $outTask = $p.StandardOutput.ReadToEndAsync()
        $errTask = $p.StandardError.ReadToEndAsync()
    }
    $sampled = 0L
    $timedOut = $false
    while (-not $p.WaitForExit(50)) {
        try { $p.Refresh(); if ($p.PeakWorkingSet64 -gt $sampled) { $sampled = $p.PeakWorkingSet64 } } catch { }
        if ($sw.Elapsed.TotalSeconds -gt $Timeout) {
            $timedOut = $true
            try { $p.Kill() } catch { }
            [void]$p.WaitForExit(10000)
            break
        }
    }
    $p.WaitForExit()
    $sw.Stop()
    $peak = [StrataBench.Native]::PeakWorkingSet($handle)
    if ($peak -le 0) { $peak = $sampled }
    $wall = $sw.Elapsed.TotalSeconds
    try { $wall = ($p.ExitTime - $p.StartTime).TotalSeconds } catch { }
    $result = [pscustomobject]@{
        ExitCode      = $(if ($timedOut) { $null } else { $p.ExitCode })
        TimedOut      = $timedOut
        WallSeconds   = $wall
        PeakWorkingSet = $peak
        StdOut        = $(if ($outTask) { $outTask.Result } else { '' })
        StdErr        = $(if ($errTask) { $errTask.Result } else { '' })
    }
    $script:Running = $null
    $p.Dispose()
    return $result
}

function Get-Match([string]$Text, [string]$Pattern, [int]$Group = 1) {
    $m = [regex]::Match($Text, $Pattern, 'Multiline')
    if ($m.Success) { return $m.Groups[$Group].Value }
    return $null
}

function ConvertTo-Long([string]$s) { if ($null -eq $s -or $s -eq '') { return $null }; return [long]$s }

# -----------------------------------------------------------------------------
# Discovery: binaries, WizTree, hardware
# -----------------------------------------------------------------------------

function Get-TargetDir {
    if ($env:CARGO_TARGET_DIR) {
        if ([IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) { return $env:CARGO_TARGET_DIR }
        return (Join-Path $RepoRoot $env:CARGO_TARGET_DIR)
    }
    return (Join-Path $RepoRoot 'target')
}

function Get-BinaryPath {
    $release = Join-Path (Get-TargetDir) 'release'
    return [pscustomobject]@{
        Cli       = Join-Path $release 'strata-cli.exe'
        Helper    = Join-Path $release 'strata-helper.exe'
        WalkBench = Join-Path $release 'examples\walkbench.exe'
    }
}

function Get-WizTree {
    $keys = @(
        'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*'
    )
    $dirs = New-Object System.Collections.Generic.List[string]
    foreach ($e in @(Get-ItemProperty $keys -ErrorAction SilentlyContinue | Where-Object { $_.PSObject.Properties['DisplayName'] -and $_.DisplayName -like 'WizTree*' })) {
        if ($e.PSObject.Properties['InstallLocation'] -and $e.InstallLocation) { $dirs.Add($e.InstallLocation.Trim('"')) }
        if ($e.PSObject.Properties['DisplayIcon'] -and $e.DisplayIcon) { $dirs.Add((Split-Path -Parent ($e.DisplayIcon -replace ',\d+$', '').Trim('"'))) }
    }
    foreach ($pf in @($env:ProgramFiles, ${env:ProgramFiles(x86)})) { if ($pf) { $dirs.Add((Join-Path $pf 'WizTree')) } }
    foreach ($name in @('WizTree64.exe', 'WizTree.exe')) {
        foreach ($d in $dirs) {
            $exe = Join-Path $d $name
            if (Test-Path -LiteralPath $exe -PathType Leaf) {
                $v = (Get-Item -LiteralPath $exe).VersionInfo
                $ver = if ($v.ProductVersion) { $v.ProductVersion.Trim() } else { $v.FileVersion }
                return [pscustomobject]@{ Path = $exe; Name = $name; Version = $ver }
            }
        }
    }
    return $null
}

function Get-DiskKind([string]$Drive) {
    $kind = [ordered]@{ mediaType = 'Unknown'; busType = 'Unknown'; short = 'unknown'; label = 'unknown disk' }
    try {
        $part = Get-Partition -DriveLetter $Drive.Substring(0, 1) -ErrorAction Stop
        $pd = @(Get-PhysicalDisk -ErrorAction Stop | Where-Object { "$($_.DeviceId)" -eq "$($part.DiskNumber)" })[0]
        if ($pd) {
            $kind.mediaType = "$($pd.MediaType)"
            $kind.busType = "$($pd.BusType)"
        }
    } catch { }
    switch -Regex ("$($kind.busType)|$($kind.mediaType)") {
        '^NVMe\|' { $kind.short = 'nvme'; $kind.label = 'NVMe SSD'; break }
        '^SATA\|SSD' { $kind.short = 'sata-ssd'; $kind.label = 'SATA SSD'; break }
        '\|SSD$' { $kind.short = 'ssd'; $kind.label = "$($kind.busType) SSD"; break }
        '\|HDD$' { $kind.short = 'hdd'; $kind.label = 'HDD'; break }
        default { $kind.short = ("$($kind.busType)-$($kind.mediaType)").ToLowerInvariant() -replace '[^a-z0-9-]', ''; $kind.label = "$($kind.busType) $($kind.mediaType) disk" }
    }
    return $kind
}

function Get-Hardware {
    $cpus = @(Get-CimInstance Win32_Processor)
    $cores = ($cpus | Measure-Object -Property NumberOfCores -Sum).Sum
    $threads = ($cpus | Measure-Object -Property NumberOfLogicalProcessors -Sum).Sum
    $mem = (Get-CimInstance Win32_PhysicalMemory | Measure-Object -Property Capacity -Sum).Sum
    if (-not $mem) { $mem = (Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory }
    $ramGb = [int][math]::Round($mem / 1GB)
    $cs = Get-CimInstance Win32_ComputerSystem
    $form = if ($cs.PCSystemType -eq 2) { 'laptop' } else { 'desktop' }
    $os = Get-CimInstance Win32_OperatingSystem
    $build = [int]$os.BuildNumber
    $osName = if ($build -ge 22000) { 'Windows 11' } else { 'Windows 10' }
    return [ordered]@{
        formFactor = $form
        cpuCores   = [int]$cores
        cpuThreads = [int]$threads
        ramGB      = $ramGb
        os         = $osName
        osBuild    = $build
    }
}

function Get-VolumeInfo([string]$Drive) {
    $v = Get-Volume -DriveLetter $Drive.Substring(0, 1) -ErrorAction Stop
    $sysDrive = ($env:SystemDrive).ToUpperInvariant()
    return [ordered]@{
        volume      = $Drive
        system      = ($Drive -eq $sysDrive)
        fileSystem  = "$($v.FileSystemType)"
        sizeGiB     = ConvertTo-Rounded ($v.Size / 1GB) 1
        usedGiB     = ConvertTo-Rounded (($v.Size - $v.SizeRemaining) / 1GB) 1
        disk        = Get-DiskKind $Drive
    }
}

function Get-StrataVersion {
    $ver = $null
    $toml = Join-Path $RepoRoot 'Cargo.toml'
    if (Test-Path -LiteralPath $toml) {
        $ver = Get-Match (Get-Content -LiteralPath $toml -Raw) '^\s*version\s*=\s*"([^"]+)"'
    }
    $commit = $null; $dirty = $null
    if (Get-Command git -ErrorAction SilentlyContinue) {
        $commit = (& git -C $RepoRoot rev-parse --short HEAD 2>$null)
        if ($LASTEXITCODE -eq 0) { $dirty = [bool](& git -C $RepoRoot status --porcelain --untracked-files=no 2>$null) } else { $commit = $null }
    }
    return [ordered]@{ version = $(if ($ver) { "v$ver" } else { $null }); commit = $commit; dirty = $dirty }
}

# -----------------------------------------------------------------------------
# Measurements
# -----------------------------------------------------------------------------

<# Parses strata-cli's text report (crates/strata-cli/src/report.rs). #>
function ConvertFrom-StrataReport([string]$Text) {
    $used = ConvertTo-Long (Get-Match $Text '^\s+used \([^)]*\)\s.*\((\d+) bytes\)\s*$')
    $acc = ConvertTo-Long (Get-Match $Text '^\s+sum allocated \(records\)\s.*\((\d+) bytes\)\s*$')
    $gap = ConvertTo-Long (Get-Match $Text '^\s+Unaccounted\s.*\((-?\d+) bytes\)\s*$')
    $problems = [regex]::Match($Text, '^\s+problems\s+(\d+) BAAD, (\d+) torn, (\d+) bad signature, (\d+) malformed, (\d+) unreadable', 'Multiline')
    $skipped = $null
    if ($problems.Success) { $skipped = 0L; foreach ($i in 1..5) { $skipped += [long]$problems.Groups[$i].Value } }
    return [ordered]@{
        scanSeconds        = ConvertTo-Rounded (Get-Match $Text '^\s+elapsed\s+([\d.]+) s') 3
        recordsTotal       = ConvertTo-Long (Get-Match $Text '^\s+records\s+(\d+) total')
        recordsInUse       = ConvertTo-Long (Get-Match $Text '^\s+records\s+\d+ total, (\d+) in use')
        recordsEmitted     = ConvertTo-Long (Get-Match $Text '^\s+emitted\s+(\d+) records')
        recordsPerSecond   = ConvertTo-Long (Get-Match $Text '^\s+throughput\s+(\d+) records/s')
        recordsSkipped     = $skipped
        files              = ConvertTo-Long (Get-Match $Text '^\s+files\s+(\d+)\s*$')
        dirs               = ConvertTo-Long (Get-Match $Text '^\s+directories\s+(\d+)\s*$')
        usedBytes          = $used
        accountedBytes     = $acc
        unaccountedBytes   = $gap
        unaccountedPercent = $(if ($used -and $null -ne $gap) { ConvertTo-Rounded (100.0 * $gap / $used) 3 } else { $null })
        cancelled          = ($Text -match 'CANCELLED')
    }
}

function Invoke-StrataScan([string]$Cli, [string]$Drive, [int]$Run, [string]$JsonOut) {
    $argLine = "scan $Drive --top 0"
    if ($JsonOut) { $argLine += " --json `"$JsonOut`"" }
    $r = Invoke-Measured -FilePath $Cli -Arguments $argLine -Capture
    $row = [ordered]@{ run = $Run; firstOfSession = ($Run -eq 1); ok = ($r.ExitCode -eq 0) }
    $row.wallSeconds = ConvertTo-Rounded $r.WallSeconds 3
    $row.peakWorkingSetMiB = ConvertTo-Rounded ($r.PeakWorkingSet / 1MB) 1
    foreach ($kv in (ConvertFrom-StrataReport $r.StdOut).GetEnumerator()) { $row[$kv.Key] = $kv.Value }
    if (-not $row.ok) {
        $row.error = $(if ($r.TimedOut) { "timed out after $TimeoutSec s" } else { "exit code $($r.ExitCode): $(($r.StdErr -split "`n")[0].Trim())" })
    }
    return $row
}

function Invoke-WalkBench([string]$Exe, [string]$Drive, [int]$Run) {
    $argLine = "$Drive\ dirinfo 1"
    if ($WalkThreads -gt 0) { $argLine += " $WalkThreads" }
    $r = Invoke-Measured -FilePath $Exe -Arguments $argLine -Capture
    $o = $r.StdOut
    $kv = { param($k) Get-Match $o "(?:^|\s)$k=([^\s]+)" }
    $row = [ordered]@{ run = $Run; ok = ($r.ExitCode -eq 0) }
    $row.wallSeconds = ConvertTo-Rounded $r.WallSeconds 3
    $row.peakWorkingSetMiB = ConvertTo-Rounded ($r.PeakWorkingSet / 1MB) 1
    $row.threads = ConvertTo-Long (& $kv 'threads')
    $row.walkSeconds = ConvertTo-Rounded (& $kv 'secs') 3
    $row.files = ConvertTo-Long (& $kv 'files')
    $row.dirs = ConvertTo-Long (& $kv 'dirs')
    $row.entriesPerSecond = ConvertTo-Long (& $kv 'entries_per_sec')
    $row.secondsPer1MEntries = ConvertTo-Rounded (& $kv 'secs_per_1M_entries') 2
    $row.accessDeniedDirs = ConvertTo-Long (& $kv 'access_denied_dirs')
    $row.allocatedGiB = ConvertTo-Rounded (& $kv 'allocated_gib') 2
    $row.volumeUsedGiB = ConvertTo-Rounded (& $kv 'volume_used_gib') 2
    if (-not $row.ok) {
        $row.error = $(if ($r.TimedOut) { "timed out after $TimeoutSec s" } else { "exit code $($r.ExitCode)" })
    }
    return $row
}

function Invoke-WizTreeScan([object]$Wiz, [string]$Drive, [int]$Run) {
    $csv = Join-Path $script:TempDir "wiztree-$($Drive.Substring(0,1))-$Run.csv"
    # WizTree 4.x documented command line: scan, export a CSV, exit. Only the
    # top-level folder rows are exported so export cost stays negligible next
    # to the scan itself; WizTree still scans the whole volume.
    $argLine = "`"$Drive`" /export=`"$csv`" /admin=1 /exportfiles=0 /exportfolders=1 /exportmaxdepth=1"
    $r = Invoke-Measured -FilePath $Wiz.Path -Arguments $argLine
    $exported = Test-Path -LiteralPath $csv -PathType Leaf
    $row = [ordered]@{ run = $Run; firstOfSession = ($Run -eq 1); ok = ($r.ExitCode -eq 0 -and $exported) }
    $row.wallSeconds = ConvertTo-Rounded $r.WallSeconds 3
    $row.peakWorkingSetMiB = ConvertTo-Rounded ($r.PeakWorkingSet / 1MB) 1
    if (-not $row.ok) {
        $row.error = $(if ($r.TimedOut) { "timed out after $TimeoutSec s" } elseif (-not $exported) { "no export written (exit code $($r.ExitCode))" } else { "exit code $($r.ExitCode)" })
    }
    if ($exported) { Remove-Item -LiteralPath $csv -Force }
    return $row
}

function Get-Summary([object[]]$Rows) {
    $ok = @($Rows | Where-Object { $_.ok })
    if ($ok.Count -eq 0) {
        return [ordered]@{ okRuns = 0; medianWallSeconds = $null; minWallSeconds = $null; maxWallSeconds = $null; firstRunWallSeconds = $null; medianPeakWorkingSetMiB = $null }
    }
    $wall = [double[]]@($ok | ForEach-Object { $_.wallSeconds })
    $mem = [double[]]@($ok | ForEach-Object { $_.peakWorkingSetMiB })
    $first = @($ok | Where-Object { $_.Contains('firstOfSession') -and $_.firstOfSession })
    return [ordered]@{
        okRuns               = $ok.Count
        medianWallSeconds    = ConvertTo-Rounded (Get-Median $wall) 3
        minWallSeconds       = ConvertTo-Rounded (($wall | Measure-Object -Minimum).Minimum) 3
        maxWallSeconds       = ConvertTo-Rounded (($wall | Measure-Object -Maximum).Maximum) 3
        firstRunWallSeconds  = $(if ($first.Count) { $first[0].wallSeconds } else { $null })
        medianPeakWorkingSetMiB = ConvertTo-Rounded (Get-Median $mem) 1
    }
}

# -----------------------------------------------------------------------------
# Output shaping
# -----------------------------------------------------------------------------

function Format-EntryCount($n) {
    if ($null -eq $n) { return 'n/a entries' }
    if ($n -ge 1e6) { return ('{0:0.0}M entries' -f ($n / 1e6)) }
    return ('{0:0}k entries' -f ($n / 1e3))
}

<# Builds site/data/benchmarks.json "comparison" (site/build/benchmarks.ts). #>
function New-Comparison($Results, [string]$MachineText, [string]$MeasuredOn) {
    $items = New-Object System.Collections.Generic.List[object]
    foreach ($v in $Results.volumes) {
        $s = $v.strata.summary
        $w = $v.wiztree
        $entries = $null
        $okRun = @($v.strata.runs | Where-Object { $_.ok })
        if ($okRun.Count) { $entries = $okRun[0].files + $okRun[0].dirs }
        $where = $(if ($v.volume.system) { 'system volume' } else { "$($v.volume.disk.label) volume" })
        $detail = "$(Format-EntryCount $entries), $($v.volume.disk.label), median of $Runs runs"
        $hasWiz = ($w -and ($w.summary.okRuns -gt 0 -or $DryRun))
        $tools = { param($strataVal, $wizVal)
            $vals = @([ordered]@{ tool = 'Strata'; version = $Results.strata.version; value = $strataVal })
            if ($hasWiz) { $vals += [ordered]@{ tool = 'WizTree'; version = $w.version; value = $wizVal } }
            , $vals
        }
        # NOTE: a dry run emits the rows with null values so the shape can be checked.
        if ($s.okRuns -gt 0 -or $DryRun) {
            $items.Add([ordered]@{ metric = "Full scan, $where"; detail = $detail; unit = 's'; lowerIsBetter = $true
                values = (& $tools $s.medianWallSeconds $(if ($hasWiz) { $w.summary.medianWallSeconds })) })
            $items.Add([ordered]@{ metric = "First scan of the session, $where"; detail = "$(Format-EntryCount $entries), no cache dropped, run 1 of $Runs"; unit = 's'; lowerIsBetter = $true
                values = (& $tools $s.firstRunWallSeconds $(if ($hasWiz) { $w.summary.firstRunWallSeconds })) })
            $items.Add([ordered]@{ metric = "Peak memory, $where"; detail = "peak working set, $(Format-EntryCount $entries), median of $Runs runs"; unit = 'MiB'; lowerIsBetter = $true
                values = (& $tools $s.medianPeakWorkingSetMiB $(if ($hasWiz) { $w.summary.medianPeakWorkingSetMiB })) })
        }
    }
    return [ordered]@{
        release     = $Results.strata.version
        measuredOn  = $MeasuredOn
        machine     = $MachineText
        methodology = @(
            'Same machine, same volume, elevated session for every tool; tools run alternately, Strata first in each round.',
            "Wall-clock time from process start to exit with a complete size total; median of $Runs runs. Run 1 is the first of the session; no cache was dropped and there was no reboot between runs.",
            'Strata: strata-cli MFT scan (reads the MFT unbuffered, so the OS file cache does not help it). WizTree: command-line scan with a minimal CSV export (top-level folders only).',
            'Peak memory is the peak working set of the measured process.',
            'Each tool at the version listed, default settings.'
        )
        results     = $items.ToArray()
    }
}

function New-Markdown($Results) {
    $lines = New-Object System.Collections.Generic.List[string]
    $lines.Add("Measured $($Results.measuredOn) on $($Results.machine.summary). Strata $($Results.strata.version) ($($Results.strata.commit)). Median of $Runs runs; first run of the session listed separately, no cache dropped.")
    $lines.Add('')
    $lines.Add('| Volume | Tool | Median wall | First run | Peak working set | Entries |')
    $lines.Add('|---|---|---:|---:|---:|---:|')
    $fmtS = { param($x) if ($null -eq $x) { 'n/a' } else { '{0:0.00} s' -f $x } }
    $fmtM = { param($x) if ($null -eq $x) { 'n/a' } else { '{0:0} MiB' -f $x } }
    foreach ($v in $Results.volumes) {
        $vol = "$($v.volume.volume) ($($v.volume.disk.label))"
        $ok = @($v.strata.runs | Where-Object { $_.ok })
        $entries = if ($ok.Count) { '{0:N0}' -f ($ok[0].files + $ok[0].dirs) } else { 'n/a' }
        $s = $v.strata.summary
        $lines.Add("| $vol | Strata (MFT) | $(& $fmtS $s['medianWallSeconds']) | $(& $fmtS $s['firstRunWallSeconds']) | $(& $fmtM $s['medianPeakWorkingSetMiB']) | $entries |")
        if ($v.wiztree) {
            $w = $v.wiztree.summary
            $lines.Add("| $vol | WizTree $($v.wiztree.version) | $(& $fmtS $w['medianWallSeconds']) | $(& $fmtS $w['firstRunWallSeconds']) | $(& $fmtM $w['medianPeakWorkingSetMiB']) | n/a |")
        }
        if ($v.walker) {
            $wk = $v.walker.summary
            $okw = @($v.walker.runs | Where-Object { $_.ok })
            $we = if ($okw.Count) { '{0:N0}' -f ($okw[0].files + $okw[0].dirs) } else { 'n/a' }
            $lines.Add("| $vol | Strata (standard scanner) | $(& $fmtS $wk['medianWallSeconds']) | n/a | $(& $fmtM $wk['medianPeakWorkingSetMiB']) | $we |")
        }
    }
    foreach ($v in $Results.volumes) {
        $ok = @($v.strata.runs | Where-Object { $_.ok })
        if ($ok.Count) {
            $r = $ok[$ok.Count - 1]
            $gib = { param($b) if ($null -eq $b) { 'n/a' } else { '{0:N2} GiB' -f ($b / 1GB) } }
            $lines.Add('')
            $lines.Add("Reconciliation on $($v.volume.volume): used $(& $gib $r.usedBytes), accounted $(& $gib $r.accountedBytes), unaccounted $(& $gib $r.unaccountedBytes) ($($r.unaccountedPercent)% of used).")
        }
    }
    return ($lines -join "`n")
}

<#
SECURITY: last line of defence before anything is written. Replaces absolute
paths, volume GUIDs, and the user and computer names, then re-parses the JSON.
#>
function Protect-Json([string]$Json) {
    $j = $Json
    $j = [regex]::Replace($j, '\\\\\\\\\?\\\\Volume\{[0-9A-Fa-f-]+\}[^"]*', '[volume]')
    $j = [regex]::Replace($j, 'Volume\{[0-9A-Fa-f-]{36}\}', '[volume]')
    $j = [regex]::Replace($j, '[A-Za-z]:\\\\[^"]*', '[path]')
    $j = [regex]::Replace($j, '\\\\\\\\[^"\\]+\\\\[^"]*', '[path]')
    foreach ($secret in @($env:USERNAME, $env:COMPUTERNAME, $env:USERDOMAIN)) {
        if ($secret -and $secret.Length -ge 3) { $j = [regex]::Replace($j, [regex]::Escape($secret), '[redacted]', 'IgnoreCase') }
    }
    [void]($j | ConvertFrom-Json)
    return $j
}

function Write-Utf8([string]$Path, [string]$Text) {
    [IO.File]::WriteAllText($Path, $Text, (New-Object Text.UTF8Encoding $false))
}

# -----------------------------------------------------------------------------
# Main
# -----------------------------------------------------------------------------

$drives = @($Volume | ForEach-Object { $_ -split ',' } | Where-Object { $_.Trim() } | ForEach-Object { ConvertTo-Drive $_ } | Select-Object -Unique)
$elevated = Test-Elevated

if (-not $elevated -and -not $DryRun) {
    Write-Host 'Strata elevated verification: this script must run as administrator.' -ForegroundColor Red
    Write-Host 'MFT scanning and the WizTree comparison both read the raw volume.'
    Write-Host 'Open PowerShell with "Run as administrator", then run:'
    Write-Host '  powershell -ExecutionPolicy Bypass -File bench\verify-elevated.ps1'
    Write-Host '(Use -DryRun to check discovery and output shaping without elevation.)'
    exit 2
}

$bins = Get-BinaryPath
$wiz = if ($SkipWizTree) { $null } else { Get-WizTree }
$cargo = Get-Command cargo -ErrorAction SilentlyContinue

Write-Host 'Strata elevated verification and benchmark' -ForegroundColor Cyan
if ($DryRun) { Write-Warn 'DRY RUN: no elevation needed, no builds, no scans, no measurements.' }
Write-Host ''
Write-Host 'This will:'
if (-not $NoBuild -and -not $DryRun) { Write-Host '  1. cargo build --release -p strata-cli -p strata-helper, and the strata-walk walkbench example' }
Write-Host "  2. Scan $($drives -join ', ') with strata-cli (MFT), $Runs timed run(s) each, recording wall time, peak memory and reconciliation"
if ($WalkRuns -gt 0) { Write-Host "  3. Walk the same volume(s) with the standard scanner, $WalkRuns run(s) each" }
if ($wiz) { Write-Host "  4. Scan the same volume(s) with WizTree $($wiz.Version), $Runs timed run(s) each (free for personal use; measured only, output deleted)" }
elseif (-not $SkipWizTree) { Write-Host '  4. WizTree not found: comparison skipped' }
Write-Host '  5. Smoke-test strata-helper (--version, --help, signature). No service is installed.'
Write-Host '  6. Write bench\results\<date>-<disk>.json and print a Markdown table'
Write-Host ''
Write-Host 'Read-only: it deletes nothing except files it created in its own temp folder,'
Write-Host 'and writes only to bench\results\ and that temp folder (plus cargo''s target dir).'
Write-Host 'No caches are dropped: run 1 is the first of this session, later runs are warm.'
Write-Host 'Close other heavy applications for steadier numbers.'
Write-Host ''

if (-not $Yes -and -not $DryRun) {
    $answer = Read-Host 'Proceed? [y/N]'
    if ($answer -notmatch '^(y|yes)$') { Write-Host 'Cancelled. Nothing was run.'; exit 1 }
}

$script:TempDir = Join-Path ([IO.Path]::GetTempPath()) ('strata-bench-' + [guid]::NewGuid().ToString('N').Substring(0, 12))
if (Test-Path -LiteralPath $script:TempDir) { throw "Temp folder already exists: $script:TempDir" }
[void](New-Item -ItemType Directory -Path $script:TempDir)
$script:Running = $null
$keepTemp = $false
$exitCode = 0

try {
    Write-Step 'Binaries'
    if ($DryRun -or $NoBuild) {
        Write-Note $(if ($DryRun) { 'Dry run: not building.' } else { '-NoBuild: using existing binaries.' })
    } elseif ($cargo) {
        Push-Location $RepoRoot
        try {
            & cargo build --release -p strata-cli -p strata-helper
            if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
            & cargo build --release -p strata-walk --example walkbench
            if ($LASTEXITCODE -ne 0) { Write-Warn "walkbench example failed to build (exit $LASTEXITCODE); standard scanner skipped" }
        } finally { Pop-Location }
    } else {
        Write-Warn 'cargo not found: using existing binaries if present.'
    }
    $have = [ordered]@{
        cli       = Test-Path -LiteralPath $bins.Cli -PathType Leaf
        helper    = Test-Path -LiteralPath $bins.Helper -PathType Leaf
        walkbench = Test-Path -LiteralPath $bins.WalkBench -PathType Leaf
    }
    Write-Note "target dir : $(Get-TargetDir)$(if ($env:CARGO_TARGET_DIR) { ' (CARGO_TARGET_DIR)' })"
    Write-Note "strata-cli : $($bins.Cli) $(if ($have.cli) { '[found]' } else { '[missing]' })"
    Write-Note "helper     : $($bins.Helper) $(if ($have.helper) { '[found]' } else { '[missing]' })"
    Write-Note "walkbench  : $($bins.WalkBench) $(if ($have.walkbench) { '[found]' } else { '[missing]' })"
    if (-not $have.cli -and -not $DryRun) { throw 'strata-cli.exe is missing; build it with: cargo build --release -p strata-cli' }

    Write-Step 'WizTree'
    if ($SkipWizTree) { Write-Note 'Skipped (-SkipWizTree).' }
    elseif ($wiz) { Write-Note "Found $($wiz.Name) $($wiz.Version) at $($wiz.Path)" }
    else { Write-Warn 'WizTree not found (registry Uninstall keys, Program Files). Comparison skipped.' }

    Write-Step 'Machine (generic description only)'
    $hw = Get-Hardware
    $sysDisk = Get-DiskKind ($env:SystemDrive.ToUpperInvariant())
    $summary = "$($hw.cpuCores)-core / $($hw.cpuThreads)-thread $($hw.formFactor), $($hw.ramGB) GB RAM, $($sysDisk.label), $($hw.os)"
    $hw.systemDisk = $sysDisk
    $hw.summary = $summary
    Write-Note $summary

    $measuredOn = (Get-Date).ToString('yyyy-MM-dd')
    $results = [ordered]@{
        schema     = $SchemaVersion
        dryRun     = [bool]$DryRun
        measuredOn = $measuredOn
        strata     = Get-StrataVersion
        machine    = $hw
        settings   = [ordered]@{ runs = $Runs; walkRuns = $WalkRuns; walkThreads = $WalkThreads; goldenJson = [bool]$GoldenJson; cacheDropped = $false }
        helper     = $null
        volumes    = @()
    }
    Write-Note "Strata $($results.strata.version) commit $($results.strata.commit)$(if ($results.strata.dirty) { ' (uncommitted changes)' })"

    Write-Step 'strata-helper smoke test'
    if ($DryRun) {
        $results.helper = [ordered]@{ present = $have.helper; signature = $null; versionOk = $null; version = $null; helpOk = $null }
        Write-Note 'Dry run: not started.'
    } elseif (-not $have.helper) {
        $results.helper = [ordered]@{ present = $false }
        Write-Warn 'strata-helper.exe not built; skipped.'
    } else {
        $sig = Get-AuthenticodeSignature -LiteralPath $bins.Helper
        $verRun = Invoke-Measured -FilePath $bins.Helper -Arguments '--version' -Capture -Timeout 20
        $helpRun = Invoke-Measured -FilePath $bins.Helper -Arguments '--help' -Capture -Timeout 20
        $results.helper = [ordered]@{
            present   = $true
            signature = "$($sig.Status)"
            versionOk = ($verRun.ExitCode -eq 0 -and $verRun.StdOut -match '^strata-helper \S+')
            version   = $(if ($verRun.StdOut -match '^strata-helper (\S+)') { $Matches[1] } else { $null })
            helpOk    = ($helpRun.ExitCode -eq 0 -and $helpRun.StdOut.Length -gt 0)
        }
        Write-Note "signature: $($results.helper.signature) (unsigned is expected for a local build)"
        Write-Note "--version: $(if ($results.helper.versionOk) { 'ok, ' + $results.helper.version } else { 'FAILED' })"
        Write-Note "--help   : $(if ($results.helper.helpOk) { 'ok' } else { 'FAILED' })"
    }

    $volumes = New-Object System.Collections.Generic.List[object]
    foreach ($d in $drives) {
        Write-Step "Volume $d"
        try { $info = Get-VolumeInfo $d } catch { Write-Warn "Volume $d not found; skipped."; continue }
        Write-Note "$($info.fileSystem), $($info.sizeGiB) GiB, $($info.usedGiB) GiB used, $($info.disk.label)$(if ($info.system) { ', system volume' })"
        if ($info.fileSystem -ne 'NTFS') { Write-Warn "$d is $($info.fileSystem), not NTFS; skipped."; continue }

        $entry = [ordered]@{ volume = $info; strata = $null; golden = $null; walker = $null; wiztree = $null }
        $sRuns = New-Object System.Collections.Generic.List[object]
        $wRuns = New-Object System.Collections.Generic.List[object]
        if ($DryRun) {
            $sRuns.Add((Invoke-Command { $r = [ordered]@{ run = 1; firstOfSession = $true; ok = $false; wallSeconds = $null; peakWorkingSetMiB = $null }; foreach ($kv in (ConvertFrom-StrataReport '').GetEnumerator()) { $r[$kv.Key] = $kv.Value }; $r }))
            if ($wiz) { $wRuns.Add([ordered]@{ run = 1; firstOfSession = $true; ok = $false; wallSeconds = $null; peakWorkingSetMiB = $null }) }
            Write-Note 'Dry run: scans skipped.'
        } else {
            for ($i = 1; $i -le $Runs; $i++) {
                $s = Invoke-StrataScan $bins.Cli $d $i $null
                $sRuns.Add($s)
                if ($s.ok) { Write-Note ('Strata  run {0}: {1,8:0.000} s wall, {2,7:0} MiB peak, {3:N0} records, unaccounted {4}%' -f $i, $s.wallSeconds, $s.peakWorkingSetMiB, $s.recordsEmitted, $s.unaccountedPercent) }
                else { Write-Warn "Strata run ${i}: $($s.error)" }
                if ($wiz) {
                    $w = Invoke-WizTreeScan $wiz $d $i
                    $wRuns.Add($w)
                    if ($w.ok) { Write-Note ('WizTree run {0}: {1,8:0.000} s wall, {2,7:0} MiB peak' -f $i, $w.wallSeconds, $w.peakWorkingSetMiB) }
                    else { Write-Warn "WizTree run ${i}: $($w.error)" }
                }
            }
        }
        $entry.strata = [ordered]@{ command = "strata-cli scan $d --top 0"; runs = $sRuns.ToArray(); summary = Get-Summary $sRuns.ToArray() }
        if ($wiz) {
            $entry.wiztree = [ordered]@{ version = $wiz.Version; executable = $wiz.Name
                command = "$($wiz.Name) `"$d`" /export=[temp csv] /admin=1 /exportfiles=0 /exportfolders=1 /exportmaxdepth=1"
                runs = $wRuns.ToArray(); summary = Get-Summary $wRuns.ToArray() }
        }

        if ($GoldenJson -and -not $DryRun) {
            $gj = Join-Path $script:TempDir "golden-$($d.Substring(0,1)).json"
            $g = Invoke-StrataScan $bins.Cli $d 0 $gj
            $size = if (Test-Path -LiteralPath $gj) { (Get-Item -LiteralPath $gj).Length } else { $null }
            $entry.golden = [ordered]@{ ok = $g.ok; wallSeconds = $g.wallSeconds; peakWorkingSetMiB = $g.peakWorkingSetMiB; jsonMiB = ConvertTo-Rounded ($size / 1MB) 1 }
            Write-Note ('Strata --json run: {0:0.000} s wall, {1:0} MiB peak, {2} MiB JSON (deleted)' -f $g.wallSeconds, $g.peakWorkingSetMiB, $entry.golden.jsonMiB)
            if ($size) { Remove-Item -LiteralPath $gj -Force }
        }

        if ($WalkRuns -gt 0) {
            if ($DryRun) {
                $entry.walker = [ordered]@{ runs = @(); summary = Get-Summary @() }
            } elseif (-not $have.walkbench) {
                Write-Warn 'walkbench example not built; standard scanner skipped.'
            } else {
                $kRuns = New-Object System.Collections.Generic.List[object]
                for ($i = 1; $i -le $WalkRuns; $i++) {
                    $k = Invoke-WalkBench $bins.WalkBench $d $i
                    $kRuns.Add($k)
                    if ($k.ok) { Write-Note ('Standard scanner run {0}: {1:0.00} s, {2:N0} entries, {3:0.00} s per 1M, {4:0} MiB peak' -f $i, $k.wallSeconds, ($k.files + $k.dirs), $k.secondsPer1MEntries, $k.peakWorkingSetMiB) }
                    else { Write-Warn "Standard scanner run ${i}: $($k.error)" }
                }
                $entry.walker = [ordered]@{ note = 'elevated session; listing=dirinfo, allocation pass on'; runs = $kRuns.ToArray(); summary = Get-Summary $kRuns.ToArray() }
            }
        }
        $volumes.Add($entry)
    }
    $results.volumes = $volumes.ToArray()
    if ($volumes.Count -eq 0) { throw 'No volume was scanned.' }

    $results.comparison = New-Comparison $results $summary $measuredOn
    $markdown = New-Markdown $results
    $results.markdown = $markdown

    $json = Protect-Json ($results | ConvertTo-Json -Depth 12)
    $short = $volumes[0].volume.disk.short
    if ($DryRun) {
        $outPath = Join-Path $script:TempDir "$measuredOn-$short.dryrun.json"
        $keepTemp = $true
    } else {
        if (-not (Test-Path -LiteralPath $ResultsDir)) { [void](New-Item -ItemType Directory -Path $ResultsDir) }
        $outPath = Join-Path $ResultsDir "$measuredOn-$short.json"
        $n = 2
        while (Test-Path -LiteralPath $outPath) { $outPath = Join-Path $ResultsDir "$measuredOn-$short-$n.json"; $n++ }
    }
    Write-Utf8 $outPath $json

    Write-Step 'site/data/benchmarks.json -> comparison'
    Write-Host (Protect-Json ($results.comparison | ConvertTo-Json -Depth 8))
    Write-Step 'docs/BENCHMARKS.md snippet'
    Write-Host $markdown
    Write-Step 'Done'
    Write-Note "Results: $outPath"
    if ($DryRun) { Write-Note 'Dry run: the file above holds no measurements; delete its folder when done.' }
} catch {
    Write-Host "`nERROR: $($_.Exception.Message)" -ForegroundColor Red
    Write-Host $_.ScriptStackTrace
    $exitCode = 1
} finally {
    if ($script:Running -and -not $script:Running.HasExited) {
        try { $script:Running.Kill() } catch { }
    }
    if (-not $keepTemp -and (Test-Path -LiteralPath $script:TempDir)) {
        # IMPORTANT: only ever removes the folder this run created (fresh GUID name, checked above).
        Remove-Item -LiteralPath $script:TempDir -Recurse -Force
    }
}
exit $exitCode
