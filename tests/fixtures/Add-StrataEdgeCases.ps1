#Requires -Version 5.1
<#
.SYNOPSIS
    Populates a volume (or any empty folder) with filesystem edge cases.

.DESCRIPTION
    Creates hardlinks (including one file with 1023 extra links), junctions,
    symlinks, sparse files, NTFS-compressed files and folders, WOF-compressed
    files, ADS (many, and one large), EFS-encrypted files, paths over 260
    characters, a tree deeper than 1000 levels, trailing dots and spaces,
    leading spaces, reserved device names, a case-sensitive directory holding
    A.txt and a.txt, unicode/emoji/RTL names, an unpaired-surrogate name and
    many small files.

    Features the target filesystem does not support (exFAT has no ADS or
    hardlinks, ReFS has no NTFS compression, ...) are skipped. Every step is
    recorded in the manifest so the comparison knows what to expect.

    Symlink creation needs elevation or Developer Mode; everything else works
    unelevated on a plain NTFS folder, which is how the script is smoke-tested
    without mounting a VHDX.

.PARAMETER Root
    Volume root (X:\) or an empty folder.

.PARAMETER SmallFiles
    How many small files to create under \many.

.PARAMETER DeepLevels
    Depth of the \deep tree.

.PARAMETER ManifestPath
    Where to write the manifest (kept off the fixture volume so it does not
    appear in the scan). Defaults to manifest.json in the current folder.

.EXAMPLE
    .\Add-StrataEdgeCases.ps1 -Root Z:\ -ManifestPath .\out\ntfs-4k.manifest.json
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Root,
    [int]$SmallFiles = 20000,
    [int]$DeepLevels = 1100,
    [string]$ManifestPath = (Join-Path (Get-Location) 'manifest.json')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'StrataFixtures.psm1') -Force
if (-not ('StrataNative' -as [type])) {
    Add-Type -TypeDefinition (Get-Content -LiteralPath (Join-Path $PSScriptRoot 'StrataNative.cs') -Raw)
}

$Root = $Root.TrimEnd('\') + '\'
$long = ConvertTo-LongPath $Root
$fs = try { (Get-Volume -FilePath $Root -ErrorAction Stop).FileSystemType } catch { 'NTFS' }
$steps = [ordered]@{}

function Invoke-Step {
    param([string]$Name, [scriptblock]$Body)
    try {
        & $Body
        $steps[$Name] = 'created'
    } catch {
        Write-Warning "skipped ${Name}: $($_.Exception.Message)"
        $steps[$Name] = "skipped: $($_.Exception.Message)"
    }
}

function Get-ByteBuffer([int]$Count, [byte]$Value = 0x61) {
    $b = [byte[]]::new($Count)
    if ($Value -ne 0) { for ($i = 0; $i -lt $Count; $i++) { $b[$i] = $Value } }
    return , $b
}

function Write-TestFile([string]$Rel, [int]$Size, [byte]$Value = 0x61) {
    [StrataNative]::File($long + $Rel, (Get-ByteBuffer $Size $Value))
}

Invoke-Step 'basic' {
    [StrataNative]::Dir($long + 'basic')
    Write-TestFile 'basic\empty.txt' 0
    Write-TestFile 'basic\resident.txt' 100
    Write-TestFile 'basic\one-cluster.bin' 4096
    Write-TestFile 'basic\large.bin' (5MB + 123)
}

Invoke-Step 'hardlinks' {
    [StrataNative]::Dir($long + 'links')
    [StrataNative]::Dir($long + 'links\a')
    [StrataNative]::Dir($long + 'links\b')
    Write-TestFile 'links\a\original.bin' 70000
    [StrataNative]::HardLink($long + 'links\b\second.bin', $long + 'links\a\original.bin')
    [StrataNative]::HardLink($long + 'links\third.bin', $long + 'links\a\original.bin')
}

Invoke-Step 'hardlinks-1024' {
    [StrataNative]::Dir($long + 'links1024')
    Write-TestFile 'links1024\target.bin' 1000
    # NTFS allows 1024 links per file: the original plus 1023 more.
    for ($i = 1; $i -le 1023; $i++) {
        [StrataNative]::HardLink($long + "links1024\l$i", $long + 'links1024\target.bin')
    }
}

Invoke-Step 'junction' {
    [StrataNative]::Dir($long + 'reparse')
    New-Item -ItemType Junction -Path ($Root + 'reparse\junction-to-basic') -Target ($Root + 'basic') | Out-Null
    # A junction pointing at its own ancestor: an apparent cycle that must not be followed.
    New-Item -ItemType Junction -Path ($Root + 'reparse\loop') -Target ($Root + 'reparse') | Out-Null
}

Invoke-Step 'symlinks' {
    [StrataNative]::Dir($long + 'reparse')
    New-Item -ItemType SymbolicLink -Path ($Root + 'reparse\file-link.bin') -Target ($Root + 'basic\large.bin') | Out-Null
    New-Item -ItemType SymbolicLink -Path ($Root + 'reparse\dir-link') -Target ($Root + 'basic') | Out-Null
}

Invoke-Step 'sparse' {
    [StrataNative]::Dir($long + 'sparse')
    $p = $Root + 'sparse\sparse-100g.bin'
    Write-TestFile 'sparse\sparse-100g.bin' 0
    Invoke-StrataNative fsutil.exe @('sparse', 'setflag', $p) | Out-Null
    Invoke-StrataNative fsutil.exe @('file', 'seteof', $p, '107374182400') | Out-Null
    $s = [IO.File]::Open($p, 'Open', 'Write', 'ReadWrite')
    try { $s.Write((Get-ByteBuffer 1048576 0x5A), 0, 1048576) } finally { $s.Dispose() }
}

Invoke-Step 'ntfs-compression' {
    [StrataNative]::Dir($long + 'compressed')
    $text = [Text.Encoding]::ASCII.GetBytes(('compressible line of text ' * 40000))
    [StrataNative]::File($long + 'compressed\file.txt', $text)
    Invoke-StrataNative compact.exe @('/c', ($Root + 'compressed\file.txt')) | Out-Null
    [StrataNative]::Dir($long + 'compressed\folder')
    Invoke-StrataNative compact.exe @('/c', ($Root + 'compressed\folder')) | Out-Null
    [StrataNative]::File($long + 'compressed\folder\inherits.txt', $text)
}

Invoke-Step 'wof-compression' {
    [StrataNative]::Dir($long + 'wof')
    $text = [Text.Encoding]::ASCII.GetBytes(('wof compressible ' * 60000))
    [StrataNative]::File($long + 'wof\program.dat', $text)
    Invoke-StrataNative compact.exe @('/c', '/exe:lzx', ($Root + 'wof\program.dat')) | Out-Null
}

Invoke-Step 'ads' {
    [StrataNative]::Dir($long + 'ads')
    Write-TestFile 'ads\host.txt' 10
    for ($i = 0; $i -lt 200; $i++) { Write-TestFile ("ads\host.txt:s{0:D3}" -f $i) ($i + 1) }
    Write-TestFile 'ads\download.exe' 2000
    [StrataNative]::File($long + 'ads\download.exe:Zone.Identifier',
        [Text.Encoding]::ASCII.GetBytes("[ZoneTransfer]`r`nZoneId=3`r`n"))
    Write-TestFile 'ads\big-stream.txt' 1
    Write-TestFile 'ads\big-stream.txt:huge' (64MB)
}

Invoke-Step 'efs' {
    [StrataNative]::Dir($long + 'efs')
    Write-TestFile 'efs\secret.txt' 5000
    Invoke-StrataNative cipher.exe @('/e', ($Root + 'efs\secret.txt')) | Out-Null
}

Invoke-Step 'long-paths' {
    $rel = 'long'
    [StrataNative]::Dir($long + $rel)
    for ($i = 0; $i -lt 6; $i++) {
        $rel += '\' + ('segment{0}-' -f $i) + ('x' * 50)
        [StrataNative]::Dir($long + $rel)
    }
    Write-TestFile ($rel + '\beyond-max-path.txt') 300
}

Invoke-Step 'deep-tree' {
    $rel = 'deep'
    [StrataNative]::Dir($long + $rel)
    for ($i = 0; $i -lt $DeepLevels; $i++) {
        $rel += '\d'
        [StrataNative]::Dir($long + $rel)
    }
    Write-TestFile ($rel + '\bottom.txt') 42
}

Invoke-Step 'odd-names' {
    [StrataNative]::Dir($long + 'names')
    foreach ($n in @('trailing dot.', 'trailing space ', ' leading space', 'CON', 'nul.txt', 'COM1.log', 'AUX')) {
        Write-TestFile ('names\' + $n) 7
    }
    $emoji = [char]::ConvertFromUtf32(0x1F600) + [char]::ConvertFromUtf32(0x1F680)
    $rtl = [string]::new([char[]](0x05E9, 0x05DC, 0x05D5, 0x05DD)) + ' ' + [string]::new([char[]](0x0645, 0x0631, 0x062D, 0x0628, 0x0627))
    foreach ($n in @("unicode-$([char]0x00E9)t$([char]0x00E9).txt", "emoji-$emoji.txt", "rtl-$rtl.txt", "cjk-$([char]0x6587)$([char]0x4EF6).txt")) {
        Write-TestFile ('names\' + $n) 11
    }
}

Invoke-Step 'unpaired-surrogate' {
    [StrataNative]::Dir($long + 'names')
    Write-TestFile ("names\lone-$([char]0xD800)-high.txt") 3
    Write-TestFile ("names\lone-$([char]0xDC00)-low.txt") 3
}

Invoke-Step 'case-sensitive' {
    [StrataNative]::Dir($long + 'case')
    Invoke-StrataNative fsutil.exe @('file', 'setCaseSensitiveInfo', ($Root + 'case'), 'enable') | Out-Null
    # NOTE: without the WSL optional feature fsutil can exit 0 while leaving
    # the flag off, and a.txt would then silently overwrite A.txt.
    $state = & fsutil.exe file queryCaseSensitiveInfo ($Root + 'case') 2>&1 | Out-String
    if ($state -notmatch 'enabled') {
        throw "case sensitivity did not enable (needs the WSL feature): $($state.Trim())"
    }
    Write-TestFile 'case\A.txt' 1
    Write-TestFile 'case\a.txt' 2
}

Invoke-Step 'many-small-files' {
    [StrataNative]::Dir($long + 'many')
    for ($d = 0; $d -lt [math]::Ceiling($SmallFiles / 1000); $d++) {
        [StrataNative]::Dir($long + "many\$d")
        for ($i = 0; $i -lt 1000 -and ($d * 1000 + $i) -lt $SmallFiles; $i++) {
            Write-TestFile ("many\$d\f$i.txt") ($i % 700)
        }
    }
}

$manifest = [ordered]@{
    format      = 'strata-fixture-manifest/1'
    root        = $Root
    file_system = $fs
    created     = (Get-Date).ToUniversalTime().ToString('o')
    steps       = $steps
}
$manifest | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $ManifestPath -Encoding utf8
Write-Output "Populated $Root ($fs). Manifest: $ManifestPath"
