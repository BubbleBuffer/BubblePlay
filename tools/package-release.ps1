#Requires -Version 5.1
<#
.SYNOPSIS
Build and verify a public Windows tray release ZIP from an explicit allowlist.
.DESCRIPTION
Does not build, download, install, elevate, sign, or restart anything. Extra
source files are ignored. Existing output is preserved unless -Overwrite is
explicit. Supply public quickstart/notices; licensing is a separate decision.
Flat ZIP installers require -SourceDir pointing at the extracted ZIP folder.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$')]
    [string]$Version,
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9a-fA-F]{40}$')]
    [string]$SourceRevision,
    [ValidateSet('clean', 'uncommitted', 'unknown')]
    [string]$SourceState = 'unknown',
    [switch]$SourceDirty,
    [string]$BinaryDir,
    [string]$ScriptDir,
    [Parameter(Mandatory = $true)][string]$QuickstartPath,
    [Parameter(Mandatory = $true)][string]$DependencyNoticesPath,
    [string]$LicensePath,
    [Parameter(Mandatory = $true)][string]$OutputDir,
    [hashtable]$ExpectedBinaryHashes,
    [switch]$PublicRelease,
    [switch]$Overwrite
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $BinaryDir) { $BinaryDir = Join-Path $PSScriptRoot '..\host\target\release' }
if (-not $ScriptDir) { $ScriptDir = Join-Path $PSScriptRoot '..\host\scripts' }
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$utf8 = New-Object Text.UTF8Encoding($false)
$fixedTime = [DateTimeOffset]::new(1980, 1, 1, 0, 0, 0, [TimeSpan]::Zero)
if ($SourceDirty) { $SourceState = 'uncommitted' }
if ($PublicRelease -and -not $LicensePath) { throw '-PublicRelease requires an explicitly supplied product LicensePath.' }

function Assert-NoReparse([string]$Path) {
    $item = Get-Item -LiteralPath $Path -Force -ErrorAction Stop
    while ($null -ne $item) {
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Reparse/symbolic paths are not accepted: $($item.FullName)"
        }
        if ($item -is [IO.DirectoryInfo]) { $item = $item.Parent }
        else { $item = $item.Directory }
    }
}

function Assert-File([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "Required release file missing: $Path" }
    Assert-NoReparse $Path
}

function Get-StreamHash([IO.Stream]$Stream) {
    $sha = [Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($sha.ComputeHash($Stream))).Replace('-', '').ToLowerInvariant() }
    finally { $sha.Dispose() }
}

function Get-FileHashValue([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    try { return Get-StreamHash $stream } finally { $stream.Dispose() }
}

function Assert-X64Executable([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    $reader = New-Object IO.BinaryReader($stream)
    try {
        if ($stream.Length -lt 64 -or $reader.ReadUInt16() -ne 0x5a4d) { throw "Release binary is not a PE executable: $Path" }
        $stream.Position = 60
        $offset = $reader.ReadUInt32()
        if ($offset -gt 1024 * 1024 -or $offset + 26 -gt $stream.Length) { throw "Invalid PE header: $Path" }
        $stream.Position = $offset
        if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne 0x8664) { throw "Release binary is not Windows x64: $Path" }
        $stream.Position = $offset + 24
        if ($reader.ReadUInt16() -ne 0x20b) { throw "Release binary does not have a PE32+ header: $Path" }
    }
    finally { $reader.Dispose(); $stream.Dispose() }
}

$BinaryDir = [IO.Path]::GetFullPath($BinaryDir)
$ScriptDir = [IO.Path]::GetFullPath($ScriptDir)
$OutputDir = [IO.Path]::GetFullPath($OutputDir)
if ($BinaryDir.Replace('/', '\') -match '(?i)(?:^|\\)relay-staging(?:\\|$)') {
    throw 'The obsolete relay-staging host build cannot be packaged. Use the main host/target/release build.'
}
Assert-NoReparse $BinaryDir
Assert-NoReparse $ScriptDir

$sources = [ordered]@{}
foreach ($name in @('bubbleplay-host.exe', 'bubbleplay_tray.exe', 'bubbleplay_admin_agent.exe', 'bubbleplay_secure_helper.exe')) {
    $path = Join-Path $BinaryDir $name
    Assert-File $path
    Assert-X64Executable $path
    if ($ExpectedBinaryHashes) {
        if (-not $ExpectedBinaryHashes.ContainsKey($name) -or
            (Get-FileHashValue $path) -ne [string]$ExpectedBinaryHashes[$name]) { throw "Release binary hash does not match the supplied build identity: $name" }
    }
    $sources[$name] = $path
}
foreach ($name in @('setup-tray.ps1', 'update-tray.ps1', 'uninstall-tray.ps1')) {
    $sources[$name] = Join-Path $ScriptDir $name
}
$sources['QUICKSTART.md'] = [IO.Path]::GetFullPath($QuickstartPath)
$sources['THIRD-PARTY-NOTICES.md'] = [IO.Path]::GetFullPath($DependencyNoticesPath)
if ($LicensePath) { $sources['LICENSE'] = [IO.Path]::GetFullPath($LicensePath) }
foreach ($path in $sources.Values) { Assert-File $path }
foreach ($name in @('QUICKSTART.md', 'THIRD-PARTY-NOTICES.md', 'LICENSE')) {
    if ($sources.Contains($name) -and (Get-Item -LiteralPath $sources[$name]).Length -eq 0) { throw "Release document is empty: $name" }
}

# Resolve and inspect every existing ancestor before creating an output folder.
$ancestor = $OutputDir
while (-not (Test-Path -LiteralPath $ancestor)) {
    $parent = [IO.Path]::GetDirectoryName($ancestor)
    if (-not $parent -or $parent -eq $ancestor) { throw 'Output directory has no existing ancestor.' }
    $ancestor = $parent
}
Assert-NoReparse $ancestor
if ((Test-Path -LiteralPath $OutputDir) -and -not (Test-Path -LiteralPath $OutputDir -PathType Container)) { throw 'OutputDir is not a directory.' }
$destination = Join-Path $OutputDir ("BubblePlay-{0}-windows-x64.zip" -f $Version)
if (Test-Path -LiteralPath $destination) {
    Assert-File $destination
    if (-not $Overwrite) { throw "Output already exists; choose a new version or explicitly pass -Overwrite: $destination" }
}

$names = [string[]]$sources.Keys
[Array]::Sort($names, [StringComparer]::Ordinal)
$files = @($names | ForEach-Object {
    [ordered]@{ path = $_; size = (Get-Item -LiteralPath $sources[$_]).Length; sha256 = Get-FileHashValue $sources[$_] }
})
$manifest = [ordered]@{
    schema = 'bubbleplay.public-release.v1'
    version = $Version
    platform = 'windows-x64'
    profile = 'per-user-tray'
    sourceRevision = $SourceRevision.ToLowerInvariant()
    sourceState = $SourceState
    sourceDirty = $(if ($SourceState -eq 'unknown') { $null } else { $SourceState -eq 'uncommitted' })
    releaseIntent = $(if ($PublicRelease) { 'public' } else { 'candidate' })
    productLicenseIncluded = [bool]$LicensePath
    files = $files
}
$manifestBytes = $utf8.GetBytes(($manifest | ConvertTo-Json -Depth 6 -Compress) + "`n")
$manifestStream = New-Object IO.MemoryStream(,$manifestBytes)
try { $manifestHash = Get-StreamHash $manifestStream } finally { $manifestStream.Dispose() }

[void][IO.Directory]::CreateDirectory($OutputDir)
Assert-NoReparse $OutputDir
$temporary = Join-Path $OutputDir ('.package-' + [Guid]::NewGuid().ToString('N') + '.tmp')
try {
    $stream = [IO.File]::Open($temporary, [IO.FileMode]::CreateNew, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    $archive = New-Object IO.Compression.ZipArchive($stream, [IO.Compression.ZipArchiveMode]::Create, $false)
    try {
        $entries = [string[]]@($names + 'release-manifest.json')
        [Array]::Sort($entries, [StringComparer]::Ordinal)
        foreach ($name in $entries) {
            $entry = $archive.CreateEntry($name, [IO.Compression.CompressionLevel]::Optimal)
            $entry.LastWriteTime = $fixedTime
            $entry.ExternalAttributes = 0
            $target = $entry.Open()
            try {
                if ($name -eq 'release-manifest.json') { $target.Write($manifestBytes, 0, $manifestBytes.Length) }
                else {
                    Assert-File $sources[$name]
                    $source = [IO.File]::OpenRead($sources[$name])
                    try { $source.CopyTo($target) } finally { $source.Dispose() }
                }
            }
            finally { $target.Dispose() }
        }
    }
    finally { $archive.Dispose(); $stream.Dispose() }

    # Reopen the finished ZIP and independently verify the full entry allowlist,
    # sizes and hashes, including the manifest's bytes. Fail before publishing.
    $verified = [IO.Compression.ZipFile]::OpenRead($temporary)
    try {
        if ($verified.Entries.Count -ne $entries.Count) { throw 'Unexpected ZIP entry count.' }
        $seen = New-Object 'Collections.Generic.HashSet[string]' ([StringComparer]::Ordinal)
        foreach ($entry in $verified.Entries) {
            if ($entries -cnotcontains $entry.FullName -or -not $seen.Add($entry.FullName)) { throw "Non-allowlisted/duplicate ZIP entry: $($entry.FullName)" }
            $expected = if ($entry.FullName -eq 'release-manifest.json') {
                [pscustomobject]@{ size = $manifestBytes.Length; sha256 = $manifestHash }
            } else { $files | Where-Object { $_.path -ceq $entry.FullName } }
            if ($entry.Length -ne $expected.size) { throw "ZIP size mismatch: $($entry.FullName)" }
            $content = $entry.Open()
            try { if ((Get-StreamHash $content) -cne $expected.sha256) { throw "ZIP hash mismatch: $($entry.FullName)" } }
            finally { $content.Dispose() }
        }
    }
    finally { $verified.Dispose() }

    Assert-NoReparse $OutputDir
    if (Test-Path -LiteralPath $destination) {
        if (-not $Overwrite) { throw 'Output appeared while packaging; refusing to replace it.' }
        Assert-File $destination
        $backup = Join-Path $OutputDir ('.previous-' + [Guid]::NewGuid().ToString('N') + '.tmp')
        [IO.File]::Replace($temporary, $destination, $backup)
        Remove-Item -LiteralPath $backup -Force
    } else { [IO.File]::Move($temporary, $destination) }
    [pscustomobject]@{ path = $destination; sha256 = Get-FileHashValue $destination; entries = $entries.Count; version = $Version }
}
finally {
    if (Test-Path -LiteralPath $temporary -PathType Leaf) { Remove-Item -LiteralPath $temporary -Force }
}
