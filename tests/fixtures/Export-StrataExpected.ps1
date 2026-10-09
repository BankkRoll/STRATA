#Requires -Version 5.1
<#
.SYNOPSIS
    Writes the filesystem's own view of a volume as strata-expected/1 JSON.

.DESCRIPTION
    Walks the volume with FindFirstFileExW (never following reparse points)
    and records, per path: kind, logical size, FILE_STANDARD_INFO allocation
    size, GetCompressedFileSizeW, Win32 attributes, reparse tag, link count
    and named streams. Compare-StrataGolden.ps1 checks the MFT scan against
    this, independently of Strata's own code.

.PARAMETER Root
    Volume root (X:\) or folder.

.PARAMETER OutFile
    Output JSON path.

.EXAMPLE
    .\Export-StrataExpected.ps1 -Root Z:\ -OutFile .\out\ntfs-4k.expected.json
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Root,
    [Parameter(Mandatory)][string]$OutFile
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not ('StrataNative' -as [type])) {
    Add-Type -TypeDefinition (Get-Content -LiteralPath (Join-Path $PSScriptRoot 'StrataNative.cs') -Raw)
}

$entries = foreach ($e in [StrataNative]::Walk($Root)) {
    [ordered]@{
        path            = $e.path
        kind            = $e.kind
        logical         = $e.logical
        allocation_size = $e.allocation_size
        compressed_size = $e.compressed_size
        attributes      = $e.attributes
        reparse_tag     = $e.reparse_tag
        link_count      = $e.link_count
        streams         = @($e.streams | ForEach-Object {
                # Win32 reports ":name:$DATA"; the scanner reports the bare name.
                [ordered]@{ name = ($_.Key -replace '^:', '' -replace ':\$DATA$', ''); logical = $_.Value }
            })
        error           = $e.error
    }
}
$doc = [ordered]@{
    format  = 'strata-expected/1'
    root    = $Root
    entries = @($entries | Sort-Object { $_.path } -CaseSensitive)
}
# NOTE: UTF-8 output turns unpaired surrogates into U+FFFD, the same lossy
# form strata-cli writes for display paths, so such names still compare.
$doc | ConvertTo-Json -Depth 6 -Compress | Set-Content -LiteralPath $OutFile -Encoding utf8
Write-Output "Wrote $($doc.entries.Count) entries to $OutFile"
