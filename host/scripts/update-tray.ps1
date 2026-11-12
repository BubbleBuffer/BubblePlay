#Requires -Version 5.1
<#
.SYNOPSIS
    Update the protected Program Files BubblePlay installation with one UAC prompt.
.DESCRIPTION
    Run as the desktop user. Setup copies and verifies the selected package,
    including optional Flutter DLLs/assets, before running protected machine
    registration. HKCU, config and receipts belong to the unelevated parent.
    -IncludeAgent is retained for compatibility; full updates refresh both helpers.
    -AppOnly updates only the optional app in an existing protected installation.
    Protected program files cannot be rolled back by the unelevated tray; install
    a previous trusted package with this script to roll back.
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string]$SourceDir,
    [string]$InstallDir,
    [string]$AgentDir,
    [string]$AppDir,
    [switch]$AppOnly,
    [switch]$Build,
    [Alias('IncludeElevated')][switch]$IncludeAgent,
    [switch]$NoStart
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not [Environment]::Is64BitProcess) { throw 'Use 64-bit PowerShell for the Windows x64 BubblePlay package.' }
$machineDir = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'BubblePlay'
if ($InstallDir -and [IO.Path]::GetFullPath($InstallDir).TrimEnd('\') -ine $machineDir.TrimEnd('\')) { throw "InstallDir must be $machineDir." }
if ($AgentDir -and [IO.Path]::GetFullPath($AgentDir).TrimEnd('\') -ine (Join-Path $machineDir 'agent')) { throw 'AgentDir must be the protected Program Files agent subdirectory.' }
$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator) -and -not $WhatIfPreference) { throw 'Run update from a normal desktop PowerShell; setup elevates only its machine child.' }
if (-not $SourceDir) {
    $SourceDir = if (Test-Path -LiteralPath (Join-Path $PSScriptRoot 'bubbleplay-host.exe')) { $PSScriptRoot } else { Join-Path $PSScriptRoot '..\target\release' }
}
$SourceDir = [IO.Path]::GetFullPath($SourceDir)
if (-not $AppDir -and -not (Test-Path -LiteralPath (Join-Path $SourceDir 'release-manifest.json'))) {
    $devApp = Join-Path $PSScriptRoot '..\..\app\build\windows\x64\runner\Release'
    if (Test-Path -LiteralPath (Join-Path $devApp 'bubbleplay_app.exe') -PathType Leaf) { $AppDir = $devApp }
}
if ($Build) {
    $manifest = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\Cargo.toml'))
    if ($PSCmdlet.ShouldProcess($manifest, 'Build host, tray and privileged helpers')) {
        if (-not $env:NVIDIA_VIDEO_CODEC_SDK_PATH) { $env:NVIDIA_VIDEO_CODEC_SDK_PATH = Join-Path $PSScriptRoot '..\sdk-stub' }
        Push-Location (Split-Path -Parent $manifest)
        try {
            & cargo build --offline --locked --release --manifest-path $manifest --bin bubbleplay-host --bin bubbleplay_tray --bin bubbleplay_admin_agent --bin bubbleplay_secure_helper
            if ($LASTEXITCODE -ne 0) { throw "cargo build failed with $LASTEXITCODE" }
        }
        finally { Pop-Location }
    }
}
# A selected release brings its matching installer. A source checkout keeps
# scripts separate from host/target/release. Both run unelevated initially.
$setup = Join-Path $SourceDir 'setup-tray.ps1'
if (-not (Test-Path -LiteralPath $setup -PathType Leaf)) { $setup = Join-Path $PSScriptRoot 'setup-tray.ps1' }
$parameters = @{ SourceDir = $SourceDir; InstallDir = $machineDir; NoLaunch = [bool]$NoStart; WhatIf = [bool]$WhatIfPreference }
if ($AppDir) { $parameters.AppDir = [IO.Path]::GetFullPath($AppDir) }
if ($AppOnly) { $parameters.AppOnly = $true }
if ($IncludeAgent) { $parameters.IncludeAgent = $true }
& $setup @parameters
