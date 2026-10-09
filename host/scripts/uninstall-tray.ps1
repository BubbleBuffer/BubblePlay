#Requires -Version 5.1
<#
.SYNOPSIS
    Uninstall BubblePlay for the current user (the "Apps" entry runs this).

.DESCRIPTION
    Elevated part (ONE UAC prompt, only when something machine-wide is
    installed): unregister the \BubblePlay\AdminAgent and \BubblePlay\SecureDesktop tasks, delete
    %ProgramFiles%\BubblePlay\agent, the AdminModeOff task and
    HKLM\SOFTWARE\BubblePlay policy key, and remove leftovers of the old elevated
    host path (\BubblePlay\ElevatedHost, %ProgramFiles%\BubblePlay\elevated).
    %ProgramFiles%\BubblePlay is removed when it ends up empty.

    User part (no elevation): quit the tray (its host and agent exit with
    it), remove the HKCU Run value, the Start-menu shortcut, the HKCU
    uninstall entry and %LOCALAPPDATA%\Programs\BubblePlay. The data folder
    %LOCALAPPDATA%\BubblePlay (host.json with DPAPI-protected tokens, logs,
    receipts) is kept unless -RemoveData is given.

    -WhatIf previews everything without elevating or changing anything.
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\BubblePlay'),
    [switch]$RemoveData,
    # Internal: the elevated half, started by this script through one UAC prompt.
    [switch]$MachinePhase,
    # Internal: this copy runs from %TEMP% so the install folder can be deleted.
    [switch]$Relocated
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$ProgramFilesRoot = if ($env:ProgramW6432) { $env:ProgramW6432 } else { $env:ProgramFiles }
$MachineDir = Join-Path $ProgramFilesRoot 'BubblePlay'
$AgentDir = Join-Path $MachineDir 'agent'
$LegacyElevatedDir = Join-Path $MachineDir 'elevated'
$TaskPath = '\BubblePlay\'
$Tasks = @('AdminAgent', 'SecureDesktop', 'AdminModeOff', 'ElevatedHost')
$AdminModeRegistryPath = 'SOFTWARE\BubblePlay'
$AdminModeKey = 'HKLM:\SOFTWARE\BubblePlay'
$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$RunValue = 'BubblePlayTray'
$UninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\BubblePlay'
$StartMenuShortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'BubblePlay.lnk'
$SendToShortcut = Join-Path ([Environment]::GetFolderPath('SendTo')) 'BubblePlay.lnk'
$DataDir = Join-Path $env:LOCALAPPDATA 'BubblePlay'
$PowerShellExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
$InstallDir = [IO.Path]::GetFullPath($InstallDir).TrimEnd('\')
$IsWhatIf = [bool]$WhatIfPreference
$IsElevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)

function Write-Step([string]$Name, [string]$Status, [string]$Detail) {
    $label = if ($IsWhatIf -and $Status -eq 'planned') { 'PLAN' } else { $Status.ToUpperInvariant() }
    Write-Host ("[{0,-9}] {1}: {2}" -f $label, $Name, $Detail)
}

function Invoke-Schtasks([string[]]$Arguments) {
    $exe = Join-Path $env:SystemRoot 'System32\schtasks.exe'
    # Native stderr ("task not found") must not become a terminating error.
    $ErrorActionPreference = 'Continue'
    $null = & $exe @Arguments 2>&1
    return $LASTEXITCODE
}

function Test-Task([string]$Name) { (Invoke-Schtasks @('/query', '/tn', "$TaskPath$Name")) -eq 0 }

function Test-AdminModeKeyExists {
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, [Microsoft.Win32.RegistryView]::Registry64)
    try {
        $key = $base.OpenSubKey($AdminModeRegistryPath, $false)
        if ($null -eq $key) { return $false }
        $key.Dispose()
        return $true
    }
    catch { return $true } # Inaccessible policy still requires elevated removal.
    finally { $base.Dispose() }
}

function Get-MachineWork {
    $work = @()
    foreach ($name in $Tasks) { if (Test-Task $name) { $work += "unregister task $TaskPath$name" } }
    foreach ($dir in @($AgentDir, $LegacyElevatedDir)) { if (Test-Path -LiteralPath $dir) { $work += "delete $dir" } }
    if (Test-AdminModeKeyExists) { $work += "delete $AdminModeKey" }
    return , $work
}

function Remove-WithRetry([string]$Path) {
    $resolved = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $allowed = @($AgentDir, $LegacyElevatedDir, $InstallDir, $DataDir) | ForEach-Object { [IO.Path]::GetFullPath($_).TrimEnd('\') }
    if ($allowed -notcontains $resolved) { throw "Refusing unexpected delete target: $resolved" }
    if ((Test-Path -LiteralPath $resolved) -and ((Get-Item -LiteralPath $resolved -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw "Refusing reparse-point delete: $resolved" }
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    while (Test-Path -LiteralPath $Path) {
        try { Remove-Item -Recurse -Force -LiteralPath $Path; return }
        catch { if ([DateTime]::UtcNow -gt $deadline) { throw }; Start-Sleep -Milliseconds 500 }
    }
}

function Invoke-MachineSteps {
    foreach ($name in $Tasks) {
        if (-not (Test-Task $name)) { continue }
        if ($PSCmdlet.ShouldProcess("$TaskPath$name", 'End and unregister')) {
            # SecureDesktop is a launcher: ending the task does not end its child.
            if ($name -ne 'SecureDesktop') { [void](Invoke-Schtasks @('/end', '/tn', "$TaskPath$name")) }
            [void](Invoke-Schtasks @('/delete', '/tn', "$TaskPath$name", '/f'))
            Write-Step 'task' 'done' "Removed $TaskPath$name."
        }
        else { Write-Step 'task' 'planned' "Remove $TaskPath$name." }
    }
    foreach ($dir in @($AgentDir, $LegacyElevatedDir)) {
        if (-not (Test-Path -LiteralPath $dir)) { continue }
        if ($PSCmdlet.ShouldProcess($dir, 'Stop its processes and delete')) {
            Get-Process bubbleplay_admin_agent, bubbleplay_secure_helper, bubbleplay_tray, bubbleplay-host -ErrorAction SilentlyContinue |
                Where-Object { $_.Path -and $_.Path.StartsWith($dir, [StringComparison]::OrdinalIgnoreCase) } |
                Stop-Process -Force -ErrorAction SilentlyContinue
            Remove-WithRetry $dir
            Write-Step 'machine-files' 'done' "Deleted $dir."
        }
        else { Write-Step 'machine-files' 'planned' "Delete $dir." }
    }
    if (Test-AdminModeKeyExists) {
        if ($PSCmdlet.ShouldProcess($AdminModeKey, 'Delete protected Admin mode policy key')) {
            $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, [Microsoft.Win32.RegistryView]::Registry64)
            try { $base.DeleteSubKeyTree($AdminModeRegistryPath, $false) }
            finally { $base.Dispose() }
            Write-Step 'admin-mode-policy' 'done' "Deleted $AdminModeKey."
        }
        else { Write-Step 'admin-mode-policy' 'planned' "Delete $AdminModeKey." }
    }
    if ((Test-Path -LiteralPath $MachineDir) -and -not (Get-ChildItem -LiteralPath $MachineDir -Force | Select-Object -First 1)) {
        if ($PSCmdlet.ShouldProcess($MachineDir, 'Delete empty folder')) { Remove-Item -Force -LiteralPath $MachineDir }
    }
}

if ($MachinePhase) {
    if (-not $IsElevated -and -not $IsWhatIf) { throw '-MachinePhase must run elevated.' }
    Invoke-MachineSteps
    exit 0
}

# Run from a temporary copy so the install folder (which holds this script)
# can be deleted.
if (-not $Relocated -and -not $IsWhatIf -and $PSCommandPath.StartsWith("$InstallDir\", [StringComparison]::OrdinalIgnoreCase)) {
    $copy = Join-Path ([IO.Path]::GetTempPath()) ("bubbleplay-uninstall-{0}.ps1" -f [Guid]::NewGuid().ToString('N'))
    Copy-Item -LiteralPath $PSCommandPath -Destination $copy
    $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$copy`"", '-Relocated', '-InstallDir', "`"$InstallDir`"")
    if ($RemoveData) { $arguments += '-RemoveData' }
    $child = Start-Process -FilePath $PowerShellExe -ArgumentList $arguments -Wait -PassThru -NoNewWindow
    exit $child.ExitCode
}
Set-Location -LiteralPath ([IO.Path]::GetTempPath())

try {
    # 1. Elevated part first (one UAC prompt), only if something is installed.
    $work = Get-MachineWork
    if ($work.Count -eq 0) { Write-Step 'machine' 'unchanged' 'No admin agent or old elevated host installed (no UAC prompt).' }
    elseif ($IsElevated) { Invoke-MachineSteps }
    elseif ($PSCmdlet.ShouldProcess('uninstall-tray.ps1 -MachinePhase', 'Elevate once (UAC)')) {
        Write-Step 'elevate' 'running' ("one UAC prompt to: " + ($work -join '; '))
        $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-MachinePhase')
        try { $child = Start-Process -FilePath $PowerShellExe -ArgumentList $arguments -Verb RunAs -Wait -PassThru -WindowStyle Hidden }
        catch { throw "The administrator prompt was declined or failed ($($_.Exception.Message)); nothing was removed." }
        if ($child.ExitCode -ne 0) { throw "The elevated part failed with exit code $($child.ExitCode); nothing else was removed." }
        Write-Step 'elevate' 'done' 'Admin agent and old elevated host removed.'
    }
    else { Write-Step 'elevate' 'planned' ("one UAC prompt to: " + ($work -join '; ')) }

    # 2. User part.
    $tray = Join-Path $InstallDir 'bubbleplay_tray.exe'
    if (Get-Process bubbleplay_tray -ErrorAction SilentlyContinue) {
        if ($PSCmdlet.ShouldProcess('bubbleplay_tray', 'Quit tray')) {
            if (Test-Path -LiteralPath $tray) { Start-Process -FilePath $tray -ArgumentList '--quit' -Wait -WindowStyle Hidden }
            Start-Sleep -Milliseconds 500
            Get-Process bubbleplay_tray, bubbleplay-host, bubbleplay_admin_agent -ErrorAction SilentlyContinue |
                Where-Object { $_.Path -and $_.Path.StartsWith($InstallDir, [StringComparison]::OrdinalIgnoreCase) } |
                Stop-Process -Force -ErrorAction SilentlyContinue
            Write-Step 'tray' 'done' 'Tray stopped.'
        }
        else { Write-Step 'tray' 'planned' 'Quit the running tray.' }
    }
    $props = Get-ItemProperty -Path $RunKey -ErrorAction SilentlyContinue
    if ($props -and $props.PSObject.Properties[$RunValue]) {
        if ($PSCmdlet.ShouldProcess("$RunKey\$RunValue", 'Remove autostart')) { Remove-ItemProperty -Path $RunKey -Name $RunValue; Write-Step 'autostart' 'done' 'Removed.' }
        else { Write-Step 'autostart' 'planned' "Remove HKCU Run\$RunValue." }
    }
    foreach ($item in @($StartMenuShortcut, $SendToShortcut, $UninstallKey)) {
        if (-not (Test-Path -LiteralPath $item)) { continue }
        if ($PSCmdlet.ShouldProcess($item, 'Remove')) { Remove-Item -Recurse -Force -LiteralPath $item; Write-Step 'registration' 'done' "Removed $item." }
        else { Write-Step 'registration' 'planned' "Remove $item." }
    }
    if (Test-Path -LiteralPath $InstallDir) {
        if ($PSCmdlet.ShouldProcess($InstallDir, 'Delete per-user install')) { Remove-WithRetry $InstallDir; Write-Step 'files' 'done' "Deleted $InstallDir." }
        else { Write-Step 'files' 'planned' "Delete $InstallDir." }
    }
    if ($RemoveData -and (Test-Path -LiteralPath $DataDir)) {
        if ($PSCmdlet.ShouldProcess($DataDir, 'Delete data (host.json, logs, receipts)')) { Remove-WithRetry $DataDir; Write-Step 'data' 'done' "Deleted $DataDir." }
        else { Write-Step 'data' 'planned' "Delete $DataDir." }
    }
    elseif (Test-Path -LiteralPath $DataDir) { Write-Step 'data' 'kept' "$DataDir kept (pass -RemoveData to delete it)." }
}
finally {
    if ($Relocated) { Remove-Item -Force -LiteralPath $PSCommandPath -ErrorAction SilentlyContinue }
}
