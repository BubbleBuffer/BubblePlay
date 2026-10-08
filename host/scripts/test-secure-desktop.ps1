#Requires -Version 5.1
<#
.SYNOPSIS
    Owner-run, unattended secure-desktop qualification from normal PowerShell.
.DESCRIPTION
    Requires an installed protocol-v2 bubbleplay_secure_helper.exe and its
    protected \BubblePlay\SecureDesktop task. Build --release --bins and update
    the installed helpers using update-tray.ps1 -IncludeAgent before this test.
    This script does not elevate itself, install files, or restart the tray/host.

    Disconnect every live viewer first and do not launch other local UAC prompts
    or change tray settings until the probe completes. Prompt ownership is inferred from a clean baseline
    and one consent process while this runas request is pending, not proven by a
    Windows originating-process identifier. The probe checks the host log and claims
    the real authenticated exclusive helper Host slot (Busy aborts) before any
    configuration change or UAC request. If the helper is absent it starts the
    already-installed task through System32\schtasks.exe /run as the normal user.

    Creates two harmless, cancellable UAC requests: runas cmd.exe /c exit 0.
    Sends only Escape down/up, absolute mouse MOVE, and DenyConsent; never
    approval keys or clicks. OFF must reject both input requests and preserve
    UAC for 2 seconds, then Deny must close it. ON must apply every MOVE and
    Escape record, save fresh center/corner PNGs, and close UAC with Escape.

    Backups/restores tray.json byte-for-byte (including initially absent files).
    The probe holds the Host pipe until cleanup and configuration restoration.
    A late/pending runas request must settle before cleanup can pass; a timeout
    reports cleanup failure. On failure, inspect summary.json and dismiss any
    reported outstanding prompt locally before retrying.

    Stdout is the JSON summary. summary.json, PNGs and probe stdout/stderr logs
    are written to -OutDir. Exit 0 requires both scenarios, confirmed cleanup,
    and exact configuration restoration. Use a fresh output directory each run.
.EXAMPLE
    powershell -NoProfile -File host/scripts/test-secure-desktop.ps1 -OutDir artifacts/secure-desktop-owner-test
#>
[CmdletBinding()]
param(
    [string]$ExePath = (Join-Path $PSScriptRoot '..\target\release\probe_secure_helper.exe'),
    [Parameter(Mandatory = $true)][ValidateNotNullOrEmpty()][string]$OutDir
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$exitCode = 1
$failure = $null
$summary = $null
$originalBytes = $null
$originalExists = $false
$backupValid = $false
$restored = $false
$mutationAuthorized = $false
$runId = [Guid]::NewGuid().ToString('N')
$markerPath = $null
$settings = Join-Path $env:LOCALAPPDATA 'BubblePlay\tray.json'
$hostLog = Join-Path $env:LOCALAPPDATA 'BubblePlay\logs\host.log'
$ExePath = [IO.Path]::GetFullPath($ExePath)
$OutDir = [IO.Path]::GetFullPath($OutDir).TrimEnd('\')
$summaryPath = Join-Path $OutDir 'summary.json'
$markerPath = Join-Path $OutDir 'mutation-authorized.json'
try {
    $principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Run from a normal, non-elevated PowerShell.' }
    if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) { throw "Missing $ExePath. Build the protocol-v2 probe first." }
    if (Test-Path -LiteralPath $summaryPath) { throw 'summary.json already exists; choose a new -OutDir.' }
    $originalExists = Test-Path -LiteralPath $settings -PathType Leaf
    if ($originalExists) {
        $originalBytes = [IO.File]::ReadAllBytes($settings)
        if ($originalBytes.Length -gt 65536) { throw 'tray.json exceeds the helper configuration limit.' }
        $original = [Text.Encoding]::UTF8.GetString($originalBytes) | ConvertFrom-Json
        if ($null -eq $original -or $original -isnot [pscustomobject]) { throw 'tray.json must be a valid JSON object.' }
        if ($original.PSObject.Properties['adminMode'] -and $original.adminMode -isnot [bool]) { throw 'tray.json adminMode must be boolean.' }
    }
    $backupValid = $true
    [void][IO.Directory]::CreateDirectory($OutDir)
    $arguments = @('--out-dir', "`"$OutDir`"", '--tray-settings', "`"$settings`"", '--host-log', "`"$hostLog`"", '--run-id', $runId)
    $probe = Start-Process -FilePath $ExePath -ArgumentList $arguments -Wait -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput (Join-Path $OutDir 'probe.stdout.log') -RedirectStandardError (Join-Path $OutDir 'probe.stderr.log')
    $exitCode = $probe.ExitCode
    if (-not (Test-Path -LiteralPath $summaryPath -PathType Leaf)) { throw "Probe exited $exitCode without summary.json; inspect probe.stderr.log." }
    $summary = [IO.File]::ReadAllText($summaryPath) | ConvertFrom-Json
    if (-not $summary.success -or @($summary.scenarios).Count -ne 2 -or @($summary.scenarios | Where-Object { -not $_.success }).Count -gt 0 -or
        -not $summary.cleanup.success -or -not $summary.config.restored -or $exitCode -ne 0) { $exitCode = 1; $summary.success = $false }
}
catch { $failure = $_.Exception.Message; $exitCode = 1 }
finally {
    if ($backupValid) {
        try {
            if (Test-Path -LiteralPath $markerPath -PathType Leaf) {
                $marker = [IO.File]::ReadAllText($markerPath) | ConvertFrom-Json
                $mutationAuthorized = $marker.run_id -eq $runId -and $marker.exclusive_host_claimed -and
                    [string]::Equals([IO.Path]::GetFullPath($marker.settings_path), [IO.Path]::GetFullPath($settings), [StringComparison]::OrdinalIgnoreCase)
            }
            # Busy/preflight/start failures did not claim permission to mutate.
            # Preserve concurrent settings unless this exact run's marker exists.
            if (-not $mutationAuthorized) { $restored = $true }
            elseif ($originalExists) {
                $current = if (Test-Path -LiteralPath $settings -PathType Leaf) { [IO.File]::ReadAllBytes($settings) } else { $null }
                $same = $null -ne $current -and $current.Length -eq $originalBytes.Length
                if ($same) { for ($i = 0; $i -lt $current.Length; $i++) { if ($current[$i] -ne $originalBytes[$i]) { $same = $false; break } } }
                if (-not $same) { [IO.File]::WriteAllBytes($settings, $originalBytes) }
                $verify = [IO.File]::ReadAllBytes($settings)
                if ($verify.Length -ne $originalBytes.Length) { throw 'tray.json restoration length mismatch.' }
                for ($i = 0; $i -lt $verify.Length; $i++) { if ($verify[$i] -ne $originalBytes[$i]) { throw 'tray.json restoration byte mismatch.' } }
            }
            elseif (Test-Path -LiteralPath $settings) {
                # Only the explicitly named file created by this probe is removed.
                if (-not (Test-Path -LiteralPath $settings -PathType Leaf)) { throw 'tray.json became a directory; refusing removal.' }
                [IO.File]::Delete($settings)
            }
            $restored = $true
        }
        catch { $failure = "Configuration restoration failed: $($_.Exception.Message)"; $exitCode = 1 }
    }
}
if ($null -eq $summary) {
    $summary = [pscustomobject]@{ schema = 'bubbleplay.secure-desktop-probe.v1'; success = $false; error = $failure }
}
$summary | Add-Member -NotePropertyName script_config_backup -NotePropertyValue ([pscustomobject]@{
    original_exists = $originalExists; read_valid = $backupValid; restored = $restored; mutation_authorized = $mutationAuthorized
}) -Force
if ($failure) { $summary | Add-Member -NotePropertyName script_error -NotePropertyValue $failure -Force; $summary.success = $false }
if (-not $restored) { $summary.success = $false; $exitCode = 1 }
$json = $summary | ConvertTo-Json -Depth 16
# Preserve any previous artifact on a preflight failure; never replace another run.
if ($backupValid -and (Test-Path -LiteralPath $OutDir -PathType Container)) {
    try { [IO.File]::WriteAllText($summaryPath, $json, [Text.UTF8Encoding]::new($false)) }
    catch { $summary.success = $false; $exitCode = 1; $summary | Add-Member -NotePropertyName script_error -NotePropertyValue "Cannot write summary: $($_.Exception.Message)" -Force; $json = $summary | ConvertTo-Json -Depth 16 }
}
Write-Output $json
exit $exitCode
