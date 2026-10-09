#Requires -Version 5.1
<#
.SYNOPSIS
    Shared helpers for Strata's VHDX fixture volumes.

.DESCRIPTION
    Responsibilities:
    - Test-StrataElevated: token elevation check.
    - New-StrataVhd / Dismount-StrataVhd: create, partition, format and mount a
      VHDX (Hyper-V cmdlets when present, diskpart otherwise).
    - Invoke-StrataNative: run a native tool and fail loudly on a non-zero exit.
    - ConvertTo-LongPath: \\?\ prefix so paths over MAX_PATH, trailing dots and
      spaces, and reserved device names reach the filesystem verbatim.
#>

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Test-StrataElevated {
    <#
    .SYNOPSIS
        Returns $true when the current process token is elevated.
    #>
    [CmdletBinding()]
    [OutputType([bool])]
    param()
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Assert-StrataElevated {
    <#
    .SYNOPSIS
        Throws unless the session is elevated.
    #>
    [CmdletBinding()]
    param()
    if (-not (Test-StrataElevated)) {
        throw 'Run from an elevated terminal: creating and mounting VHDX fixtures needs administrator rights.'
    }
}

function ConvertTo-LongPath {
    <#
    .SYNOPSIS
        Prefixes an absolute path with \\?\ (idempotent).
    .PARAMETER Path
        Absolute path such as X:\dir\file.
    #>
    [CmdletBinding()]
    [OutputType([string])]
    param([Parameter(Mandatory)][string]$Path)
    if ($Path.StartsWith('\\?\')) { return $Path }
    return '\\?\' + $Path
}

function Invoke-StrataNative {
    <#
    .SYNOPSIS
        Runs a native executable and throws if it exits non-zero.
    .PARAMETER FilePath
        Executable name or path.
    .PARAMETER ArgumentList
        Arguments, passed verbatim.
    .PARAMETER AllowFailure
        Return the exit code instead of throwing.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$FilePath,
        [string[]]$ArgumentList = @(),
        [switch]$AllowFailure
    )
    $output = & $FilePath @ArgumentList 2>&1
    $code = $LASTEXITCODE
    if ($code -ne 0 -and -not $AllowFailure) {
        throw "$FilePath $($ArgumentList -join ' ') exited with $code`n$($output | Out-String)"
    }
    Write-Verbose ($output | Out-String)
    return $code
}

function Get-FreeDriveLetter {
    <#
    .SYNOPSIS
        Returns an unused drive letter, scanning from Z downwards.
    #>
    [CmdletBinding()]
    [OutputType([char])]
    param()
    $used = (Get-PSDrive -PSProvider FileSystem).Name
    foreach ($c in [char[]]('ZYXWVUTSRQPONMLKJIHG')) {
        if ($used -notcontains [string]$c) { return $c }
    }
    throw 'No free drive letter.'
}

function New-StrataVhd {
    <#
    .SYNOPSIS
        Creates, mounts, partitions and formats a dynamic VHDX.
    .PARAMETER Path
        Where to create the .vhdx (must not exist).
    .PARAMETER SizeMB
        Maximum size of the virtual disk.
    .PARAMETER FileSystem
        NTFS, exFAT or ReFS.
    .PARAMETER ClusterSize
        Allocation unit in bytes (512, 4096, 65536, 2097152, ...).
    .PARAMETER Label
        Volume label.
    .OUTPUTS
        The drive letter the new volume is mounted at.
    #>
    [CmdletBinding(SupportsShouldProcess)]
    [OutputType([char])]
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][int]$SizeMB,
        [Parameter(Mandatory)][ValidateSet('NTFS', 'exFAT', 'ReFS')][string]$FileSystem,
        [Parameter(Mandatory)][int]$ClusterSize,
        [string]$Label = 'STRATA'
    )
    Assert-StrataElevated
    if (Test-Path -LiteralPath $Path) { throw "$Path already exists." }
    if (-not $PSCmdlet.ShouldProcess($Path, "create and format a $FileSystem VHDX")) { return }
    $letter = Get-FreeDriveLetter
    if (Get-Command -Name New-VHD -ErrorAction SilentlyContinue) {
        New-VHD -Path $Path -SizeBytes ([int64]$SizeMB * 1MB) -Dynamic | Out-Null
        $disk = Mount-VHD -Path $Path -Passthru | Get-Disk
        Initialize-Disk -Number $disk.Number -PartitionStyle GPT
        $part = New-Partition -DiskNumber $disk.Number -UseMaximumSize -DriveLetter $letter
        Format-Volume -Partition $part -FileSystem $FileSystem -AllocationUnitSize $ClusterSize `
            -NewFileSystemLabel $Label -Confirm:$false -Force | Out-Null
    } else {
        # NOTE: Windows Home has no Hyper-V module; diskpart's vdisk commands
        # cover the same steps.
        $script = Join-Path $env:TEMP "strata-diskpart-$([guid]::NewGuid()).txt"
        @(
            "create vdisk file=`"$Path`" maximum=$SizeMB type=expandable"
            "select vdisk file=`"$Path`""
            'attach vdisk'
            'convert gpt'
            'create partition primary'
            "format fs=$($FileSystem.ToLowerInvariant()) unit=$ClusterSize label=$Label quick"
            "assign letter=$letter"
        ) | Set-Content -LiteralPath $script -Encoding ascii
        try {
            Invoke-StrataNative -FilePath 'diskpart.exe' -ArgumentList @('/s', $script) | Out-Null
        } finally {
            Remove-Item -LiteralPath $script -ErrorAction SilentlyContinue
        }
    }
    $deadline = (Get-Date).AddSeconds(30)
    while (-not (Test-Path -LiteralPath "$($letter):\")) {
        if ((Get-Date) -gt $deadline) { throw "Volume $($letter): did not appear." }
        Start-Sleep -Milliseconds 250
    }
    return $letter
}

function Dismount-StrataVhd {
    <#
    .SYNOPSIS
        Detaches a VHDX created by New-StrataVhd.
    .PARAMETER Path
        The .vhdx path.
    #>
    [CmdletBinding()]
    param([Parameter(Mandatory)][string]$Path)
    if (Get-Command -Name Dismount-VHD -ErrorAction SilentlyContinue) {
        Dismount-VHD -Path $Path
        return
    }
    $script = Join-Path $env:TEMP "strata-diskpart-$([guid]::NewGuid()).txt"
    @("select vdisk file=`"$Path`"", 'detach vdisk') | Set-Content -LiteralPath $script -Encoding ascii
    try {
        Invoke-StrataNative -FilePath 'diskpart.exe' -ArgumentList @('/s', $script) | Out-Null
    } finally {
        Remove-Item -LiteralPath $script -ErrorAction SilentlyContinue
    }
}

Export-ModuleMember -Function Test-StrataElevated, Assert-StrataElevated, ConvertTo-LongPath,
    Invoke-StrataNative, Get-FreeDriveLetter, New-StrataVhd, Dismount-StrataVhd
