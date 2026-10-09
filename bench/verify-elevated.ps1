#Requires -Version 5.1
<#
.SYNOPSIS
    Elevated verification and benchmark run for Strata: MFT scan, standard
    scanner, WizTree and Windows File Explorer head-to-head and a helper
    smoke test.

.DESCRIPTION
    Run once from an elevated PowerShell. Measures, per volume:
      - strata-cli MFT scan: wall time, peak working set, records, files,
        directories and the used-space reconciliation (used vs. accounted).
      - the standard scanner (strata-walk walkbench example), for reference.
      - WizTree (if installed): wall time and peak working set of a full scan
        with its command-line export.
      - Windows File Explorer: the Properties dialog on everything Select all
        picks in the volume root, timed until Size, Size on disk and Contains
        stop changing (read with UI Automation). Falls back to a timed
        "dir /s /a" listing, labelled as such, if the dialog can't be read.
    Then writes a results file and prints a docs/BENCHMARKS.md table.

    READ-ONLY GUARANTEES
      - Never deletes, moves or modifies anything except files this script
        created itself, inside its own fresh temp folder, which is removed at
        the end.
      - Writes only to bench\results\ and that temp folder (plus Cargo's normal
        build output in the target directory when it builds the binaries).
      - Installs nothing, starts no services, changes no settings, and leaves
        no processes running: a timed-out child it started is stopped.
      - Every Properties dialog it opens is closed with Cancel semantics, so
        nothing on it is applied. It never stops or restarts explorer.exe.
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

.PARAMETER SkipExplorer
    Skip the Windows File Explorer comparison.

.PARAMETER NoBuild
    Don't invoke cargo; use existing binaries.

.PARAMETER TimeoutSec
    Per-run timeout. A run that exceeds it is stopped and recorded as failed.

.PARAMETER Yes
    Skip the Y/N confirmation.

.PARAMETER DryRun
    Skip elevation, builds and scans. Detects binaries, WizTree and hardware,
    and writes a results-shaped JSON (no measurements) into a temp folder.

.PARAMETER ExplorerSelfTest
    Internal check of the Explorer measurement, no elevation needed: opens
    Properties on the given folder, then on everything in it, waits for each
    size to settle, closes the dialog and prints what it read. Nothing else
    runs and nothing is written.

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
    [switch]$SkipExplorer,
    [switch]$NoBuild,
    [ValidateRange(10, 7200)][int]$TimeoutSec = 900,
    [switch]$Yes,
    [switch]$DryRun,
    [string]$ExplorerSelfTest
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

# -----------------------------------------------------------------------------
# Windows File Explorer
# -----------------------------------------------------------------------------

$ExplorerTool = 'Windows File Explorer'
$DirListingTool = 'Windows built-in (dir /s)'
# NOTE: Win32 control IDs of the shell's General property page (shell32). They
# are the same in every display language, so fields are found by ID, not label.
$ExplorerFieldIds = [ordered]@{ size = '13064'; sizeOnDisk = '13106'; contains = '13087' }
$ExplorerSettleSec = 2.0
$ExplorerPollMs = 100

if (-not ('StrataBench.ShellProps' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace StrataBench {
    public static class ShellProps {
        [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
        static extern int SHParseDisplayName(string name, IntPtr bindCtx, out IntPtr pidl, uint sfgaoIn, out uint sfgaoOut);
        [DllImport("shell32.dll")] static extern IntPtr ILFindLastID(IntPtr pidl);
        [DllImport("shell32.dll")] static extern void ILFree(IntPtr pidl);
        [DllImport("shell32.dll")]
        static extern int SHCreateDataObject(IntPtr pidlFolder, uint cidl, IntPtr[] apidl, IntPtr inner, ref Guid riid, out IntPtr dataObject);
        [DllImport("shell32.dll")] static extern int SHMultiFileProperties(IntPtr dataObject, uint flags);
        [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
        static extern bool SHObjectProperties(IntPtr hwnd, uint type, string name, string page);

        [StructLayout(LayoutKind.Sequential)]
        struct Msg { public IntPtr hwnd; public uint message; public IntPtr wParam; public IntPtr lParam; public uint time; public int x; public int y; }
        [DllImport("user32.dll")] static extern int GetMessage(out Msg msg, IntPtr hwnd, uint min, uint max);
        [DllImport("user32.dll")] static extern bool TranslateMessage(ref Msg msg);
        [DllImport("user32.dll")] static extern IntPtr DispatchMessage(ref Msg msg);
        [DllImport("user32.dll")] static extern bool PostThreadMessage(uint threadId, uint msg, IntPtr wParam, IntPtr lParam);
        [DllImport("kernel32.dll")] static extern uint GetCurrentThreadId();

        static System.Threading.Thread host;
        static uint hostId;

        // The shell objects behind the dialog live on the thread that opens it,
        // and the dialog's size worker calls back into them. That thread must
        // keep pumping messages while the caller polls with UI Automation, so
        // the open call runs on a dedicated STA thread with its own loop.
        // Returns the open call's HRESULT (0 = dialog requested).
        public static int Open(string parent, string[] children) {
            Close();
            int result = unchecked((int)0x80004005);
            var ready = new System.Threading.ManualResetEvent(false);
            host = new System.Threading.Thread(() => {
                hostId = GetCurrentThreadId();
                try {
                    result = children == null
                        ? (SHObjectProperties(IntPtr.Zero, 2 /* SHOP_FILEPATH */, parent, null) ? 0 : unchecked((int)0x80004005))
                        : OpenSelection(parent, children);
                } finally { ready.Set(); }
                Msg m;
                while (GetMessage(out m, IntPtr.Zero, 0, 0) > 0) { TranslateMessage(ref m); DispatchMessage(ref m); }
            });
            host.IsBackground = true;
            host.SetApartmentState(System.Threading.ApartmentState.STA);
            host.Start();
            ready.WaitOne();
            return result;
        }

        // Ends the host thread's message loop. Call after the dialog is closed.
        public static void Close() {
            if (host == null) return;
            PostThreadMessage(hostId, 0x0012 /* WM_QUIT */, IntPtr.Zero, IntPtr.Zero);
            host.Join(5000);
            host = null;
        }

        static int OpenSelection(string parent, string[] children) {
            IntPtr parentPidl; uint unused;
            int hr = SHParseDisplayName(parent, IntPtr.Zero, out parentPidl, 0, out unused);
            if (hr != 0) return hr;
            var abs = new IntPtr[children.Length];
            var rel = new IntPtr[children.Length];
            try {
                for (int i = 0; i < children.Length; i++) {
                    hr = SHParseDisplayName(children[i], IntPtr.Zero, out abs[i], 0, out unused);
                    if (hr != 0) return hr;
                    rel[i] = ILFindLastID(abs[i]);
                }
                Guid iidDataObject = new Guid("0000010e-0000-0000-C000-000000000046");
                IntPtr dataObject;
                hr = SHCreateDataObject(parentPidl, (uint)children.Length, rel, IntPtr.Zero, ref iidDataObject, out dataObject);
                if (hr != 0) return hr;
                try { return SHMultiFileProperties(dataObject, 0); } finally { Marshal.Release(dataObject); }
            } finally {
                foreach (var p in abs) if (p != IntPtr.Zero) ILFree(p);
                ILFree(parentPidl);
            }
        }
    }
}
'@
}

function Get-ExplorerVersion {
    try { return (Get-Item -LiteralPath (Join-Path $env:windir 'explorer.exe')).VersionInfo.ProductVersion.Trim() } catch { return $null }
}

<#
What Select all shows in the user's own Explorer: hidden items only when
"Show hidden files" is on, protected OS items (hidden + system) only when
"Hide protected operating system files" is also off. Reads HKCU, changes nothing.
#>
function Get-ExplorerSelection([string]$Folder) {
    $adv = Get-ItemProperty -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced' -ErrorAction SilentlyContinue
    $showHidden = [bool]($adv -and $adv.PSObject.Properties['Hidden'] -and $adv.Hidden -eq 1)
    $showSuper = [bool]($showHidden -and $adv.PSObject.Properties['ShowSuperHidden'] -and $adv.ShowSuperHidden -eq 1)
    $all = @(Get-ChildItem -LiteralPath $Folder -Force -ErrorAction SilentlyContinue)
    $shown = @($all | Where-Object {
            $a = $_.Attributes
            $hidden = [bool]($a -band [IO.FileAttributes]::Hidden)
            $system = [bool]($a -band [IO.FileAttributes]::System)
            -not $hidden -or ($showHidden -and (-not $system -or $showSuper))
        })
    return [pscustomobject]@{
        Paths      = [string[]]@($shown | ForEach-Object { $_.FullName })
        Shown      = $shown.Count
        Hidden     = $all.Count - $shown.Count
        ShowHidden = $showHidden
    }
}

function Initialize-Uia {
    if (-not ('System.Windows.Automation.AutomationElement' -as [type])) {
        Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
    }
}

function Get-ShellDialog {
    $cond = New-Object System.Windows.Automation.AndCondition (
        (New-Object System.Windows.Automation.PropertyCondition ([System.Windows.Automation.AutomationElement]::ProcessIdProperty, $PID)),
        (New-Object System.Windows.Automation.PropertyCondition ([System.Windows.Automation.AutomationElement]::ClassNameProperty, '#32770')))
    return @([System.Windows.Automation.AutomationElement]::RootElement.FindAll('Children', $cond))
}

function Get-DialogField($Dialog, [string]$AutomationId) {
    $cond = New-Object System.Windows.Automation.PropertyCondition ([System.Windows.Automation.AutomationElement]::AutomationIdProperty, $AutomationId)
    $e = $Dialog.FindFirst('Descendants', $cond)
    if ($null -eq $e) { return $null }
    # NOTE: an Edit field's Name is usually its label ("Size:"); the shown text
    # is its Value. A Static field (Contains, on a multi-item page) has only a Name.
    $text = $null
    $vp = $null
    if ($e.TryGetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern, [ref]$vp)) { $text = $vp.Current.Value }
    if (-not $text) { $text = $e.Current.Name }
    return ($text -replace '\p{Cf}', '')
}

<# "976 KB (1,000,000 bytes)" in any language: the digits inside the last parentheses. #>
function ConvertFrom-SizeField([string]$Text) {
    if (-not $Text) { return $null }
    $m = [regex]::Matches($Text, '\(([^()]*)\)')
    if ($m.Count -eq 0) { return $null }
    $digits = $m[$m.Count - 1].Groups[1].Value -replace '\D', ''
    if ($digits -eq '') { return $null }
    return [long]$digits
}

<# "1,000 Files, 5 Folders": two integers, files first, with any group separator. #>
function ConvertFrom-ContainsField([string]$Text) {
    if (-not $Text) { return @($null, $null) }
    $n = @([regex]::Matches($Text, '\d{1,3}(?:[,.''\p{Zs}]\d{3})+(?!\d)|\d+') | ForEach-Object { [long]($_.Value -replace '\D', '') })
    if ($n.Count -lt 2) { return @($null, $null) }
    return @($n[0], $n[1])
}

function Close-ShellDialog($Dialog) {
    if ($null -eq $Dialog) { return }
    $handle = 0
    try { $handle = $Dialog.Current.NativeWindowHandle } catch { return }
    # NOTE: Close is the same as Cancel: nothing on the property page is applied.
    try { $Dialog.GetCurrentPattern([System.Windows.Automation.WindowPattern]::Pattern).Close() } catch { }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt 10) {
        if (-not @(Get-ShellDialog | Where-Object { $_.Current.NativeWindowHandle -eq $handle }).Count) { return }
        Start-Sleep -Milliseconds 100
    }
    try {
        $cancel = $Dialog.FindFirst('Descendants', (New-Object System.Windows.Automation.PropertyCondition ([System.Windows.Automation.AutomationElement]::AutomationIdProperty, '2')))
        if ($cancel) { $cancel.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke() }
    } catch { }
}

<#
Opens the shell's Properties dialog and times it until the size settles.

Steps:
  1. Open Properties: on the folder itself (Mode Folder) or on everything
     Explorer's Select all would pick in it (Mode Selection, used for a volume
     root, whose own Properties page shows the free-space chart, not a count).
  2. Find the new dialog with UI Automation and poll the Size, Size on disk
     and Contains fields every $ExplorerPollMs ms.
  3. Settled = no field changed for $ExplorerSettleSec s. Wall time is from
     the open call to the last observed change, so the settle window is not
     counted.
  4. Close the dialog (Cancel semantics) and wait for it to go away.

The dialog runs on a shell thread in this process: the same shell32 code
Explorer runs, with this session's rights.
#>
function Measure-ExplorerSize {
    param(
        [Parameter(Mandatory)][string]$Target,
        [ValidateSet('Folder', 'Selection')][string]$Mode = 'Folder',
        [int]$Run = 1,
        [int]$Timeout = $TimeoutSec
    )
    $row = [ordered]@{ run = $Run; firstOfSession = ($Run -eq 1); ok = $false; supported = $true; wallSeconds = $null
        dialogOpenSeconds = $null; sizeBytes = $null; sizeOnDiskBytes = $null; files = $null; folders = $null
        selectedItems = $null; peakWorkingSetMiB = $null }
    $selection = $null
    if ($Mode -eq 'Selection') {
        $selection = Get-ExplorerSelection $Target
        $row.selectedItems = $selection.Shown
        if ($selection.Shown -eq 0) { $row.error = 'nothing to select'; return $row }
    }
    try { Initialize-Uia } catch { $row.supported = $false; $row.error = "UI Automation unavailable: $($_.Exception.Message)"; return $row }
    $before = @(Get-ShellDialog | ForEach-Object { $_.Current.NativeWindowHandle })

    $sw = [Diagnostics.Stopwatch]::StartNew()
    $hr = [StrataBench.ShellProps]::Open($Target, $(if ($selection) { $selection.Paths } else { $null }))
    if ($hr -ne 0) {
        [StrataBench.ShellProps]::Close()
        $row.supported = $false
        $row.error = ('opening Properties failed (0x{0:X8})' -f $hr)
        return $row
    }

    $dlg = $null
    while (-not $dlg -and $sw.Elapsed.TotalSeconds -lt [math]::Min(30, $Timeout)) {
        $dlg = @(Get-ShellDialog | Where-Object { $before -notcontains $_.Current.NativeWindowHandle })[0]
        if (-not $dlg) { Start-Sleep -Milliseconds 50 }
    }
    if (-not $dlg) {
        [StrataBench.ShellProps]::Close()
        $row.supported = $false
        $row.error = 'the Properties dialog did not open'
        return $row
    }
    $script:ExplorerDialog = $dlg
    $row.dialogOpenSeconds = ConvertTo-Rounded $sw.Elapsed.TotalSeconds 3

    try {
        $last = $null; $lastChange = $sw.Elapsed.TotalSeconds; $fields = $null
        while ($true) {
            $now = $sw.Elapsed.TotalSeconds
            $f = [ordered]@{}
            foreach ($kv in $ExplorerFieldIds.GetEnumerator()) { $f[$kv.Key] = Get-DialogField $dlg $kv.Value }
            $snap = "$($f.size)|$($f.sizeOnDisk)|$($f.contains)"
            if ($snap -ne $last) { $last = $snap; $lastChange = $now; $fields = $f }
            $quiet = $now - $lastChange
            $bytes = ConvertFrom-SizeField $f.size
            # NOTE: an empty selection legitimately stays at 0 bytes; give it longer before calling it settled.
            if ($null -ne $bytes -and $quiet -ge $ExplorerSettleSec -and ($bytes -gt 0 -or $quiet -ge 5 * $ExplorerSettleSec)) { break }
            if ($now -gt $Timeout) { $row.error = "size still changing after $Timeout s"; break }
            if ($null -eq $f.size -and $now -gt 15) {
                $row.supported = $false
                $row.error = "the Size field (control $($ExplorerFieldIds.size)) was not found on the Properties dialog; this Windows build lays it out differently"
                break
            }
            Start-Sleep -Milliseconds $ExplorerPollMs
        }
        if (-not $row.Contains('error')) {
            $row.ok = $true
            $row.wallSeconds = ConvertTo-Rounded $lastChange 3
            $row.sizeBytes = ConvertFrom-SizeField $fields.size
            $row.sizeOnDiskBytes = ConvertFrom-SizeField $fields.sizeOnDisk
            $counts = ConvertFrom-ContainsField $fields.contains
            $row.files = $counts[0]
            $row.folders = $counts[1]
        }
    } finally {
        Close-ShellDialog $dlg
        $script:ExplorerDialog = $null
        [StrataBench.ShellProps]::Close()
    }
    return $row
}

<# Fallback when the Properties dialog can't be automated: a full recursive listing by cmd's dir. #>
function Invoke-DirListing([string]$Drive, [int]$Run) {
    $r = Invoke-Measured -FilePath $env:ComSpec -Arguments "/d /c dir /s /a $Drive\ >NUL 2>NUL"
    $row = [ordered]@{ run = $Run; firstOfSession = ($Run -eq 1); ok = (-not $r.TimedOut) }
    $row.wallSeconds = ConvertTo-Rounded $r.WallSeconds 3
    $row.peakWorkingSetMiB = ConvertTo-Rounded ($r.PeakWorkingSet / 1MB) 1
    if ($r.TimedOut) { $row.error = "timed out after $TimeoutSec s" }
    return $row
}

<# Checks the Properties automation end to end on a few files in this run's own temp folder. #>
function Test-ExplorerAutomation {
    $probe = Join-Path $script:TempDir 'explorer-probe'
    [void](New-Item -ItemType Directory -Path $probe)
    foreach ($i in 1..3) { Write-Utf8 (Join-Path $probe "file$i.txt") ('x' * 1000) }
    try {
        $r = Measure-ExplorerSize -Target $script:TempDir -Mode Selection -Timeout 60
        $okCounts = ($r.ok -and $r.sizeBytes -ge 3000 -and $r.files -ge 3)
        return [pscustomobject]@{ Ok = $okCounts; Error = $(if ($okCounts) { $null } elseif ($r.Contains('error')) { $r.error } else { 'fields read but values did not match the probe files' }) }
    } finally {
        Remove-Item -LiteralPath $probe -Recurse -Force -ErrorAction SilentlyContinue
    }
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
        # NOTE: Explorer gets rows of its own because its scope differs from a
        # full scan; the detail states both scopes. No memory row: see peakMemoryNote.
        $x = $v.explorer
        if ($x -and ($s.okRuns -gt 0 -or $DryRun) -and ($x.summary.okRuns -gt 0 -or $DryRun)) {
            if ($x.method -eq 'properties') {
                $okX = @($x.runs | Where-Object { $_.ok })
                $counted = if ($okX.Count) { "$('{0:N0}' -f $okX[0].files) files and $('{0:N0}' -f $okX[0].folders) folders" } else { 'n/a files and folders' }
                $hidden = if ($x.hiddenItemsLeftOut -gt 0) { ', hidden items left out as in its default view' } else { '' }
                $scope = "Strata scans the entire volume ($(Format-EntryCount $entries)); Explorer counts the $($x.selectedItems) top-level items Select all picks in the drive root ($counted$hidden), Properties dialog until the size settles"
            } else {
                $scope = "Strata MFT scan vs. dir /s /a listing, both over the entire volume ($(Format-EntryCount $entries))"
            }
            $pair = { param($strataVal, $xVal)
                , @([ordered]@{ tool = 'Strata'; version = $Results.strata.version; value = $strataVal },
                    [ordered]@{ tool = $x.tool; version = $x.version; value = $xVal })
            }
            $items.Add([ordered]@{ metric = "Total size of the drive, $where"; detail = "$scope; median of $Runs runs"; unit = 's'; lowerIsBetter = $true
                values = (& $pair $s.medianWallSeconds $x.summary.medianWallSeconds) })
            $items.Add([ordered]@{ metric = "Total size of the drive, first of the session, $where"; detail = "$scope; no cache dropped, run 1 of $Runs"; unit = 's'; lowerIsBetter = $true
                values = (& $pair $s.firstRunWallSeconds $x.summary.firstRunWallSeconds) })
        }
    }
    $methodology = New-Object System.Collections.Generic.List[string]
    $methodology.Add('Same machine, same volume, elevated session for every tool; tools run alternately, Strata first in each round.')
    $methodology.Add("Wall-clock time from process start to exit with a complete size total; median of $Runs runs. Run 1 is the first of the session; no cache was dropped and there was no reboot between runs.")
    $methodology.Add('Strata: strata-cli MFT scan (reads the MFT unbuffered, so the OS file cache does not help it). WizTree: command-line scan with a minimal CSV export (top-level folders only).')
    $explorerRows = @($Results.volumes | Where-Object { $_.explorer })
    if ($explorerRows.Count -and $explorerRows[0].explorer.method -eq 'properties') {
        $methodology.Add("Windows File Explorer: the Properties dialog on everything Select all picks in the drive root (what a user does to see what fills a drive; the root's own Properties page shows only used and free space). Timed from opening the dialog to the last change of its Size, Size on disk and Contains fields, read with UI Automation; a total counts as settled after $ExplorerSettleSec s without change, which is not included. Strata always scans the entire volume, so the scopes differ as each row states.")
    } elseif ($explorerRows.Count) {
        $methodology.Add("$DirListingTool`: cmd.exe dir /s /a over the entire volume with output discarded, timed like the other tools. Used because the Explorer Properties dialog could not be automated on the measuring machine; it is not a measurement of Explorer.")
    }
    $methodology.Add('Peak memory is the peak working set of the measured process.' + $(if ($explorerRows.Count -and $explorerRows[0].explorer.method -eq 'properties') { ' Not reported for Explorer: its Properties dialog runs inside a shared host process.' } else { '' }))
    $methodology.Add('Each tool at the version listed, default settings.')
    return [ordered]@{
        release     = $Results.strata.version
        measuredOn  = $MeasuredOn
        machine     = $MachineText
        methodology = $methodology.ToArray()
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
        if ($v.explorer) {
            $x = $v.explorer
            $xs = $x.summary
            $okx = @($x.runs | Where-Object { $_.ok })
            if ($x.method -eq 'properties') {
                $xe = if ($okx.Count -and $null -ne $okx[0].files) { '{0:N0}' -f ($okx[0].files + $okx[0].folders) } else { 'n/a' }
                $lines.Add("| $vol | $($x.tool) (Properties on $($x.selectedItems) top-level items)* | $(& $fmtS $xs['medianWallSeconds']) | $(& $fmtS $xs['firstRunWallSeconds']) | n/a | $xe |")
            } else {
                $lines.Add("| $vol | $($x.tool)* | $(& $fmtS $xs['medianWallSeconds']) | $(& $fmtS $xs['firstRunWallSeconds']) | $(& $fmtM $xs['medianPeakWorkingSetMiB']) | n/a |")
            }
        }
        if ($v.walker) {
            $wk = $v.walker.summary
            $okw = @($v.walker.runs | Where-Object { $_.ok })
            $we = if ($okw.Count) { '{0:N0}' -f ($okw[0].files + $okw[0].dirs) } else { 'n/a' }
            $lines.Add("| $vol | Strata (standard scanner) | $(& $fmtS $wk['medianWallSeconds']) | n/a | $(& $fmtM $wk['medianPeakWorkingSetMiB']) | $we |")
        }
    }
    $xv = @($Results.volumes | Where-Object { $_.explorer })
    if ($xv.Count) {
        $lines.Add('')
        $lines.Add('\* The other rows scan the entire volume.' + $(if ($xv[0].explorer.method -eq 'properties') { ' Explorer has no process of its own, so no peak memory.' } else { '' }))
        foreach ($v in $xv) { $lines.Add("On $($v.volume.volume), $($v.explorer.tool): $($v.explorer.scope).") }
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

if ($ExplorerSelfTest) {
    if (-not (Test-Path -LiteralPath $ExplorerSelfTest -PathType Container)) { Write-Host "Not a folder: $ExplorerSelfTest" -ForegroundColor Red; exit 2 }
    $target = (Resolve-Path -LiteralPath $ExplorerSelfTest).ProviderPath
    Write-Host "Explorer self-test (Properties dialog, $Runs run(s) per mode, timeout $TimeoutSec s)" -ForegroundColor Cyan
    $allOk = $true
    $script:ExplorerDialog = $null
    try {
        foreach ($mode in @('Folder', 'Selection')) {
            Write-Step $(if ($mode -eq 'Folder') { 'Properties on the folder' } else { 'Properties on Select all inside the folder' })
            for ($i = 1; $i -le $Runs; $i++) {
                $r = Measure-ExplorerSize -Target $target -Mode $mode -Run $i
                if ($r.ok) {
                    Write-Note ('run {0}: dialog open {1:0.000} s, settled at {2:0.000} s; size {3:N0} B, on disk {4:N0} B, {5:N0} files, {6:N0} folders{7}; dialog closed' -f $i, $r.dialogOpenSeconds, $r.wallSeconds, $r.sizeBytes, $r.sizeOnDiskBytes, $r.files, $r.folders, $(if ($null -ne $r.selectedItems) { ", $($r.selectedItems) items selected" }))
                } else {
                    $allOk = $false
                    Write-Warn "run ${i}: $($r.error)"
                }
            }
        }
        $left = @(Get-ShellDialog).Count
        Write-Note "Properties dialogs still open in this process: $left"
        if ($left) { $allOk = $false }
    } finally {
        Close-ShellDialog $script:ExplorerDialog
    }
    exit $(if ($allOk) { 0 } else { 1 })
}

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
if (-not $SkipExplorer) {
    Write-Host "  5. Windows File Explorer, $Runs timed run(s) per volume: open Properties on everything Select all picks in the"
    Write-Host '     volume root, wait until the size stops changing, close the dialog (Cancel). Falls back to a timed "dir /s /a"'
    Write-Host '     if the dialog cannot be read. Nothing is changed.'
}
Write-Host '  6. Smoke-test strata-helper (--version, --help, signature). No service is installed.'
Write-Host '  7. Write bench\results\<date>-<disk>.json and print a Markdown table'
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
$script:ExplorerDialog = $null
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

    Write-Step 'Windows File Explorer'
    # Method: 'properties' (UI Automation on the Properties dialog), 'dir' (fallback) or $null (skipped).
    $explorerMethod = $null
    $explorerVersion = Get-ExplorerVersion
    if ($SkipExplorer) { Write-Note 'Skipped (-SkipExplorer).' }
    else {
        $probe = Test-ExplorerAutomation
        if ($probe.Ok) {
            $explorerMethod = 'properties'
            Write-Note "Properties dialog automation works (checked on files in this run's temp folder). Explorer $explorerVersion."
        } else {
            $explorerMethod = 'dir'
            Write-Warn "Properties dialog can't be read here: $($probe.Error)"
            Write-Warn "Falling back to a timed 'dir /s /a', reported as '$DirListingTool', not as Explorer."
        }
    }

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
        settings   = [ordered]@{ runs = $Runs; walkRuns = $WalkRuns; walkThreads = $WalkThreads; goldenJson = [bool]$GoldenJson; cacheDropped = $false; explorer = $(if ($explorerMethod) { $explorerMethod } else { 'skipped' }) }
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

        $entry = [ordered]@{ volume = $info; strata = $null; golden = $null; walker = $null; wiztree = $null; explorer = $null }
        $sRuns = New-Object System.Collections.Generic.List[object]
        $wRuns = New-Object System.Collections.Generic.List[object]
        $eRuns = New-Object System.Collections.Generic.List[object]
        $sel = if ($explorerMethod -eq 'properties') { Get-ExplorerSelection "$d\" } else { $null }
        if ($DryRun) {
            $sRuns.Add((Invoke-Command { $r = [ordered]@{ run = 1; firstOfSession = $true; ok = $false; wallSeconds = $null; peakWorkingSetMiB = $null }; foreach ($kv in (ConvertFrom-StrataReport '').GetEnumerator()) { $r[$kv.Key] = $kv.Value }; $r }))
            if ($wiz) { $wRuns.Add([ordered]@{ run = 1; firstOfSession = $true; ok = $false; wallSeconds = $null; peakWorkingSetMiB = $null }) }
            if ($explorerMethod) { $eRuns.Add([ordered]@{ run = 1; firstOfSession = $true; ok = $false; wallSeconds = $null; peakWorkingSetMiB = $null }) }
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
                if ($explorerMethod -eq 'properties') {
                    $e = Measure-ExplorerSize -Target "$d\" -Mode Selection -Run $i
                    $eRuns.Add($e)
                    if ($e.ok) { Write-Note ('Explorer run {0}: {1,7:0.000} s to settle, {2:N0} files, {3:N0} folders, {4:N0} bytes' -f $i, $e.wallSeconds, $e.files, $e.folders, $e.sizeBytes) }
                    else { Write-Warn "Explorer run ${i}: $($e.error)" }
                } elseif ($explorerMethod -eq 'dir') {
                    $e = Invoke-DirListing $d $i
                    $eRuns.Add($e)
                    if ($e.ok) { Write-Note ('dir /s run {0}: {1,7:0.000} s wall' -f $i, $e.wallSeconds) }
                    else { Write-Warn "dir /s run ${i}: $($e.error)" }
                }
            }
        }
        $entry.strata = [ordered]@{ command = "strata-cli scan $d --top 0"; runs = $sRuns.ToArray(); summary = Get-Summary $sRuns.ToArray() }
        if ($wiz) {
            $entry.wiztree = [ordered]@{ version = $wiz.Version; executable = $wiz.Name
                command = "$($wiz.Name) `"$d`" /export=[temp csv] /admin=1 /exportfiles=0 /exportfolders=1 /exportmaxdepth=1"
                runs = $wRuns.ToArray(); summary = Get-Summary $wRuns.ToArray() }
        }
        if ($explorerMethod -eq 'properties') {
            $eSummary = Get-Summary $eRuns.ToArray()
            $eSummary.medianPeakWorkingSetMiB = $null
            $hiddenNote = if ($sel.Hidden -gt 0) { ", leaving out $($sel.Hidden) hidden item(s) the user's Explorer view doesn't show" } else { '' }
            $entry.explorer = [ordered]@{ tool = $ExplorerTool; version = $explorerVersion; method = 'properties'
                scope = "Properties dialog on the $($sel.Shown) top-level item(s) Select all picks in the $d root$hiddenNote; timed until Size, Size on disk and Contains stopped changing for $ExplorerSettleSec s (settle window not counted)"
                selectedItems = $sel.Shown; hiddenItemsLeftOut = $sel.Hidden; settleSeconds = $ExplorerSettleSec; pollMilliseconds = $ExplorerPollMs
                peakMemoryNote = 'not measured: the dialog is shell code on a thread of a host process (normally explorer.exe, shared with the desktop; here the benchmark''s own PowerShell), so no process peak belongs to the count alone'
                runs = $eRuns.ToArray(); summary = $eSummary }
        } elseif ($explorerMethod -eq 'dir') {
            $entry.explorer = [ordered]@{ tool = $DirListingTool; version = $explorerVersion; method = 'dir'
                scope = "cmd.exe dir /s /a of the whole $d volume, output discarded"
                runs = $eRuns.ToArray(); summary = Get-Summary $eRuns.ToArray() }
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
    if ($script:ExplorerDialog) { Close-ShellDialog $script:ExplorerDialog }
    if ('StrataBench.ShellProps' -as [type]) { [StrataBench.ShellProps]::Close() }
    if (-not $keepTemp -and (Test-Path -LiteralPath $script:TempDir)) {
        # IMPORTANT: only ever removes the folder this run created (fresh GUID name, checked above).
        Remove-Item -LiteralPath $script:TempDir -Recurse -Force
    }
}
exit $exitCode
