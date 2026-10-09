#Requires -Version 5.1
<#
.SYNOPSIS
    Owner-run secure-desktop qualification from normal PowerShell.
.DESCRIPTION
    Requires an installed protocol-v2 bubbleplay_secure_helper.exe and its
    protected \BubblePlay\SecureDesktop task. Build --release --bins and update
    the installed helpers using update-tray.ps1 -IncludeAgent before this test.
    Requires the protected \BubblePlay\AdminModeOff task. The owner approves
    UAC for the installed helper's --set-admin-mode on and exact HKLM restoration.
    This script does not install files or restart the tray/host.

    Disconnect every live viewer first and do not launch other local UAC prompts
    or change tray settings until the probe completes. Prompt ownership is inferred from a clean baseline
    and one consent process while this runas request is pending, not proven by a
    Windows originating-process identifier. The probe checks the host log and claims
    the real authenticated exclusive helper Host slot (Busy aborts) before any
    configuration change or UAC request. If the helper is absent it starts the
    already-installed task through System32\schtasks.exe /run as the normal user.

    Creates three harmless, cancellable test UAC requests: runas cmd.exe /c exit 0.
    Sends only Escape down/up, absolute mouse MOVE, and DenyConsent; never
    approval keys or clicks. OFF must reject both input requests and preserve
    UAC for 2 seconds, then Deny must close it. ON must apply every MOVE and
    Escape record, save fresh center/corner PNGs, and close UAC with Escape.

    Also tests tray.json adminMode=true with HKLM off using the OFF assertions.
    Backs up/restores tray.json byte-for-byte and HKLM AdminMode/AdminModeSid
    as exact raw types/bytes (including initially absent values).
    The probe holds the Host pipe until cleanup and configuration restoration.
    A late/pending runas request must settle before cleanup can pass; a timeout
    reports cleanup failure. On failure, inspect summary.json and dismiss any
    reported outstanding prompt locally before retrying.

    Stdout is the JSON summary. summary.json, PNGs and probe stdout/stderr logs
    are written to -OutDir. Exit 0 requires all three scenarios, confirmed cleanup,
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
$registryBackup = $null
$registryRestored = $false
$policyRestoreCommand = $null
$registryInterop = @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
public sealed class BubblePlayPolicyValue {
    public bool exists;
    public uint type;
    public string data;
}
public static class BubblePlayPolicyRegistry {
    static readonly IntPtr Hklm = new IntPtr(unchecked((int)0x80000002));
    [DllImport("advapi32.dll", CharSet=CharSet.Unicode)] static extern int RegOpenKeyEx(IntPtr root,string path,uint options,uint access,out IntPtr key);
    [DllImport("advapi32.dll", CharSet=CharSet.Unicode)] static extern int RegQueryValueEx(IntPtr key,string name,IntPtr reserved,out uint type,[Out] byte[] data,ref uint size);
    [DllImport("advapi32.dll", CharSet=CharSet.Unicode)] static extern int RegSetValueEx(IntPtr key,string name,uint reserved,uint type,[In] byte[] data,uint size);
    [DllImport("advapi32.dll", CharSet=CharSet.Unicode)] static extern int RegDeleteValue(IntPtr key,string name);
    [DllImport("advapi32.dll")] static extern int RegCloseKey(IntPtr key);
    static void Check(int status) { if(status != 0) throw new Win32Exception(status); }
    static IntPtr Open(uint access) { IntPtr key; Check(RegOpenKeyEx(Hklm,"SOFTWARE\\BubblePlay",0,access|0x100,out key)); return key; }
    public static BubblePlayPolicyValue Read(string name) {
        IntPtr key=Open(1);
        try {
            uint type,size=0;
            int status=RegQueryValueEx(key,name,IntPtr.Zero,out type,null,ref size);
            if(status==2) return new BubblePlayPolicyValue {exists=false,type=0,data=""};
            Check(status);
            if(size>65536) throw new Exception("Admin mode registry value exceeds backup limit");
            byte[] data=new byte[size];
            Check(RegQueryValueEx(key,name,IntPtr.Zero,out type,data,ref size));
            Array.Resize(ref data,(int)size);
            return new BubblePlayPolicyValue {exists=true,type=type,data=Convert.ToBase64String(data)};
        } finally { RegCloseKey(key); }
    }
    public static void Restore(string name,bool exists,uint type,string encoded) {
        if(name!="AdminMode" && name!="AdminModeSid") throw new Exception("Unexpected policy value");
        IntPtr key=Open(2);
        try {
            if(exists) { byte[] data=Convert.FromBase64String(encoded); Check(RegSetValueEx(key,name,0,type,data,(uint)data.Length)); }
            else { int status=RegDeleteValue(key,name); if(status!=2) Check(status); }
        } finally { RegCloseKey(key); }
    }
}
'@
function Read-PolicySnapshot {
    [pscustomobject]@{ AdminMode = [BubblePlayPolicyRegistry]::Read('AdminMode'); AdminModeSid = [BubblePlayPolicyRegistry]::Read('AdminModeSid') }
}
function New-PolicyRestoreCommand {
    param([string]$InteropSource, [string]$SnapshotJson)
    $source = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($InteropSource))
    $snapshot = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($SnapshotJson))
    # Only fixed registry names and the in-memory original raw snapshot are
    # passed to the elevated child; no path or command comes from a request.
    $code = @"
`$ErrorActionPreference = 'Stop'
try {
    Add-Type -TypeDefinition ([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('$source')))
    `$backup = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('$snapshot')) | ConvertFrom-Json
    # Clear first so restoring a different original SID never grants this
    # console user input even briefly. Restore the exact mode bytes last.
    [BubblePlayPolicyRegistry]::Restore('AdminMode', `$true, 4, 'AAAAAA==')
    foreach (`$name in @('AdminModeSid','AdminMode')) {
        `$value = `$backup.`$name
        [BubblePlayPolicyRegistry]::Restore(`$name, [bool]`$value.exists, [uint32]`$value.type, [string]`$value.data)
    }
    exit 0
} catch { Write-Error `$_; exit 1 }
"@
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($code))
    # Leave room below Windows' 32767-character command-line limit for the
    # protected PowerShell path and arguments. Reject before policy mutation.
    if ($encoded.Length -gt 30000) { throw 'Original HKLM policy exceeds the bounded restoration command capacity; no probe or policy mutation was started.' }
    return $encoded
}
function Restore-PolicySnapshot {
    $powershell = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::System)) 'WindowsPowerShell\v1.0\powershell.exe'
    $child = Start-Process -FilePath $powershell -ArgumentList @('-NoProfile','-EncodedCommand',$policyRestoreCommand) -Verb RunAs -Wait -PassThru -WindowStyle Hidden
    if ($child.ExitCode -ne 0) { throw "Elevated HKLM restoration exited $($child.ExitCode)." }
    $expected = $registryBackup | ConvertTo-Json -Depth 4 -Compress
    $actual = Read-PolicySnapshot | ConvertTo-Json -Depth 4 -Compress
    if ($expected -cne $actual) { throw 'HKLM restoration raw type/data verification failed.' }
}
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
    }
    $backupValid = $true
    [void][IO.Directory]::CreateDirectory($OutDir)
    Add-Type -TypeDefinition $registryInterop
    $registryBackup = Read-PolicySnapshot
    $policyRestoreCommand = New-PolicyRestoreCommand -InteropSource $registryInterop -SnapshotJson ($registryBackup | ConvertTo-Json -Depth 4 -Compress)
    [IO.File]::WriteAllText((Join-Path $OutDir 'registry-backup.json'), ($registryBackup | ConvertTo-Json -Depth 4), [Text.UTF8Encoding]::new($false))
    $helper = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles)) 'BubblePlay\agent\bubbleplay_secure_helper.exe'
    if (-not (Test-Path -LiteralPath $helper -PathType Leaf)) { throw "Missing installed protected helper: $helper" }
    [IO.File]::WriteAllText((Join-Path $OutDir 'policy-wrapper.json'),
        ([pscustomobject]@{ run_id=$runId; pid=$PID } | ConvertTo-Json -Compress), [Text.UTF8Encoding]::new($false))
    $arguments = @('--out-dir', "`"$OutDir`"", '--tray-settings', "`"$settings`"", '--host-log', "`"$hostLog`"", '--run-id', $runId)
    $probe = Start-Process -FilePath $ExePath -ArgumentList $arguments -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput (Join-Path $OutDir 'probe.stdout.log') -RedirectStandardError (Join-Path $OutDir 'probe.stderr.log')
    $requestId = 1
    $offCompleted = $false
    while (-not $probe.HasExited) {
        $requestPath = Join-Path $OutDir "policy-request-$requestId.json"
        if (Test-Path -LiteralPath $requestPath -PathType Leaf) {
            $policyError = $null
            try {
                $request = [IO.File]::ReadAllText($requestPath) | ConvertFrom-Json
                if ($request.run_id -ne $runId -or $request.id -ne $requestId) { throw 'Policy request identity mismatch.' }
                $marker = [IO.File]::ReadAllText($markerPath) | ConvertFrom-Json
                if ($marker.run_id -ne $runId -or -not $marker.exclusive_host_claimed -or $marker.pid -ne $probe.Id) { throw 'Policy request lacks this probe exclusive-Host marker.' }
                if ($request.action -eq 'on') {
                    $child = Start-Process -FilePath $helper -ArgumentList @('--set-admin-mode','on') -Verb RunAs -Wait -PassThru -WindowStyle Hidden
                    if ($child.ExitCode -ne 0) { throw "Elevated protected helper exited $($child.ExitCode)." }
                }
                elseif ($request.action -eq 'off') {
                    # Preserve the normal-user schtasks launch used by the
                    # existing helper probe, but await completion: a queued
                    # OFF must not execute after the following ON/restoration.
                    $mode = [BubblePlayPolicyRegistry]::Read('AdminMode')
                    if (-not ($offCompleted -and $mode.exists -and $mode.type -eq 4 -and $mode.data -ceq 'AAAAAA==')) {
                        $scheduler = New-Object -ComObject 'Schedule.Service'
                        $scheduler.Connect()
                        $task = $scheduler.GetFolder('\BubblePlay').GetTask('AdminModeOff')
                        $previousRun = $task.LastRunTime
                        $schtasks = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::System)) 'schtasks.exe'
                        $taskOutput = & $schtasks /run /tn '\BubblePlay\AdminModeOff' 2>&1
                        if ($LASTEXITCODE -ne 0) { throw "AdminModeOff launch failed: $taskOutput" }
                        # Retain Host rather than race an uncompleted queued
                        # policy writer with ON or original-state restoration.
                        do { Start-Sleep -Milliseconds 50; $task = $scheduler.GetFolder('\BubblePlay').GetTask('AdminModeOff') }
                        while ($task.LastRunTime -eq $previousRun -or $task.State -eq 2 -or $task.State -eq 4)
                        if ($task.LastTaskResult -ne 0) { throw "AdminModeOff failed: $($task.LastTaskResult)" }
                        $offCompleted = $true
                        $mode = [BubblePlayPolicyRegistry]::Read('AdminMode')
                    }
                    if (-not $mode.exists -or $mode.type -ne 4 -or $mode.data -cne 'AAAAAA==') { throw 'AdminModeOff did not write DWORD zero.' }
                }
                elseif ($request.action -eq 'restore') { Restore-PolicySnapshot; $registryRestored = $true }
                else { throw 'Unexpected policy action; only on, off and restore are accepted.' }
            } catch { $policyError = $_.Exception.Message }
            $response = [pscustomobject]@{ run_id=$runId; id=$requestId; success=($null -eq $policyError); error=$policyError } | ConvertTo-Json -Compress
            $responsePath = Join-Path $OutDir "policy-response-$requestId.json"
            $tempResponse = "$responsePath.tmp"
            [IO.File]::WriteAllText($tempResponse, $response, [Text.UTF8Encoding]::new($false))
            [IO.File]::Move($tempResponse, $responsePath)
            $requestId++
        }
        Start-Sleep -Milliseconds 50
        $probe.Refresh()
    }
    $probe.WaitForExit()
    $exitCode = $probe.ExitCode
    if (-not (Test-Path -LiteralPath $summaryPath -PathType Leaf)) { throw "Probe exited $exitCode without summary.json; inspect probe.stderr.log." }
    $summary = [IO.File]::ReadAllText($summaryPath) | ConvertFrom-Json
    if (-not $summary.success -or @($summary.scenarios).Count -ne 3 -or @($summary.scenarios | Where-Object { -not $_.success }).Count -gt 0 -or
        -not $summary.cleanup.success -or -not $summary.config.restored -or -not $summary.registry.restored -or
        -not $summary.non_elevated_registry_write.success -or $exitCode -ne 0) { $exitCode = 1; $summary.success = $false }
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
            if (-not $mutationAuthorized) { $registryRestored = $true }
            elseif (-not $registryRestored) {
                # A crashed probe cannot prove its late runas request settled.
                # Do not create a competing elevation; report the recovery
                # snapshot for local cleanup and restoration instead.
                throw 'HKLM restoration did not complete while the exclusive Host was held. Inspect registry-backup.json and summary cleanup; dismiss any outstanding consent locally before restoring.'
            }
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
$summary | Add-Member -NotePropertyName script_registry_backup -NotePropertyValue ([pscustomobject]@{
    read_valid = ($null -ne $registryBackup); restored = $registryRestored; path = (Join-Path $OutDir 'registry-backup.json')
}) -Force
if ($failure) { $summary | Add-Member -NotePropertyName script_error -NotePropertyValue $failure -Force; $summary.success = $false }
if (-not $restored -or -not $registryRestored) { $summary.success = $false; $exitCode = 1 }
$json = $summary | ConvertTo-Json -Depth 16
# Preserve any previous artifact on a preflight failure; never replace another run.
if ($backupValid -and (Test-Path -LiteralPath $OutDir -PathType Container)) {
    try { [IO.File]::WriteAllText($summaryPath, $json, [Text.UTF8Encoding]::new($false)) }
    catch { $summary.success = $false; $exitCode = 1; $summary | Add-Member -NotePropertyName script_error -NotePropertyValue "Cannot write summary: $($_.Exception.Message)" -Force; $json = $summary | ConvertTo-Json -Depth 16 }
}
Write-Output $json
exit $exitCode
