#Requires -Version 5.1
<#
.SYNOPSIS
    Non-elevated update of the BubblePlay per-user install (no UAC prompt
    unless -IncludeAgent).

.DESCRIPTION
    Copies bubbleplay-host.exe and bubbleplay_tray.exe from the build output
    into %LOCALAPPDATA%\Programs\BubblePlay, restarting the tray and its host.
    The replaced binaries are moved to <install>\previous\ (exactly one
    previous of each). Also refreshes the staged admin agent
    (bubbleplay_admin_agent.exe) and the setup/uninstall scripts beside them.
    Run setup-tray.ps1 once before the first update.

    When the host changes, update-state.json in the tray data dir records it
    as "pending": the tray rolls back to previous\bubbleplay-host.exe if the
    new host does not register with signaling within 120 s or crash-loops, and
    marks it "good" after 10 healthy minutes. Honors BUBBLEPLAY_TRAY_DATA_DIR.

    The admin input agent in %ProgramFiles%\BubblePlay\agent is a separate,
    rarely changing component with a versioned pipe protocol: host updates
    never require it to change. When the staged agent differs from the
    installed one this script only reports it; -IncludeAgent installs it with
    one UAC prompt (setup-tray.ps1 -AgentOnly). If the host ever needs a newer
    agent protocol, the tray itself offers "Admin agent needs update (one
    admin prompt)" and injects at user level until then.

.EXAMPLE
    cargo build --offline --locked --release --bins --manifest-path host/Cargo.toml
    powershell -File host/scripts/update-tray.ps1
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string]$SourceDir,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\BubblePlay'),
    [string]$AgentDir,
    [switch]$Build,
    [Alias('IncludeElevated')]
    [switch]$IncludeAgent,
    [switch]$NoStart
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Rollback-tracked binaries (kept in previous\) and plain staged files.
$Binaries = @('bubbleplay-host.exe', 'bubbleplay_tray.exe')
$AgentExeName = 'bubbleplay_admin_agent.exe'
$Scripts = @('setup-tray.ps1', 'uninstall-tray.ps1')
if (-not $SourceDir) { $SourceDir = Join-Path $PSScriptRoot '..\target\release' }
$SourceDir = [IO.Path]::GetFullPath($SourceDir)
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
if (-not $AgentDir) {
    $programFiles = if ($env:ProgramW6432) { $env:ProgramW6432 } else { $env:ProgramFiles }
    $AgentDir = Join-Path $programFiles 'BubblePlay\agent'
}
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

function Get-Sha256OrNull([string]$Path) {
    try { if (Test-Path -LiteralPath $Path -PathType Leaf) { return Get-Sha256 $Path } } catch { }
    return $null
}

if ($Build) {
    $manifest = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\Cargo.toml'))
    if (-not $env:NVIDIA_VIDEO_CODEC_SDK_PATH) { $env:NVIDIA_VIDEO_CODEC_SDK_PATH = Join-Path $PSScriptRoot '..\sdk-stub' }
    if ($PSCmdlet.ShouldProcess($manifest, 'cargo build --release (host + tray + admin agent)')) {
        & cargo build --offline --locked --release --manifest-path $manifest --bin bubbleplay-host --bin bubbleplay_tray --bin bubbleplay_admin_agent
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed with $LASTEXITCODE" }
    }
}

$sources = @{}
foreach ($name in @($Binaries + $AgentExeName)) { $sources[$name] = Join-Path $SourceDir $name }
foreach ($name in $Scripts) { $sources[$name] = Join-Path $PSScriptRoot $name }
$changed = @()
foreach ($name in @($Binaries + $AgentExeName + $Scripts)) {
    $source = $sources[$name]
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "Missing $source (build first or pass -Build)." }
    if ((Get-Sha256OrNull (Join-Path $InstallDir $name)) -ne (Get-Sha256 $source)) { $changed += $name }
}
$agentSourceHash = Get-Sha256 $sources[$AgentExeName]
$agentInstalledHash = Get-Sha256OrNull (Join-Path $AgentDir $AgentExeName)
$agentStale = $agentInstalledHash -ne $agentSourceHash
if ($agentStale -and -not $IncludeAgent) {
    if ($agentInstalledHash) {
        Write-Host "Note: the installed admin agent differs from this build's agent. That is fine: the host negotiates the agent protocol and the tray asks for an agent update only when one is required. Pass -IncludeAgent (one UAC prompt) to install it anyway."
    }
    else {
        Write-Host 'Note: the admin agent is not installed (Admin mode unavailable). Pass -IncludeAgent (one UAC prompt) or use the tray menu to install it.'
    }
}
$refreshAgent = $IncludeAgent -and $agentStale
$dataDir = if ($env:BUBBLEPLAY_TRAY_DATA_DIR) { $env:BUBBLEPLAY_TRAY_DATA_DIR } else { Join-Path $env:LOCALAPPDATA 'BubblePlay' }
if (-not (Test-Path -LiteralPath (Join-Path $dataDir 'host.json'))) {
    Write-Warning 'host.json is missing: run setup-tray.ps1 once to write the host configuration.'
}
$binariesChanged = @($changed | Where-Object { $Binaries -contains $_ })
if ($changed.Count -eq 0) {
    Write-Host "Install in $InstallDir is already current."
}
elseif ($PSCmdlet.ShouldProcess($InstallDir, "Stop tray and replace $($changed -join ', ')")) {
    if ($binariesChanged.Count) {
        $wasRunning = [bool](Get-Process bubbleplay_tray -ErrorAction SilentlyContinue)
        if ($wasRunning -and (Test-Path -LiteralPath $TrayExe)) {
            # Graceful: the tray stops its host and the admin agent.
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
    }
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $previousDir = Join-Path $InstallDir 'previous'
    New-Item -ItemType Directory -Force -Path $previousDir | Out-Null
    $hostPreviousHash = $null
    foreach ($name in $changed) {
        $target = Join-Path $InstallDir $name
        $temp = "$target.new"
        Copy-Item -Force -LiteralPath $sources[$name] -Destination $temp
        if ((Test-Path -LiteralPath $target) -and $Binaries -contains $name) {
            # Keep exactly one previous build of each binary.
            $previous = Join-Path $previousDir $name
            Move-Item -Force -LiteralPath $target -Destination $previous
            if ($name -eq 'bubbleplay-host.exe') { $hostPreviousHash = Get-Sha256 $previous }
        }
        # Pre-rollback-scheme leftovers.
        Remove-Item -Force -LiteralPath "$target.previous" -ErrorAction SilentlyContinue
        Move-Item -Force -LiteralPath $temp -Destination $target
        if ((Get-Sha256 $target) -ne (Get-Sha256 $sources[$name])) { throw "Hash mismatch after copying $name." }
    }
    if ($changed -contains 'bubbleplay-host.exe') {
        # The tray puts a pending host on probation and rolls back on failure.
        New-Item -ItemType Directory -Force -Path $dataDir | Out-Null
        $state = [ordered]@{
            status        = 'pending'
            newHash       = Get-Sha256 (Join-Path $InstallDir 'bubbleplay-host.exe')
            previousHash  = $hostPreviousHash
            updatedUnixMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
            reason        = $null
        }
        $json = $state | ConvertTo-Json
        [IO.File]::WriteAllText((Join-Path $dataDir 'update-state.json'), $json, (New-Object Text.UTF8Encoding($false)))
        if (-not $hostPreviousHash) { Write-Warning 'No previous host to roll back to; this update cannot be rolled back automatically.' }
    }
    Write-Host "Updated: $($changed -join ', ')"
}

if ($refreshAgent) {
    $setup = Join-Path $InstallDir 'setup-tray.ps1'
    if ($PSCmdlet.ShouldProcess($AgentDir, 'Install the staged admin agent (one UAC prompt: setup-tray.ps1 -AgentOnly)')) {
        $shell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
        $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$setup`"", '-AgentOnly',
            '-SourceDir', "`"$($InstallDir.TrimEnd('\'))`"", '-InstallDir', "`"$($InstallDir.TrimEnd('\'))`"")
        $elevated = Start-Process -FilePath $shell -ArgumentList $arguments -Verb RunAs -Wait -PassThru -WindowStyle Hidden
        if ($elevated.ExitCode -ne 0) { throw "Agent install failed with exit code $($elevated.ExitCode); see setup-tray-machine-*.json in %LOCALAPPDATA%\BubblePlay\receipts." }
        Write-Host "Installed the admin agent in $AgentDir."
    }
}

# Send To entry (file transfer), for installs made before it existed.
$SendToShortcut = Join-Path ([Environment]::GetFolderPath('SendTo')) 'BubblePlay.lnk'
if ((Test-Path -LiteralPath $TrayExe) -and -not (Test-Path -LiteralPath $SendToShortcut)) {
    if ($PSCmdlet.ShouldProcess($SendToShortcut, "Create Send To entry for $TrayExe --send-files")) {
        New-Item -ItemType Directory -Force -Path (Split-Path $SendToShortcut) | Out-Null
        $shell = New-Object -ComObject WScript.Shell
        $link = $shell.CreateShortcut($SendToShortcut)
        $link.TargetPath = $TrayExe
        $link.Arguments = '--send-files'
        $link.WorkingDirectory = $InstallDir
        $link.Description = 'Offer files to the connected BubblePlay viewer'
        $link.IconLocation = "$TrayExe,0"
        $link.Save()
        Write-Host "Created $SendToShortcut."
    }
}

if (-not $NoStart -and -not (Get-Process bubbleplay_tray -ErrorAction SilentlyContinue)) {
    if ($PSCmdlet.ShouldProcess($TrayExe, 'Start tray')) {
        if (-not (Test-Path -LiteralPath $TrayExe)) { throw "Tray not installed at $TrayExe; run setup-tray.ps1 first." }
        Start-Process -FilePath $TrayExe -WorkingDirectory $InstallDir
        Write-Host 'Tray started.'
    }
}
