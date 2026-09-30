#Requires -Version 5.1
<#
.SYNOPSIS
    Non-elevated update of the BubblePlay tray install (no UAC prompt).

.DESCRIPTION
    Copies bubbleplay-host.exe and bubbleplay_tray.exe from the build output
    into %LOCALAPPDATA%\Programs\BubblePlay, restarting the tray (and with it
    the user or elevated host). The previous binaries are kept as
    <name>.previous for manual rollback. Run setup-tray.ps1 once (elevated)
    before the first update.

    Admin mode runs a separate admin-owned copy in
    %ProgramFiles%\BubblePlay\elevated. When that copy differs from the build,
    this script reports it as stale; -IncludeElevated refreshes it with one
    UAC prompt (setup-tray.ps1 -ElevatedBinariesOnly).

.EXAMPLE
    cargo build --offline --locked --release --bins --manifest-path host/Cargo.toml
    powershell -File host/scripts/update-tray.ps1
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string]$SourceDir,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\BubblePlay'),
    [string]$ElevatedDir = (Join-Path $env:ProgramFiles 'BubblePlay\elevated'),
    [switch]$Build,
    [switch]$IncludeElevated,
    [switch]$NoStart
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Binaries = @('bubbleplay-host.exe', 'bubbleplay_tray.exe')
if (-not $SourceDir) { $SourceDir = Join-Path $PSScriptRoot '..\target\release' }
$SourceDir = [IO.Path]::GetFullPath($SourceDir)
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
$TrayExe = Join-Path $InstallDir 'bubbleplay_tray.exe'

$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator) -and -not $NoStart -and -not $WhatIfPreference) {
    throw 'Run update-tray.ps1 from a normal (non-elevated) PowerShell so the tray does not start elevated, or pass -NoStart.'
}

function Get-Sha256([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    try {
        $sha = [Security.Cryptography.SHA256]::Create()
        try { return (($sha.ComputeHash($stream) | ForEach-Object { $_.ToString('X2') }) -join '') } finally { $sha.Dispose() }
    }
    finally { $stream.Dispose() }
}

if ($Build) {
    $manifest = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\Cargo.toml'))
    if (-not $env:NVIDIA_VIDEO_CODEC_SDK_PATH) { $env:NVIDIA_VIDEO_CODEC_SDK_PATH = Join-Path $PSScriptRoot '..\sdk-stub' }
    if ($PSCmdlet.ShouldProcess($manifest, 'cargo build --release (host + tray)')) {
        & cargo build --offline --locked --release --manifest-path $manifest --bin bubbleplay-host --bin bubbleplay_tray
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed with $LASTEXITCODE" }
    }
}

$changed = @()
$staleElevated = @()
$elevatedInstalled = $false
foreach ($name in $Binaries) {
    $source = Join-Path $SourceDir $name
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "Missing $source (build first or pass -Build)." }
    $sourceHash = Get-Sha256 $source
    $target = Join-Path $InstallDir $name
    if (-not (Test-Path -LiteralPath $target) -or (Get-Sha256 $target) -ne $sourceHash) { $changed += $name }
    $elevatedTarget = Join-Path $ElevatedDir $name
    $elevatedHash = $null
    try { if (Test-Path -LiteralPath $elevatedTarget) { $elevatedInstalled = $true; $elevatedHash = Get-Sha256 $elevatedTarget } } catch { }
    if ($elevatedHash -ne $sourceHash) { $staleElevated += $name }
}
$refreshElevated = $IncludeElevated -and $staleElevated.Count -gt 0
if ($staleElevated.Count -gt 0 -and -not $IncludeElevated) {
    if ($elevatedInstalled) {
        Write-Warning "Admin-mode binaries in $ElevatedDir are stale ($($staleElevated -join ', ')). Rerun with -IncludeElevated (one UAC prompt) to refresh them; Admin mode keeps working on the old build meanwhile."
    }
    else {
        Write-Warning "Admin-mode binaries are not installed in $ElevatedDir (or not readable). Rerun with -IncludeElevated or run setup-tray.ps1 elevated."
    }
}
$dataDir = if ($env:BUBBLEPLAY_TRAY_DATA_DIR) { $env:BUBBLEPLAY_TRAY_DATA_DIR } else { Join-Path $env:LOCALAPPDATA 'BubblePlay' }
if (-not (Test-Path -LiteralPath (Join-Path $dataDir 'host.json'))) {
    Write-Warning 'host.json is missing: run setup-tray.ps1 (elevated, once) to migrate the host configuration.'
}
if ($changed.Count -eq 0 -and -not $refreshElevated) {
    Write-Host "Install in $InstallDir is already current."
}
elseif ($PSCmdlet.ShouldProcess($InstallDir, "Stop tray and replace $(@($changed + ($staleElevated | Where-Object { $refreshElevated } | ForEach-Object { "elevated\$_" })) -join ', ')")) {
    $wasRunning = [bool](Get-Process bubbleplay_tray -ErrorAction SilentlyContinue)
    if ($wasRunning -and (Test-Path -LiteralPath $TrayExe)) {
        # Graceful: the tray stops its host and asks the elevated runner to stop.
        $quit = Start-Process -FilePath $TrayExe -ArgumentList '--quit' -PassThru -Wait -WindowStyle Hidden
        if ($quit.ExitCode -ne 0) { Write-Warning 'Tray did not confirm shutdown in time.' }
    }
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        $busy = @(Get-Process bubbleplay_tray, bubbleplay-host -ErrorAction SilentlyContinue |
                Where-Object { $_.Path -and $_.Path.StartsWith($InstallDir, [StringComparison]::OrdinalIgnoreCase) })
        if ($busy.Count -eq 0) { break }
        Start-Sleep -Milliseconds 300
    } while ([DateTime]::UtcNow -lt $deadline)
    if ($busy.Count -gt 0) {
        # Only our own non-elevated processes can be stopped here.
        $busy | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 500
    }
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    foreach ($name in $changed) {
        $target = Join-Path $InstallDir $name
        $temp = "$target.new"
        Copy-Item -Force -LiteralPath (Join-Path $SourceDir $name) -Destination $temp
        if (Test-Path -LiteralPath $target) { Copy-Item -Force -LiteralPath $target -Destination "$target.previous" }
        Move-Item -Force -LiteralPath $temp -Destination $target
        if ((Get-Sha256 $target) -ne (Get-Sha256 (Join-Path $SourceDir $name))) { throw "Hash mismatch after copying $name." }
    }
    if ($changed.Count) { Write-Host "Updated: $($changed -join ', ')" }
    if ($refreshElevated) {
        # One UAC prompt: setup-tray.ps1 copies the admin-only copy and re-verifies its ACL.
        $shell = (Get-Process -Id $PID).Path
        $setup = Join-Path $PSScriptRoot 'setup-tray.ps1'
        $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$setup`"", '-ElevatedBinariesOnly', '-SourceDir', "`"$SourceDir`"")
        $elevated = Start-Process -FilePath $shell -ArgumentList $arguments -Verb RunAs -Wait -PassThru
        if ($elevated.ExitCode -ne 0) { throw "Elevated refresh failed with exit code $($elevated.ExitCode); see the receipt in %LOCALAPPDATA%\BubblePlay\receipts." }
        Write-Host "Refreshed Admin-mode binaries in $ElevatedDir."
    }
}

if (-not $NoStart -and -not (Get-Process bubbleplay_tray -ErrorAction SilentlyContinue)) {
    if ($PSCmdlet.ShouldProcess($TrayExe, 'Start tray')) {
        if (-not (Test-Path -LiteralPath $TrayExe)) { throw "Tray not installed at $TrayExe; run setup-tray.ps1 first." }
        Start-Process -FilePath $TrayExe -WorkingDirectory $InstallDir
        Write-Host 'Tray started.'
    }
}
