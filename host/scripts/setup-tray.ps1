#Requires -Version 5.1
<#
.SYNOPSIS
    Install BubblePlay program files in Program Files with one UAC prompt.
.DESCRIPTION
    Run from a normal desktop PowerShell. The selected package is the initial
    trust root. Its elevated bootstrap copies/verifies all program files into
    Program Files\BubblePlay with inherited Program Files ACLs, then executes
    only the protected setup copy to register helpers, tasks and machine policy.
    The admin agent and secure helper run from the protected agent subdirectory.

    The unelevated parent owns HKCU, shortcuts, autostart, DPAPI host.json,
    tray.json and receipts under LOCALAPPDATA\BubblePlay. It verifies protected
    replacement files before deleting the legacy LOCALAPPDATA\Programs\BubblePlay
    program tree. Existing user data is retained. Unreadable legacy service
    credentials require explicit -WorkerUrl and -Token; elevation never reads
    or writes config, data or receipts.

    -AgentOnly repairs tasks/helpers exclusively from protected Program Files
    copies. -SkipAgent removes helper tasks/policy and the agent directory.
    -AppDir includes the optional Flutter executable, root DLLs and data assets;
    -AppOnly updates that app in an existing machine install. Source checkouts
    can keep scripts separate from build output; flat packages include all three
    setup/update/uninstall scripts. -WhatIf never elevates or writes files.
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string]$SourceDir,
    [string]$InstallDir,
    [string]$AppDir,
    [switch]$AppOnly,
    [string]$ServiceDataDir,
    [string]$WorkerUrl,
    [Security.SecureString]$Token,
    [string]$Monitor,
    [switch]$KeepServices,
    [Alias('SkipElevatedTask')]
    [switch]$SkipAgent,
    # Explicit spelling for the default full install (also used by update-tray).
    [switch]$IncludeAgent,
    [Alias('ElevatedBinariesOnly')]
    [switch]$AgentOnly,
    # Internal: the elevated half, started by this script through one UAC prompt.
    [switch]$MachinePhase,
    # Internal: package bytes have been copied and verified; execute only protected code.
    [switch]$ProtectedPhase,
    [switch]$NoLaunch
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not [Environment]::Is64BitProcess) { throw 'Use 64-bit PowerShell for the Windows x64 BubblePlay package.' }
if ($IncludeAgent -and $SkipAgent) { throw '-IncludeAgent and -SkipAgent cannot be combined.' }
$SystemDirectory = [Environment]::GetFolderPath('System')
$WindowsDirectory = [Environment]::GetFolderPath('Windows')
$SystemModuleRoot = [IO.Path]::Combine($PSHOME, 'Modules')
$WindowsModuleRoot = [IO.Path]::Combine($SystemDirectory, 'WindowsPowerShell\v1.0\Modules')
$IsElevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if ($IsElevated -or $MachinePhase) {
    # Prevent inherited PSModulePath/autoload from executing a user-owned module
    # before the selected package has reached the protected install directory.
    $env:PSModulePath = $SystemModuleRoot + ';' + $WindowsModuleRoot
    $PSModuleAutoLoadingPreference = 'None'
    foreach ($name in 'Microsoft.PowerShell.Utility', 'Microsoft.PowerShell.Management', 'Microsoft.PowerShell.Security') {
        Import-Module ([IO.Path]::Combine($SystemModuleRoot, $name, "$name.psd1")) -ErrorAction Stop
    }
}
Add-Type -AssemblyName System.Security
# Load modules up front with WhatIf off so module import noise (alias creation)
# does not show up as fake plan items; queries below pass -WhatIf:$false too.
& {
    $WhatIfPreference = $false
    foreach ($module in 'CimCmdlets', 'ScheduledTasks', 'NetQos') {
        Import-Module ([IO.Path]::Combine($WindowsModuleRoot, $module, "$module.psd1")) -ErrorAction SilentlyContinue
    }
}

$ProgramFilesRoot = [Environment]::GetFolderPath('ProgramFiles')
$MachineDir = Join-Path $ProgramFilesRoot 'BubblePlay'
$AgentDir = Join-Path $MachineDir 'agent'
$AgentExeName = 'bubbleplay_admin_agent.exe'
$AgentExe = Join-Path $AgentDir $AgentExeName
$HelperExeName = 'bubbleplay_secure_helper.exe'
$HelperExe = Join-Path $AgentDir $HelperExeName
$PrivilegedBinaries = @($AgentExeName, $HelperExeName)
$LegacyElevatedDir = Join-Path $MachineDir 'elevated'
$TrustedSids = @('S-1-5-18', 'S-1-5-32-544', 'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464')
$ServiceNames = @('BubblePlayLabHelper', 'BubblePlayTransport', 'BubblePlayBroker')
# Decision 25: the service-mode host is unsupported in phase 2, so setup never installs or starts
# Transport and removes one left by an older install, even with -KeepServices.
$TransportServiceName = 'BubblePlayTransport'
$ServiceProcessNames = @('bubbleplay_broker', 'bubbleplay_transport', 'bubbleplay_agent',
    'bubbleplay_input_proxy', 'bubbleplay_lab_helper', 'bubbleplay_lab_activity_guard', 'bubbleplay-host')
$UserBinaries = @('bubbleplay-host.exe', 'bubbleplay_tray.exe') + $PrivilegedBinaries
$UserScripts = @('setup-tray.ps1', 'update-tray.ps1', 'uninstall-tray.ps1')
$TaskPath = '\BubblePlay\'
$AgentTaskName = 'AdminAgent'
$HelperTaskName = 'SecureDesktop'
$AdminModeOffTaskName = 'AdminModeOff'
$AdminModeRegistryPath = 'SOFTWARE\BubblePlay'
$AdminModeKey = 'HKLM:\SOFTWARE\BubblePlay'
$LegacyTaskName = 'ElevatedHost'
$LaunchTaskName = 'LaunchTray'
$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$RunValue = 'BubblePlayTray'
$UninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\BubblePlay'
$StartMenuShortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'BubblePlay.lnk'
# Explorer "Send to > BubblePlay": offers the selected files to the viewer.
$SendToShortcut = Join-Path ([Environment]::GetFolderPath('SendTo')) 'BubblePlay.lnk'

if (-not $SourceDir) {
    $SourceDir = if (Test-Path -LiteralPath (Join-Path $PSScriptRoot 'bubbleplay-host.exe')) { $PSScriptRoot } else { Join-Path $PSScriptRoot '..\target\release' }
}
$SourceDir = [IO.Path]::GetFullPath($SourceDir)
if (-not $ServiceDataDir) { $ServiceDataDir = Join-Path ([Environment]::GetFolderPath('CommonApplicationData')) 'BubblePlay' }
if (-not $InstallDir) { $InstallDir = $MachineDir }
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
$LegacyUserDir = $null
if (-not $MachinePhase -and -not $IsElevated) { $LegacyUserDir = Join-Path $env:LOCALAPPDATA 'Programs\BubblePlay' }
$DataDir = $null; $HostConfigPath = $null; $TraySettingsPath = $null
if (-not $MachinePhase -and -not $IsElevated) {
    $DataDir = Join-Path $env:LOCALAPPDATA 'BubblePlay'
    $HostConfigPath = Join-Path $DataDir 'host.json'
    $TraySettingsPath = Join-Path $DataDir 'tray.json'
}
$TrayExe = Join-Path $InstallDir 'bubbleplay_tray.exe'
$PowerShellExe = Join-Path $SystemDirectory 'WindowsPowerShell\v1.0\powershell.exe'
$IsWhatIf = [bool]$WhatIfPreference

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$IsElevated = ([Security.Principal.WindowsPrincipal]$identity).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
$UserSid = $identity.User.Value
$UserName = $identity.Name

$phase = if ($MachinePhase) { 'machine' } elseif ($AgentOnly) { 'agent-only' } else { 'full' }
$refused = $false
$receipt = [ordered]@{
    schema      = 'bubbleplay.setup-tray.v2'
    phase       = $phase
    startedUtc  = [DateTimeOffset]::UtcNow.ToString('o')
    finishedUtc = $null
    whatIf      = $IsWhatIf
    elevated    = $IsElevated
    user        = $UserName
    userSid     = $UserSid
    sourceDir   = $SourceDir
    installDir  = $InstallDir
    agentDir    = $AgentDir
    dataDir     = $DataDir
    steps       = [Collections.Generic.List[object]]::new()
    result      = 'running'
    error       = $null
}

function Add-Step([string]$Name, [string]$Status, [string]$Detail = '') {
    $receipt.steps.Add([ordered]@{ step = $Name; status = $Status; detail = $Detail })
    $label = if ($IsWhatIf -and $Status -eq 'planned') { 'PLAN' } else { $Status.ToUpperInvariant() }
    Write-Host ("[{0,-9}] {1}: {2}" -f $label, $Name, $Detail)
}

# Get-FileHash honours -WhatIf in Windows PowerShell 5.1, so hash via .NET.
function Get-Sha256([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    try {
        $sha = [Security.Cryptography.SHA256]::Create()
        try { return ConvertTo-Hex $sha.ComputeHash($stream) } finally { $sha.Dispose() }
    }
    finally { $stream.Dispose() }
}

function Get-Sha256OrNull([string]$Path) {
    try { if (Test-Path -LiteralPath $Path -PathType Leaf) { return Get-Sha256 $Path } } catch { }
    return $null
}

function Test-SamePath([string]$A, [string]$B) {
    [string]::Equals([IO.Path]::GetFullPath($A).TrimEnd('\'), [IO.Path]::GetFullPath($B).TrimEnd('\'),
        [StringComparison]::OrdinalIgnoreCase)
}

function Get-RunValue {
    $props = Get-ItemProperty -Path $RunKey -ErrorAction SilentlyContinue
    if ($props -and $props.PSObject.Properties[$RunValue]) { return [string]$props.$RunValue }
    return $null
}

function ConvertTo-Hex([byte[]]$Bytes) { ($Bytes | ForEach-Object { $_.ToString('X2') }) -join '' }

function ConvertFrom-Hex([string]$Hex) {
    $Hex = $Hex.Trim()
    if ($Hex.Length % 2 -ne 0) { throw 'DPAPI hex has odd length.' }
    $bytes = New-Object byte[] ($Hex.Length / 2)
    for ($i = 0; $i -lt $bytes.Length; $i++) { $bytes[$i] = [Convert]::ToByte($Hex.Substring($i * 2, 2), 16) }
    return , $bytes
}

function Unprotect-Hex([string]$Hex) {
    if (-not $Hex) { return $null }
    $script:LegacyUserProtection = $false
    try {
        $plain = [Security.Cryptography.ProtectedData]::Unprotect((ConvertFrom-Hex $Hex),
            [Text.Encoding]::UTF8.GetBytes('BubblePlay.Host.DPAPI.v1'), [Security.Cryptography.DataProtectionScope]::CurrentUser)
    }
    catch {
        $plain = [Security.Cryptography.ProtectedData]::Unprotect((ConvertFrom-Hex $Hex), $null,
            [Security.Cryptography.DataProtectionScope]::CurrentUser)
        $script:LegacyUserProtection = $true
    }
    try { return [Text.Encoding]::UTF8.GetString($plain) } finally { [Array]::Clear($plain, 0, $plain.Length) }
}

function Protect-UserHex([string]$Plain) {
    if (-not $Plain) { return $null }
    $bytes = [Text.Encoding]::UTF8.GetBytes($Plain)
    try {
        return ConvertTo-Hex ([Security.Cryptography.ProtectedData]::Protect($bytes, [Text.Encoding]::UTF8.GetBytes('BubblePlay.Host.DPAPI.v1'),
                [Security.Cryptography.DataProtectionScope]::CurrentUser))
    }
    finally { [Array]::Clear($bytes, 0, $bytes.Length) }
}

function ConvertFrom-Secure([Security.SecureString]$Secret) {
    if ($null -eq $Secret) { return $null }
    $pointer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($Secret)
    try { return [Runtime.InteropServices.Marshal]::PtrToStringBSTR($pointer) }
    finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($pointer) }
}

function Invoke-Schtasks([string[]]$Arguments) {
    $exe = Join-Path $SystemDirectory 'schtasks.exe'
    # Native stderr ("task not found") must not become a terminating error.
    $ErrorActionPreference = 'Continue'
    $output = & $exe @Arguments 2>&1
    return [pscustomobject]@{ ExitCode = $LASTEXITCODE; Output = ($output | Out-String).Trim() }
}

function Test-Task([string]$Name) {
    [bool](Get-ScheduledTask -TaskPath $TaskPath -TaskName $Name -ErrorAction SilentlyContinue)
}

function Assert-InteractiveUserMatches {
    $owners = @(Get-CimInstance Win32_Process -Filter "Name='explorer.exe'" -ErrorAction SilentlyContinue |
            ForEach-Object { $r = Invoke-CimMethod -InputObject $_ -MethodName GetOwnerSid -WhatIf:$false -ErrorAction SilentlyContinue; if ($r -and $r.PSObject.Properties['Sid']) { $r.Sid } } |
            Where-Object { $_ } | Sort-Object -Unique)
    if ($owners.Count -eq 0) {
        Add-Step 'identity' 'warning' "No explorer.exe found; assuming $UserName is the desktop user."
        return
    }
    if ($owners -notcontains $UserSid) {
        throw "This session runs as $UserName ($UserSid) but the desktop belongs to $($owners -join ', '). " +
        'Run as the desktop user (and approve UAC with consent, not a different admin login) so CurrentUser DPAPI, HKCU, %LOCALAPPDATA% and the agent task belong to that user.'
    }
    Add-Step 'identity' 'ok' "$UserName owns the interactive desktop."
}

# --- Configuration ----------------------------------------------------------------
function Get-ServiceConfig {
    $path = Join-Path $ServiceDataDir 'transport.json'
    $visible = $false
    try { $visible = Test-Path -LiteralPath $path -PathType Leaf } catch { }
    if (-not $visible) {
        # Non-elevated, the service ACL can hide the file; report it as unreadable.
        if (-not $IsElevated -and (Test-Path -LiteralPath $ServiceDataDir)) {
            return [pscustomobject]@{ Path = $path; Readable = $false; Error = 'not visible without elevation' }
        }
        return $null
    }
    try { $json = Get-Content -Raw -LiteralPath $path | ConvertFrom-Json }
    catch { return [pscustomobject]@{ Path = $path; Readable = $false; Error = $_.Exception.Message } }
    $unprotect = {
        param([string]$Hex)
        if (-not $Hex) { return $null }
        try {
            $plain = [Security.Cryptography.ProtectedData]::Unprotect((ConvertFrom-Hex $Hex),
                [Text.Encoding]::UTF8.GetBytes('BubblePlay.Host.DPAPI.v1'), [Security.Cryptography.DataProtectionScope]::LocalMachine)
        }
        catch {
            $plain = [Security.Cryptography.ProtectedData]::Unprotect((ConvertFrom-Hex $Hex), $null,
                [Security.Cryptography.DataProtectionScope]::LocalMachine)
        }
        try { [Text.Encoding]::UTF8.GetString($plain) } finally { [Array]::Clear($plain, 0, $plain.Length) }
    }
    return [pscustomobject]@{
        Path     = $path
        Readable = $true
        Worker   = [string]$json.worker
        Token    = & $unprotect ([string]$json.tokenDpapiHex)
    }
}

function Get-ExistingHostConfig {
    if (-not (Test-Path -LiteralPath $HostConfigPath -PathType Leaf)) { return $null }
    try {
        $json = Get-Content -Raw -LiteralPath $HostConfigPath | ConvertFrom-Json
        $monitorValue = if ($json.PSObject.Properties['monitor']) { $json.monitor } else { $null }
        $decodedToken = Unprotect-Hex ([string]$json.tokenDpapiHex)
        return [pscustomobject]@{
            Worker  = [string]$json.worker
            Token   = $decodedToken
            Monitor = $monitorValue
            # A host.json from an older setup may carry fields that are no longer written; rewrite it then.
            Extra   = $script:LegacyUserProtection -or @($json.PSObject.Properties.Name | Where-Object { $_ -notin @('worker', 'tokenDpapiHex', 'monitor') }).Count -gt 0
        }
    }
    catch {
        Write-Warning "Existing host.json is unreadable and will be replaced: $($_.Exception.Message)"
        return $null
    }
}

# Returns 'machine' when only the elevated part can read the config (the old
# service config under ProgramData); otherwise writes/plans host.json itself.
function Assert-HostCredentials([string]$RegistrationToken) {
    # Keep this minimum aligned with the relay's TOKEN minimum.
    if ([string]::IsNullOrWhiteSpace($RegistrationToken) -or $RegistrationToken.Length -lt 32) {
        throw 'Host registration token (TOKEN / -Token) must contain at least 32 characters.'
    }
}

function Update-HostConfig {
    $existing = Get-ExistingHostConfig
    $service = if (-not $existing -and (-not $WorkerUrl -or $null -eq $Token)) { Get-ServiceConfig } else { $null }
    $explicitToken = ConvertFrom-Secure $Token
    $hasExplicitToken = $null -ne $Token

    $source = $null; $worker = $null; $tok = $null
    if ($WorkerUrl -or $hasExplicitToken) {
        $source = 'parameters'
        $worker = if ($WorkerUrl) { $WorkerUrl } elseif ($existing) { $existing.Worker } elseif ($service -and $service.Readable) { $service.Worker } else { $null }
        $tok = if ($hasExplicitToken) { $explicitToken } elseif ($existing) { $existing.Token } elseif ($service -and $service.Readable) { $service.Token } else { $null }
    }
    elseif ($existing) {
        $source = 'existing host.json'
        $worker = $existing.Worker; $tok = $existing.Token
    }
    elseif ($service -and $service.Readable) {
        $source = "service config $($service.Path)"
        $worker = $service.Worker; $tok = $service.Token
    }
    elseif ($service -and -not $service.Readable) {
        return 'machine'
    }
    else {
        throw 'No existing configuration found. Pass -WorkerUrl wss://.../host-ws and -Token (SecureString).'
    }
    if (-not $worker -or $worker -notmatch '^wss?://') { throw "Worker URL must be ws:// or wss:// (got '$worker')." }
    Assert-HostCredentials $tok
    $monitorValue = if ($PSBoundParameters.ContainsKey('Monitor') -or $Monitor) { $Monitor } elseif ($existing) { $existing.Monitor } else { $null }

    if ($existing -and $existing.Worker -eq $worker -and $existing.Token -eq $tok -and
        -not $existing.Extra -and $existing.Monitor -eq $monitorValue) {
        Add-Step 'config' 'unchanged' "$HostConfigPath already matches ($source)."
        return 'done'
    }
    if ($PSCmdlet.ShouldProcess($HostConfigPath, "Write host config from $source (secrets CurrentUser-DPAPI)")) {
        $config = [ordered]@{
            worker        = $worker
            tokenDpapiHex = Protect-UserHex $tok
            monitor       = if ($monitorValue) { $monitorValue } else { $null }
        }
        New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
        $temp = "$HostConfigPath.tmp"
        [IO.File]::WriteAllText($temp, ($config | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
        Move-Item -Force -LiteralPath $temp -Destination $HostConfigPath
        # Round-trip to prove the tray (same user) can decrypt it.
        $check = Get-ExistingHostConfig
        if (-not $check -or $check.Token -ne $tok) { throw 'host.json round-trip verification failed.' }
        Add-Step 'config' 'done' "Wrote $HostConfigPath from $source (worker $worker)."
    }
    else {
        Add-Step 'config' 'planned' "Write $HostConfigPath from $source (worker $worker)."
    }
    return 'done'
}

# --- Stop the running tray ---------------------------------------------------------
function Stop-RunningTray {
    $roots = @(($MachineDir.TrimEnd('\') + '\'), ($LegacyUserDir.TrimEnd('\') + '\'))
    $running = @(Get-Process bubbleplay_tray -ErrorAction SilentlyContinue |
        Where-Object { $path = $_.Path; $path -and @($roots | Where-Object { $path.StartsWith($_, [StringComparison]::OrdinalIgnoreCase) }).Count })
    if ($running.Count -eq 0) { Add-Step 'stop-tray' 'skipped' 'Tray is not running.'; return }
    if (-not $PSCmdlet.ShouldProcess('bubbleplay_tray', 'Quit running tray (its host and admin agent exit with it)')) {
        Add-Step 'stop-tray' 'planned' "Quit $($running.Count) running tray process(es); the host and admin agent exit with the tray."
        return
    }
    $quitter = if (Test-Path -LiteralPath $TrayExe) { $TrayExe } else { $running[0].Path }
    if ($quitter) { Start-Process -FilePath $quitter -ArgumentList '--quit' -Wait -WindowStyle Hidden }
    Start-Sleep -Milliseconds 500
    $running | Stop-Process -Force -ErrorAction SilentlyContinue
    Get-Process bubbleplay-host, bubbleplay_app, bubbleplay_admin_agent -ErrorAction SilentlyContinue |
        Where-Object { $path = $_.Path; $path -and @($roots | Where-Object { $path.StartsWith($_, [StringComparison]::OrdinalIgnoreCase) }).Count } |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Add-Step 'stop-tray' 'done' 'Tray and its host stopped.'
}

# --- Protected program package -------------------------------------------------------
# Reject links in every ancestor, including a dangling link. Never enumerate
# recursively through an unchecked directory (Windows PowerShell 5.1 follows junctions).
function Assert-NoReparse([string]$Path) {
    $absolute = [IO.Path]::GetFullPath($Path)
    while ($absolute) {
        $item = Get-Item -LiteralPath $absolute -Force -ErrorAction SilentlyContinue
        if ($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw "Reparse paths are not accepted: $absolute" }
        $parent = [IO.Path]::GetDirectoryName($absolute)
        if ($parent -eq $absolute) { break }
        $absolute = $parent
    }
}

function Get-SafeTreeFiles([string]$Path, [switch]$IncludeDirectories) {
    Assert-NoReparse $Path
    foreach ($item in Get-ChildItem -LiteralPath $Path -Force) {
        Assert-NoReparse $item.FullName
        if ($item.PSIsContainer) {
            if ($IncludeDirectories) { $item }
            Get-SafeTreeFiles $item.FullName -IncludeDirectories:$IncludeDirectories
        }
        else { $item }
    }
}

function Remove-SafeTree([string]$Path) {
    Assert-NoReparse $Path
    if (-not (Test-Path -LiteralPath $Path)) { return }
    # Preflight the entire tree before deleting anything, then recheck each entry.
    $null = @(Get-SafeTreeFiles $Path)
    foreach ($item in Get-ChildItem -LiteralPath $Path -Force) {
        Assert-NoReparse $item.FullName
        if ($item.PSIsContainer) { Remove-SafeTree $item.FullName }
        else { Remove-Item -LiteralPath $item.FullName -Force }
    }
    Assert-NoReparse $Path
    [IO.Directory]::Delete($Path, $false)
}

function Assert-FixedInstallPath {
    if (-not (Test-SamePath $InstallDir $MachineDir)) {
        $script:refused = $true
        throw "InstallDir must be the protected machine path $MachineDir."
    }
    Assert-NoReparse $MachineDir
}

function Assert-ProtectedFiles {
    Assert-FixedInstallPath
    foreach ($path in @($ProgramFilesRoot, $MachineDir) + @(Get-SafeTreeFiles $MachineDir -IncludeDirectories | ForEach-Object { $_.FullName })) {
        Assert-NoReparse $path
        # Check each directory too: an otherwise safe file can be replaced by
        # someone with delete-child rights on its parent.
        $current = $path
        while ($current -and ($current.Length -ge $MachineDir.Length)) {
            $findings = Get-UnsafeAclFindings $current
            if ($findings.Count) { throw "Unsafe protected install: $($findings -join '; ')" }
            $current = [IO.Path]::GetDirectoryName($current)
        }
    }
    $findings = Get-UnsafeAclFindings $ProgramFilesRoot
    if ($findings.Count) { throw "Unsafe Program Files directory: $($findings -join '; ')" }
}

function Test-AppPayloadPath([string]$Relative) {
    if ([string]::IsNullOrWhiteSpace($Relative) -or [IO.Path]::IsPathRooted($Relative) -or $Relative.Contains(':')) { return $false }
    $parts = $Relative -split '[\\/]'
    if ($parts -contains '.' -or $parts -contains '..' -or $parts -contains '') { return $false }
    if ($parts.Count -eq 1) { return $Relative -ieq 'bubbleplay_app.exe' -or $Relative -imatch '^[a-z0-9_.-]+\.dll$' }
    return $parts[0] -ieq 'data'
}

function Get-PackageSources {
    $sources = @{}
    $baseNames = if ($AppOnly) { @() } else { @($UserBinaries + $UserScripts) }
    foreach ($name in $baseNames) {
        # A flat public package owns its scripts. Source/dev builds use scripts
        # beside this setup script, never a script discovered in a data folder.
        $sources[$name] = if ($UserScripts -contains $name) { Join-Path $PSScriptRoot $name } else { Join-Path $SourceDir $name }
    }
    foreach ($name in @('QUICKSTART.md', 'THIRD-PARTY-NOTICES.md', 'LICENSE', 'release-manifest.json')) {
        if ($AppOnly) { break }
        $path = Join-Path $SourceDir $name
        if (Test-Path -LiteralPath $path -PathType Leaf) { $sources[$name] = $path }
    }
    $appRoot = $AppDir
    if (-not $appRoot -and (Test-Path -LiteralPath (Join-Path $SourceDir 'bubbleplay_app.exe') -PathType Leaf)) { $appRoot = $SourceDir }
    if ($appRoot) {
        $appRoot = [IO.Path]::GetFullPath($appRoot).TrimEnd('\')
        if (-not (Test-Path -LiteralPath (Join-Path $appRoot 'bubbleplay_app.exe') -PathType Leaf)) { throw "Missing bubbleplay_app.exe in $appRoot." }
        foreach ($file in Get-SafeTreeFiles $appRoot) {
            $relative = $file.FullName.Substring($appRoot.Length + 1)
            if (Test-AppPayloadPath $relative) { $sources[$relative] = $file.FullName }
        }
    }
    elseif ($AppOnly) { throw '-AppOnly requires -AppDir containing bubbleplay_app.exe.' }
    foreach ($path in $sources.Values) {
        Assert-NoReparse $path
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing $path (build first)." }
    }
    # The manifest is an inventory, not a signing authority. The package the user
    # selected is the trust root. Check its complete allowlist and hashes before
    # copying so truncation, unexpected entries and stale mixed builds fail closed.
    if ($sources.ContainsKey('release-manifest.json')) {
        $manifest = Get-Content -LiteralPath $sources['release-manifest.json'] -Raw | ConvertFrom-Json
        if ($manifest.schema -ne 'bubbleplay.public-release.v1' -or $manifest.profile -ne 'per-machine-tray') { throw 'Unsupported release manifest layout.' }
        $seen = @{}
        foreach ($file in @($manifest.files)) {
            $relative = ([string]$file.path).Replace('/', '\')
            if (-not $sources.ContainsKey($relative) -or $relative -ieq 'release-manifest.json' -or $seen.ContainsKey($relative)) { throw "Invalid release inventory entry: $relative" }
            $seen[$relative] = $true
            if ((Get-Sha256 $sources[$relative]) -ine [string]$file.sha256 -or (Get-Item -LiteralPath $sources[$relative]).Length -ne $file.size) { throw "Release inventory mismatch: $relative" }
        }
        if ($seen.Count -ne ($sources.Count - 1)) { throw 'Release inventory does not cover the complete payload.' }
    }
    return $sources
}

# Initial trusted bootstrap only: after this returns no elevated code reads a
# source package or a user directory. All registration uses protected copies.
function Install-MachineFiles([hashtable]$Sources) {
    Assert-FixedInstallPath
    if (-not $PSCmdlet.ShouldProcess($MachineDir, 'Copy and verify the selected program package with inherited Program Files permissions')) { return }
    $findings = Get-UnsafeAclFindings $ProgramFilesRoot
    if ($findings.Count) { throw "Unsafe Program Files directory: $($findings -join '; ')" }
    if (Test-Path -LiteralPath $MachineDir) { Assert-ProtectedFiles }
    else { New-Item -ItemType Directory -Path $MachineDir | Out-Null; Reset-ToInheritedAcl $MachineDir }
    $hashes = @{}
    foreach ($relative in $Sources.Keys) { $hashes[$relative] = Get-Sha256 $Sources[$relative] }
    foreach ($relative in $Sources.Keys) {
        $target = Join-Path $MachineDir $relative
        Assert-NoReparse $target
        $directory = Split-Path -Parent $target
        if (-not (Test-Path -LiteralPath $directory)) { [void][IO.Directory]::CreateDirectory($directory) }
        $parent = $directory
        while ($parent.Length -ge $MachineDir.Length) {
            Assert-NoReparse $parent
            Reset-ToInheritedAcl $parent
            if (Test-SamePath $parent $MachineDir) { break }
            $parent = [IO.Path]::GetDirectoryName($parent)
        }
        if (-not (Test-SamePath $Sources[$relative] $target) -and (Get-Sha256OrNull $target) -ne $hashes[$relative]) {
            # .NET File.Copy copies bytes into a new object; it does not import
            # permissive source ACLs. The verified protected parent owns the temp.
            $temporary = Join-Path $directory ('.install-' + [Guid]::NewGuid().ToString('N') + '.tmp')
            try {
                [IO.File]::Copy($Sources[$relative], $temporary, $false)
                Reset-ToInheritedAcl $temporary
                if ((Get-Sha256 $temporary) -ne $hashes[$relative]) { throw "Package changed while copying $relative." }
                Move-Item -LiteralPath $temporary -Destination $target -Force
            }
            finally { if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force } }
        }
        Reset-ToInheritedAcl $target
        if ((Get-Sha256 $target) -ne $hashes[$relative]) { throw "Hash mismatch after copying $relative." }
    }
    if ($Sources.ContainsKey('bubbleplay_app.exe')) {
        $appManifest = Join-Path $MachineDir 'app-files.json'
        $appFiles = @($Sources.Keys | Where-Object { Test-AppPayloadPath $_ } | Sort-Object)
        if (Test-Path -LiteralPath $appManifest -PathType Leaf) {
            # The old inventory is already protected and ACL-checked. Corrupt
            # inventories own nothing; never guess which files may be deleted.
            $old = $null
            try { $old = Get-Content -LiteralPath $appManifest -Raw | ConvertFrom-Json } catch { Write-Warning 'Ignoring unreadable protected app inventory.' }
            if ($old -and $old.PSObject.Properties['files']) {
                foreach ($file in @($old.files)) {
                    $relative = ([string]$file.path).Replace('/', '\')
                    if ((Test-AppPayloadPath $relative) -and $appFiles -notcontains $relative) {
                        $path = Join-Path $MachineDir $relative
                        Assert-NoReparse $path
                        if (Test-Path -LiteralPath $path -PathType Leaf) { Remove-Item -LiteralPath $path -Force }
                    }
                }
            }
        }
        $files = @($appFiles | ForEach-Object { [ordered]@{ path = $_; sha256 = $hashes[$_] } })
        [IO.File]::WriteAllText($appManifest, ([ordered]@{ version = 1; files = $files } | ConvertTo-Json -Depth 4), [Text.UTF8Encoding]::new($false))
        Reset-ToInheritedAcl $appManifest
    }
    Assert-ProtectedFiles
}

function Remove-LegacyUserFiles([hashtable]$Sources) {
    if (-not (Test-Path -LiteralPath $LegacyUserDir)) { return }
    if (-not $IsWhatIf) {
        Assert-ProtectedFiles
        foreach ($relative in $Sources.Keys) {
            if ((Get-Sha256 (Join-Path $MachineDir $relative)) -ne (Get-Sha256 $Sources[$relative])) { throw "Protected copy verification failed: $relative" }
        }
    }
    if ($PSCmdlet.ShouldProcess($LegacyUserDir, 'Delete legacy per-user program files after verifying protected replacement (data retained)')) { Remove-SafeTree $LegacyUserDir }
}

function Test-UserInstallExists {
    foreach ($path in @($TrayExe, (Join-Path $LegacyUserDir 'bubbleplay_tray.exe'), $HostConfigPath, $TraySettingsPath, $UninstallKey)) {
        if (Test-Path -LiteralPath $path) { return $true }
    }
    return $null -ne (Get-RunValue)
}

function Set-UserAutostart([bool]$ExistingInstall) {
    # Updating the command of an enabled legacy install must not enable a user
    # who disabled autostart. Keep tray.json independent of the HKCU Run state.
    $command = "`"$TrayExe`""
    $current = Get-RunValue
    if ($ExistingInstall -and [string]::IsNullOrWhiteSpace($current)) {
        Add-Step 'autostart' 'unchanged' 'Existing install has autostart disabled; HKCU Run is unchanged.'
    }
    elseif ($current -eq $command) { Add-Step 'autostart' 'unchanged' "HKCU Run\$RunValue already set." }
    elseif ($PSCmdlet.ShouldProcess("$RunKey\$RunValue", "Set to $command")) {
        New-Item -Path $RunKey -Force -ErrorAction SilentlyContinue | Out-Null
        Set-ItemProperty -Path $RunKey -Name $RunValue -Value $command
        Add-Step 'autostart' 'done' "HKCU Run\$RunValue = $command"
    }
    else { Add-Step 'autostart' 'planned' "Set HKCU Run\$RunValue = $command" }

    if ($ExistingInstall) {
        Add-Step 'tray-settings' 'unchanged' 'Existing tray settings, including startAtLogin, are unchanged.'
        return
    }
    $settings = [ordered]@{ startAtLogin = $true }
    if ($PSCmdlet.ShouldProcess($TraySettingsPath, 'Write fresh-install startAtLogin=true')) {
        New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
        [IO.File]::WriteAllText($TraySettingsPath, ($settings | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
        Add-Step 'tray-settings' 'done' 'Fresh install: startAtLogin=true'
    }
    else { Add-Step 'tray-settings' 'planned' "Write $TraySettingsPath (fresh install, startAtLogin=true)." }
}

function Set-UserRegistration([bool]$ExistingInstall) {
    Set-UserAutostart $ExistingInstall

    # Start-menu shortcut
    if ($PSCmdlet.ShouldProcess($StartMenuShortcut, "Create shortcut to $TrayExe")) {
        New-Item -ItemType Directory -Force -Path (Split-Path $StartMenuShortcut) | Out-Null
        $shell = New-Object -ComObject WScript.Shell
        $link = $shell.CreateShortcut($StartMenuShortcut)
        $link.TargetPath = $TrayExe
        $link.WorkingDirectory = $InstallDir
        $link.Description = 'BubblePlay tray'
        $link.IconLocation = "$TrayExe,0"
        $link.Save()
        Add-Step 'start-menu' 'done' $StartMenuShortcut
    }
    else { Add-Step 'start-menu' 'planned' "Create $StartMenuShortcut -> $TrayExe" }

    # Send To entry (file transfer): bubbleplay_tray.exe --send-files <paths>
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
        Add-Step 'send-to' 'done' $SendToShortcut
    }
    else { Add-Step 'send-to' 'planned' "Create $SendToShortcut -> $TrayExe --send-files" }

    # Apps & features entry (per user)
    $uninstallScript = Join-Path $InstallDir 'uninstall-tray.ps1'
    $uninstallCommand = "`"$PowerShellExe`" -NoProfile -ExecutionPolicy Bypass -File `"$uninstallScript`""
    if ($PSCmdlet.ShouldProcess($UninstallKey, 'Register per-user uninstall entry')) {
        New-Item -Path $UninstallKey -Force | Out-Null
        $sizeKb = 0
        try { $sizeKb = [int](((Get-ChildItem -LiteralPath $InstallDir -Recurse -File | Measure-Object Length -Sum).Sum) / 1KB) } catch { }
        $values = [ordered]@{
            DisplayName     = 'BubblePlay'
            Publisher       = 'BubblePlay'
            DisplayIcon     = "$TrayExe,0"
            InstallLocation = $InstallDir
            UninstallString = $uninstallCommand
            InstallDate     = [DateTime]::Now.ToString('yyyyMMdd')
        }
        foreach ($entry in $values.GetEnumerator()) { Set-ItemProperty -Path $UninstallKey -Name $entry.Key -Value $entry.Value }
        foreach ($name in 'NoModify', 'NoRepair') { New-ItemProperty -Path $UninstallKey -Name $name -PropertyType DWord -Value 1 -Force | Out-Null }
        New-ItemProperty -Path $UninstallKey -Name EstimatedSize -PropertyType DWord -Value $sizeKb -Force | Out-Null
        Add-Step 'uninstall-entry' 'done' "$UninstallKey -> $uninstallCommand"
    }
    else { Add-Step 'uninstall-entry' 'planned' "Register $UninstallKey (UninstallString: $uninstallCommand)" }
}

function Start-Tray {
    if ($NoLaunch) { Add-Step 'launch' 'skipped' '-NoLaunch given.'; return }
    if ($IsElevated) { throw 'Only the unelevated parent may launch the tray.' }
    if (-not $PSCmdlet.ShouldProcess($TrayExe, 'Start tray')) { Add-Step 'launch' 'planned' "Start $TrayExe."; return }
    Start-Process -FilePath $TrayExe -WorkingDirectory $InstallDir
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        Start-Sleep -Milliseconds 500
        $tray = Get-Process bubbleplay_tray -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -and (Test-SamePath $_.Path $TrayExe) } | Select-Object -First 1
    } until ($tray -or [DateTime]::UtcNow -gt $deadline)
    if (-not $tray) { throw 'The installed tray did not start. Start BubblePlay from the Start menu or sign out and back in.' }
    Add-Step 'launch' 'done' "Tray running (pid $($tray.Id))."
}

# --- Machine part: what needs the (single) elevation ---------------------------------
$WriteRights = [int64]([Security.AccessControl.FileSystemRights]::WriteData -bor
    [Security.AccessControl.FileSystemRights]::AppendData -bor
    [Security.AccessControl.FileSystemRights]::WriteExtendedAttributes -bor
    [Security.AccessControl.FileSystemRights]::WriteAttributes -bor
    [Security.AccessControl.FileSystemRights]::Delete -bor
    [Security.AccessControl.FileSystemRights]::DeleteSubdirectoriesAndFiles -bor
    [Security.AccessControl.FileSystemRights]::ChangePermissions -bor
    [Security.AccessControl.FileSystemRights]::TakeOwnership) -bor 0x40000000 -bor 0x10000000 # GENERIC_WRITE, GENERIC_ALL

function Get-UnsafeAclFindings([string]$Path) {
    $findings = @()
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { $findings += "$Path is a reparse point" }
    $acl = Get-Acl -LiteralPath $Path
    $owner = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value
    if ($TrustedSids -notcontains $owner) { $findings += "$Path owner $owner is not SYSTEM/Administrators/TrustedInstaller" }
    foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
        if ($rule.AccessControlType -ne 'Allow') { continue }
        $sid = $rule.IdentityReference.Value
        # CREATOR OWNER only applies to objects owned by the (trusted) creator.
        if ($TrustedSids -contains $sid -or $sid -eq 'S-1-3-0') { continue }
        if (([int64]$rule.FileSystemRights -band $WriteRights) -ne 0) {
            $findings += "$Path grants $($rule.FileSystemRights) to $sid"
        }
    }
    return , $findings
}

function Get-AgentAclFindings {
    $findings = @()
    foreach ($path in @($MachineDir, $AgentDir, $AgentExe, $HelperExe, (Join-Path $AgentDir 'logs'))) {
        try { if (Test-Path -LiteralPath $path) { $findings += Get-UnsafeAclFindings $path } }
        catch { $findings += "$path is not inspectable: $($_.Exception.Message)" }
    }
    return , $findings
}

# Everything the elevated part would change, as short descriptions.
function Get-MachineWork([bool]$ConfigNeedsMachine) {
    $work = [Collections.Generic.List[string]]::new()
    if (-not $AgentOnly) { $work.Add("copy and verify all selected program files in $MachineDir using inherited Program Files ACLs") }
    if ($AppOnly) { return , $work }
    if ($SkipAgent) {
        if ((Test-Task $AgentTaskName) -or (Test-Task $HelperTaskName) -or (Test-Task $AdminModeOffTaskName) -or (Test-AdminModeKeyExists) -or (Test-Path -LiteralPath $AgentDir)) {
            $work.Add("remove privileged helpers (tasks $TaskPath$AgentTaskName, $TaskPath$HelperTaskName and $TaskPath$AdminModeOffTaskName, $AdminModeKey, $AgentDir) because of -SkipAgent")
        }
    }
    else {
        foreach ($name in $PrivilegedBinaries) {
            $source = Join-Path $SourceDir $name
            $target = Join-Path $AgentDir $name
            $sourceHash = Get-Sha256OrNull $source
            $installedHash = Get-Sha256OrNull $target
            if (-not $sourceHash) { $work.Add("install $name (source $source is missing: build first)") }
            elseif ($installedHash -ne $sourceHash) {
                $what = if ($installedHash) { 'update' } else { 'install' }
                $work.Add("$what $name -> $target (protected ACL: SYSTEM/Administrators full, Users read+execute; verified)")
            }
        }
        if ((Get-AgentAclFindings).Count -gt 0) { $work.Add("repair the privileged helper folder ACL in $AgentDir") }
        if (-not (Test-AgentTaskCurrent)) { $work.Add("register/repair on-demand task $TaskPath$AgentTaskName (protected action; user $UserName, RunLevel Highest, user may only query/run/end it)") }
        if (-not (Test-SecureTaskCurrent)) { $work.Add("register/repair on-demand task $TaskPath$HelperTaskName (SYSTEM, --launch; protected DACL: this user read+execute, SYSTEM/Administrators full; verified)") }
        if (-not (Test-SystemHelperTaskCurrent $AdminModeOffTaskName '--set-admin-mode off')) { $work.Add("register/repair on-demand task $TaskPath$AdminModeOffTaskName (SYSTEM, --set-admin-mode off; protected DACL: this user read+execute, SYSTEM/Administrators full; verified)") }
        if (-not (Test-AdminModeKeyCurrent)) { $work.Add("create/repair $AdminModeKey (protected DACL: SYSTEM/Administrators full, Users read only; initialise AdminMode DWORD=0; no tray.json migration)") }
    }
    if (Test-Task $LegacyTaskName) { $work.Add("remove the old elevated-host task $TaskPath$LegacyTaskName") }
    if (Test-Task $LaunchTaskName) { $work.Add("remove the old tray launch task $TaskPath$LaunchTaskName") }
    if (Test-Path -LiteralPath $LegacyElevatedDir) { $work.Add("remove the old elevated host copy $LegacyElevatedDir") }
    if (-not $AgentOnly) {
        $services = @(Get-RemovableServiceNames | Where-Object { Get-Service -Name $_ -ErrorAction SilentlyContinue })
        if ($services.Count) { $work.Add("stop and delete services $($services -join ', ') (ProgramData is kept)") }
    }
    return , $work
}

function Stop-AgentProcesses {
    if (-not $PSCmdlet.ShouldProcess('installed privileged helper processes', 'Stop before replacing/removing protected binaries')) {
        Add-Step 'stop-helpers' 'planned' 'Stop installed admin agent and secure helper processes (SecureDesktop task is only a launcher; do not end it).'
        return
    }
    foreach ($name in @($AgentTaskName, $LegacyTaskName)) {
        if (Test-Task $name) { [void](Invoke-Schtasks @('/end', '/tn', "$TaskPath$name")) }
    }
    $roots = @($MachineDir)
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    do {
        $busy = @(Get-Process bubbleplay_admin_agent, bubbleplay_secure_helper, bubbleplay_tray, bubbleplay-host, bubbleplay_app -ErrorAction SilentlyContinue |
                Where-Object { $p = $_.Path; $p -and @($roots | Where-Object { $p.StartsWith("$($_.TrimEnd('\'))\", [StringComparison]::OrdinalIgnoreCase) }).Count })
        if ($busy.Count -eq 0) { return }
        $busy | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 300
    } while ([DateTime]::UtcNow -lt $deadline)
}

function Reset-ToInheritedAcl([string]$Path) {
    $acl = Get-Acl -LiteralPath $Path
    $acl.SetOwner([Security.Principal.SecurityIdentifier]'S-1-5-32-544')
    $acl.SetAccessRuleProtection($false, $false)
    foreach ($rule in @($acl.GetAccessRules($true, $false, [Security.Principal.SecurityIdentifier]))) { [void]$acl.RemoveAccessRule($rule) }
    Set-Acl -LiteralPath $Path -AclObject $acl
}

function Assert-RemovableAgentDir {
    $expected = [IO.Path]::GetFullPath((Join-Path $MachineDir 'agent'))
    if (-not (Test-SamePath $AgentDir $expected)) { throw "Refusing unexpected privileged directory: $AgentDir" }
    foreach ($path in @($MachineDir, $AgentDir)) {
        if ((Test-Path -LiteralPath $path) -and ((Get-Item -LiteralPath $path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw "$path is a reparse point." }
    }
}

function Install-Agent {
    if ($SkipAgent) {
        foreach ($name in @($AgentTaskName, $HelperTaskName, $AdminModeOffTaskName)) {
            if ((Test-Task $name) -and $PSCmdlet.ShouldProcess("$TaskPath$name", 'Unregister (because -SkipAgent)')) {
                Unregister-ScheduledTask -TaskPath $TaskPath -TaskName $name -Confirm:$false
            }
        }
        if ((Test-Path -LiteralPath $AgentDir) -and $PSCmdlet.ShouldProcess($AgentDir, 'Remove (because -SkipAgent)')) {
            Assert-RemovableAgentDir
            Remove-SafeTree $AgentDir
        }
        Remove-AdminModeKey
        Add-Step 'agent' 'skipped' '-SkipAgent given; Admin mode and secure-desktop helper unavailable.'
        return
    }
    # Validate both sources before making any machine change.
    $hashes = [ordered]@{}
    foreach ($name in $PrivilegedBinaries) {
        $source = Join-Path $SourceDir $name
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "Missing $source." }
        $hashes[$name] = Get-Sha256 $source
    }
    if (-not $PSCmdlet.ShouldProcess($AgentDir, "Install admin input agent and secure-desktop helper ($($PrivilegedBinaries -join ', '))")) {
        Add-Step 'agent' 'planned' "Copy $($PrivilegedBinaries -join ', ') -> $AgentDir; set protected ACL and verify no user write access."
        return
    }
    foreach ($dir in @($MachineDir, $AgentDir)) {
        if (-not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
        if ((Get-Item -LiteralPath $dir -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "$dir is a reparse point." }
        Reset-ToInheritedAcl $dir
    }
    foreach ($path in @($AgentExe, $HelperExe, "$AgentExe.new", "$HelperExe.new", (Join-Path $AgentDir 'logs'))) {
        if ((Test-Path -LiteralPath $path) -and ((Get-Item -LiteralPath $path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw "$path is a reparse point." }
    }
    foreach ($name in $PrivilegedBinaries) {
        $target = Join-Path $AgentDir $name
        if ((Get-Sha256OrNull $target) -ne $hashes[$name]) {
            $temp = "$target.new"
            [IO.File]::Copy((Join-Path $SourceDir $name), $temp, $true)
            Move-Item -Force -LiteralPath $temp -Destination $target
        }
        Reset-ToInheritedAcl $target
        if ((Get-Sha256 $target) -ne $hashes[$name]) { throw "Hash mismatch after copying $name." }
    }
    $logs = Join-Path $AgentDir 'logs'
    if (-not (Test-Path -LiteralPath $logs)) { New-Item -ItemType Directory -Path $logs | Out-Null }
    Reset-ToInheritedAcl $logs
    $findings = Get-AgentAclFindings
    if ($findings.Count -gt 0) { throw "Privileged helper folder is not admin-only: $($findings -join '; ')" }
    $receipt.agentSha256 = $hashes[$AgentExeName]
    $receipt.secureHelperSha256 = $hashes[$HelperExeName]
    Add-Step 'agent' 'done' "Installed/current $($PrivilegedBinaries -join ', ') in $AgentDir; ACL verified admin-only."
}

function Get-SecureTaskSddl { "D:P(A;;FA;;;BA)(A;;FA;;;SY)(A;;FRFX;;;$UserSid)" }

function Test-SecureTaskDacl([string]$Actual) {
    # Task Scheduler normalises what SetSecurityDescriptor stores: it drops the
    # protected flag and adds (A;;FR;;;SY). So check the effect, not the text:
    # only SYSTEM and Administrators may hold more than read+execute, the user
    # holds read+execute, and nobody else is granted anything.
    $readExecute = 0x1200a9
    $admins = @('S-1-5-18', 'S-1-5-32-544')
    $userCanRun = $false
    foreach ($ace in [Security.AccessControl.RawSecurityDescriptor]::new($Actual).DiscretionaryAcl) {
        if ($ace.AceType -ne [Security.AccessControl.AceType]::AccessAllowed) { continue }
        $sid = $ace.SecurityIdentifier.Value
        if ($sid -in $admins) { continue }
        if ($sid -ne $UserSid -or ($ace.AccessMask -band -bnot $readExecute) -ne 0) { return $false }
        if (($ace.AccessMask -band $readExecute) -eq $readExecute) { $userCanRun = $true }
    }
    return $userCanRun
}

function Test-SystemHelperTaskCurrent([string]$Name, [string]$Arguments) {
    try {
        $task = Get-ScheduledTask -TaskPath $TaskPath -TaskName $Name -ErrorAction Stop
        if ($task.Principal.UserId -notin @('SYSTEM', 'S-1-5-18', 'NT AUTHORITY\SYSTEM') -or $task.Actions.Count -ne 1 -or
            -not (Test-SamePath $task.Actions[0].Execute $HelperExe) -or
            -not (Test-SamePath $task.Actions[0].WorkingDirectory $AgentDir) -or $task.Actions[0].Arguments -ne $Arguments -or $task.Triggers) { return $false }
        $scheduler = New-Object -ComObject Schedule.Service
        $scheduler.Connect()
        return Test-SecureTaskDacl ($scheduler.GetFolder($TaskPath.TrimEnd('\')).GetTask($Name).GetSecurityDescriptor(4))
    }
    catch { return $false }
}

function Test-AgentTaskCurrent {
    try {
        $task = Get-ScheduledTask -TaskPath $TaskPath -TaskName $AgentTaskName -ErrorAction Stop
        if ($task.Principal.UserId -notin @($UserName, $UserSid) -or $task.Principal.RunLevel -ne 'Highest' -or
            $task.Principal.LogonType -ne 'Interactive' -or $task.Actions.Count -ne 1 -or $task.Triggers -or
            -not (Test-SamePath $task.Actions[0].Execute $AgentExe) -or
            -not (Test-SamePath $task.Actions[0].WorkingDirectory $AgentDir) -or $task.Actions[0].Arguments) { return $false }
        $scheduler = New-Object -ComObject Schedule.Service
        $scheduler.Connect()
        return Test-SecureTaskDacl ($scheduler.GetFolder($TaskPath.TrimEnd('\')).GetTask($AgentTaskName).GetSecurityDescriptor(4))
    }
    catch { return $false }
}

function Test-SecureTaskCurrent { Test-SystemHelperTaskCurrent $HelperTaskName '--launch' }

function Register-SecureTask {
    if ($SkipAgent) { return }
    $fullName = "$TaskPath$HelperTaskName"
    if (-not $PSCmdlet.ShouldProcess($fullName, 'Register on-demand SYSTEM secure-desktop launcher and verify its DACL')) {
        Add-Step 'secure-task' 'planned' "Register $fullName -> `"$HelperExe`" --launch (SYSTEM, on demand; only $UserSid read+execute, SYSTEM/Administrators full; verified)."
        return
    }
    $findings = Get-AgentAclFindings
    if (-not (Test-Path -LiteralPath $HelperExe) -or $findings.Count -gt 0) { throw "Secure helper missing or unsafe: $($findings -join '; ')" }
    $action = New-ScheduledTaskAction -Execute $HelperExe -Argument '--launch' -WorkingDirectory $AgentDir
    $principal = New-ScheduledTaskPrincipal -UserId 'S-1-5-18' -LogonType ServiceAccount -RunLevel Highest
    $settings = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -Priority 4
    Register-ScheduledTask -TaskPath $TaskPath -TaskName $HelperTaskName -Action $action -Principal $principal -Settings $settings -Description 'BubblePlay secure-desktop helper launcher (viewer sessions only; host owns helper lifetime).' -Force | Out-Null
    $scheduler = New-Object -ComObject Schedule.Service
    $scheduler.Connect()
    $task = $scheduler.GetFolder($TaskPath.TrimEnd('\')).GetTask($HelperTaskName)
    $task.SetSecurityDescriptor((Get-SecureTaskSddl), 0)
    if (-not (Test-SecureTaskCurrent)) { throw 'SecureDesktop task principal/action/DACL verification failed.' }
    Add-Step 'secure-task' 'done' "Registered and verified $fullName (SYSTEM; user read+execute only)."
}

function Register-AdminModeOffTask {
    if ($SkipAgent) { return }
    $fullName = "$TaskPath$AdminModeOffTaskName"
    if (-not $PSCmdlet.ShouldProcess($fullName, 'Register on-demand SYSTEM Admin mode OFF task and verify its DACL')) {
        Add-Step 'admin-mode-off-task' 'planned' "Register $fullName -> `"$HelperExe`" --set-admin-mode off (SYSTEM, no triggers; only $UserSid read+execute, SYSTEM/Administrators full; verified)."
        return
    }
    $findings = Get-AgentAclFindings
    if (-not (Test-Path -LiteralPath $HelperExe) -or $findings.Count -gt 0) { throw "Secure helper missing or unsafe: $($findings -join '; ')" }
    $action = New-ScheduledTaskAction -Execute $HelperExe -Argument '--set-admin-mode off' -WorkingDirectory $AgentDir
    $principal = New-ScheduledTaskPrincipal -UserId 'S-1-5-18' -LogonType ServiceAccount -RunLevel Highest
    $settings = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -ExecutionTimeLimit ([TimeSpan]::FromMinutes(1)) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -Priority 4
    Register-ScheduledTask -TaskPath $TaskPath -TaskName $AdminModeOffTaskName -Action $action -Principal $principal -Settings $settings -Description 'BubblePlay Admin mode OFF only (on demand; never enables input).' -Force | Out-Null
    $scheduler = New-Object -ComObject Schedule.Service
    $scheduler.Connect()
    $task = $scheduler.GetFolder($TaskPath.TrimEnd('\')).GetTask($AdminModeOffTaskName)
    $task.SetSecurityDescriptor((Get-SecureTaskSddl), 0)
    if (-not (Test-SystemHelperTaskCurrent $AdminModeOffTaskName '--set-admin-mode off')) { throw 'AdminModeOff task principal/action/DACL verification failed.' }
    Add-Step 'admin-mode-off-task' 'done' "Registered and verified $fullName (SYSTEM; user read+execute only)."
}

# Explicit Registry64 matches the protected helper even from 32-bit PowerShell.
function Open-AdminModeRegistry {
    [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, [Microsoft.Win32.RegistryView]::Registry64)
}

function Test-AdminModeKeyExists {
    $base = Open-AdminModeRegistry
    try {
        $key = $base.OpenSubKey($AdminModeRegistryPath, $false)
        if ($null -eq $key) { return $false }
        $key.Dispose()
        return $true
    }
    catch { return $true } # Access denied still needs an elevated removal/repair.
    finally { $base.Dispose() }
}

function Test-AdminModeKeyCurrent {
    $base = Open-AdminModeRegistry
    try {
        $key = $base.OpenSubKey($AdminModeRegistryPath, $false)
        if ($null -eq $key) { return $false }
        try {
            if ($key.GetValueKind('AdminMode') -ne [Microsoft.Win32.RegistryValueKind]::DWord) { return $false }
            $acl = $key.GetAccessControl()
            if (-not $acl.AreAccessRulesProtected -or $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -ne 'S-1-5-32-544') { return $false }
            $expected = @{ 'S-1-5-18' = [Security.AccessControl.RegistryRights]::FullControl; 'S-1-5-32-544' = [Security.AccessControl.RegistryRights]::FullControl; 'S-1-5-32-545' = [Security.AccessControl.RegistryRights]::ReadKey }
            $rules = @($acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]))
            if ($rules.Count -ne $expected.Count) { return $false }
            foreach ($rule in $rules) {
                $sid = $rule.IdentityReference.Value
                if (-not $expected.ContainsKey($sid) -or $rule.IsInherited -or $rule.AccessControlType -ne 'Allow' -or $rule.RegistryRights -ne $expected[$sid]) { return $false }
                $expected.Remove($sid)
            }
            return $expected.Count -eq 0
        }
        finally { $key.Dispose() }
    }
    catch { return $false }
    finally { $base.Dispose() }
}

function Install-AdminModeKey {
    if ($SkipAgent) { return }
    if (-not $PSCmdlet.ShouldProcess($AdminModeKey, 'Create/repair protected policy key; keep AdminMode if the key was already protected, else initialise DWORD=0 (no tray.json migration)')) {
        Add-Step 'admin-mode-policy' 'planned' "Create/repair $AdminModeKey; protected DACL SYSTEM/Administrators full, Users read only; AdminMode kept if the key was already protected, else DWORD=0."
        return
    }
    $acl = [Security.AccessControl.RegistrySecurity]::new()
    $acl.SetOwner([Security.Principal.SecurityIdentifier]'S-1-5-32-544')
    $acl.SetAccessRuleProtection($true, $false)
    foreach ($entry in @(@('S-1-5-18', 'FullControl'), @('S-1-5-32-544', 'FullControl'), @('S-1-5-32-545', 'ReadKey'))) {
        $acl.AddAccessRule([Security.AccessControl.RegistryAccessRule]::new(
                [Security.Principal.SecurityIdentifier]$entry[0], [Security.AccessControl.RegistryRights]$entry[1],
                [Security.AccessControl.InheritanceFlags]::None, [Security.AccessControl.PropagationFlags]::None,
                [Security.AccessControl.AccessControlType]::Allow))
    }
    # Keep the owner's choice across updates, but only when the key was already protected:
    # a value written under a broken DACL could have come from a non-admin, so reset it.
    $keepExisting = Test-AdminModeKeyCurrent
    $base = Open-AdminModeRegistry
    try {
        $key = $base.CreateSubKey($AdminModeRegistryPath, [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree,
            [Microsoft.Win32.RegistryOptions]::None, $acl)
        try {
            $key.SetAccessControl($acl)
            if (-not $keepExisting) { $key.SetValue('AdminMode', 0, [Microsoft.Win32.RegistryValueKind]::DWord) }
        }
        finally { $key.Dispose() }
    }
    finally { $base.Dispose() }
    if (-not (Test-AdminModeKeyCurrent)) { throw 'Admin mode registry type/owner/protected DACL verification failed.' }
    Add-Step 'admin-mode-policy' 'done' "$AdminModeKey $(if ($keepExisting) { 'kept the existing AdminMode value' } else { 'initialised OFF' }); protected DACL verified."
}

function Remove-AdminModeKey {
    if (-not (Test-AdminModeKeyExists)) { return }
    if (-not $PSCmdlet.ShouldProcess($AdminModeKey, 'Delete protected Admin mode policy (because -SkipAgent)')) {
        Add-Step 'admin-mode-policy' 'planned' "Remove $AdminModeKey (because -SkipAgent)."
        return
    }
    $base = Open-AdminModeRegistry
    try { $base.DeleteSubKeyTree($AdminModeRegistryPath, $false) }
    finally { $base.Dispose() }
    Add-Step 'admin-mode-policy' 'done' "Removed $AdminModeKey."
}

function Register-AgentTask {
    if ($SkipAgent) { return }
    $fullName = "$TaskPath$AgentTaskName"
    if (-not $PSCmdlet.ShouldProcess($fullName, "Register on-demand RunLevel Highest task for $UserName")) {
        Add-Step 'agent-task' 'planned' "Register $fullName -> `"$AgentExe`" (user $UserName, Highest, on demand, grant user read+run)."
        return
    }
    if (-not (Test-Path -LiteralPath $AgentExe)) { throw "Missing $AgentExe." }
    $findings = Get-AgentAclFindings
    if ($findings.Count -gt 0) { throw "Agent folder is not admin-only: $($findings -join '; ')" }
    $action = New-ScheduledTaskAction -Execute $AgentExe -WorkingDirectory $AgentDir
    $principal = New-ScheduledTaskPrincipal -UserId $UserName -LogonType Interactive -RunLevel Highest
    $settings = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -ExecutionTimeLimit ([TimeSpan]::Zero) `
        -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -Priority 4
    Register-ScheduledTask -TaskPath $TaskPath -TaskName $AgentTaskName -Action $action -Principal $principal `
        -Settings $settings -Description 'BubblePlay admin input agent (started by the tray while Admin mode is on).' -Force | Out-Null
    # Let the (non-elevated) user query, run and end it; only admins may change it.
    $scheduler = New-Object -ComObject Schedule.Service
    $scheduler.Connect()
    $task = $scheduler.GetFolder($TaskPath.TrimEnd('\')).GetTask($AgentTaskName)
    $task.SetSecurityDescriptor((Get-SecureTaskSddl), 0)
    if (-not (Test-AgentTaskCurrent)) { throw 'AdminAgent task principal/action/DACL verification failed.' }
    Add-Step 'agent-task' 'done' "Registered and verified $fullName for $UserName."
}

function Remove-LegacyElevated {
    $fullName = "$TaskPath$LegacyTaskName"
    if (Test-Task $LegacyTaskName) {
        if ($PSCmdlet.ShouldProcess($fullName, 'Unregister old elevated-host task')) {
            Unregister-ScheduledTask -TaskPath $TaskPath -TaskName $LegacyTaskName -Confirm:$false
            Add-Step 'legacy-task' 'done' "Removed $fullName."
        }
        else { Add-Step 'legacy-task' 'planned' "Remove $fullName." }
    }
    if (Test-Path -LiteralPath $LegacyElevatedDir) {
        if ($PSCmdlet.ShouldProcess($LegacyElevatedDir, 'Delete old elevated host copy')) {
            $deadline = [DateTime]::UtcNow.AddSeconds(10)
            while ($true) {
                try { Remove-SafeTree $LegacyElevatedDir; break }
                catch { if ([DateTime]::UtcNow -gt $deadline) { throw }; Start-Sleep -Milliseconds 500 }
            }
            Add-Step 'legacy-dir' 'done' "Deleted $LegacyElevatedDir."
        }
        else { Add-Step 'legacy-dir' 'planned' "Delete $LegacyElevatedDir." }
    }
}

function Get-RemovableServiceNames {
    if ($KeepServices) { return @($TransportServiceName) }
    return @($ServiceNames)
}

function Remove-BubblePlayServices {
    $present = @(Get-RemovableServiceNames | Where-Object { Get-Service -Name $_ -ErrorAction SilentlyContinue })
    if ($present.Count -eq 0) {
        Add-Step 'services' 'unchanged' 'No BubblePlay services registered.'
    }
    elseif ($PSCmdlet.ShouldProcess(($present -join ', '), 'Stop and delete service registrations')) {
        $sc = Join-Path $SystemDirectory 'sc.exe'
        foreach ($name in $present) {
            $service = Get-Service -Name $name
            if ($service.Status -ne 'Stopped') {
                Stop-Service -Name $name -Force
                $service.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(30))
            }
            & $sc delete $name | Out-Null
            if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 1072) { throw "sc.exe delete $name failed with $LASTEXITCODE" }
        }
        Add-Step 'services' 'done' "Stopped and deleted: $($present -join ', ')."
    }
    else {
        Add-Step 'services' 'planned' "Stop and delete: $($present -join ', ') (ProgramData is kept)."
    }
    if ($KeepServices) { Add-Step 'services-kept' 'skipped' '-KeepServices given; only Transport is removed (decision 25).'; return }

    $leftovers = @(Get-Process -Name $ServiceProcessNames -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -and $_.Path.StartsWith($MachineDir.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase) })
    if ($leftovers.Count -gt 0) {
        if ($PSCmdlet.ShouldProcess(($leftovers | ForEach-Object { "$($_.Name):$($_.Id)" }) -join ', ', 'Stop leftover service processes')) {
            $leftovers | Stop-Process -Force
            Add-Step 'service-processes' 'done' "Stopped $($leftovers.Count) leftover process(es)."
        }
        else { Add-Step 'service-processes' 'planned' "Stop $($leftovers.Count) leftover process(es) under $MachineDir." }
    }

    # Old ProgramData hash receipts are user data and are never consumed by an
    # elevated install. Obsolete files remain protected until full uninstall.

    $qos = @()
    if (Get-Command Get-NetQosPolicy -ErrorAction SilentlyContinue) {
        foreach ($store in @('ActiveStore', $null)) {
            $query = @{ ErrorAction = 'SilentlyContinue' }
            if ($store) { $query.PolicyStore = $store }
            $qos += @(Get-NetQosPolicy @query | Where-Object { $_.Name -like 'BubblePlay-StreamLab-*' -or $_.Name -like 'BubblePlayLab-*' } |
                    ForEach-Object { [pscustomobject]@{ Name = $_.Name; Store = $store } })
        }
    }
    if ($qos.Count -eq 0) { Add-Step 'qos' 'unchanged' 'No Stream Lab QoS policies present.'; return }
    if ($PSCmdlet.ShouldProcess(($qos.Name -join ', '), 'Remove leftover Stream Lab QoS policies')) {
        foreach ($policy in $qos) {
            $remove = @{ Name = $policy.Name; Confirm = $false; ErrorAction = 'SilentlyContinue' }
            if ($policy.Store) { $remove.PolicyStore = $policy.Store }
            Remove-NetQosPolicy @remove
        }
        Add-Step 'qos' 'done' "Removed: $($qos.Name -join ', ')."
    }
    else { Add-Step 'qos' 'planned' "Remove: $($qos.Name -join ', ')." }
}

# Only protected code reaches task/policy registration. No user configuration,
# service-data directory or receipt path is read or written in this phase.
function Invoke-MachineSteps([bool]$ConfigNeedsMachine) {
    Assert-ProtectedFiles
    if ($AppOnly) { return }
    Stop-AgentProcesses
    Install-Agent
    Register-AgentTask
    Register-SecureTask
    Install-AdminModeKey
    Register-AdminModeOffTask
    Remove-LegacyElevated
    if (-not $AgentOnly) { Remove-BubblePlayServices }
    # Remove every obsolete task action, including temporary legacy tray launch.
    if ((Test-Task $LaunchTaskName) -and $PSCmdlet.ShouldProcess("$TaskPath$LaunchTaskName", 'Remove old tray launch task')) {
        Unregister-ScheduledTask -TaskPath $TaskPath -TaskName $LaunchTaskName -Confirm:$false
    }
}

function Invoke-ElevatedMachinePhase([Collections.Generic.List[string]]$Work) {
    $summary = 'One UAC prompt: ' + ($Work -join '; ')
    if (-not $PSCmdlet.ShouldProcess($MachineDir, $summary)) {
        Add-Step 'elevate' 'planned' $summary
        return
    }
    # Agent repair must not elevate a source/downloaded script. Full setup uses
    # the explicitly selected package as its initial trusted bootstrap only.
    $script = if ($AgentOnly) { Join-Path $MachineDir 'setup-tray.ps1' } else { $PSCommandPath }
    if ($AgentOnly) { Assert-ProtectedFiles }
    $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$script`"", '-MachinePhase')
    if ($AgentOnly) { $arguments += '-AgentOnly' }
    else {
        $arguments += @('-SourceDir', "`"$($SourceDir.TrimEnd('\'))`"")
        if ($AppDir) { $arguments += @('-AppDir', "`"$([IO.Path]::GetFullPath($AppDir).TrimEnd('\'))`"") }
        if ($AppOnly) { $arguments += '-AppOnly' }
    }
    if ($SkipAgent) { $arguments += '-SkipAgent' }
    if ($KeepServices) { $arguments += '-KeepServices' }
    Add-Step 'elevate' 'running' $summary
    try { $child = Start-Process -FilePath $PowerShellExe -ArgumentList $arguments -Verb RunAs -Wait -PassThru -WindowStyle Hidden }
    catch { throw "The administrator prompt was declined or failed: $($_.Exception.Message)" }
    if ($child.ExitCode -ne 0) { throw "The elevated install failed with exit code $($child.ExitCode). User configuration and receipts are handled by this unelevated parent." }
    Add-Step 'elevate' 'done' 'Protected machine install completed.'
}

function Write-Receipt {
    if ($IsElevated -or $MachinePhase) { throw 'Machine setup never writes user receipts.' }
    $receipt.finishedUtc = [DateTimeOffset]::UtcNow.ToString('o')
    $json = $receipt | ConvertTo-Json -Depth 6
    if ($IsWhatIf) {
        Write-Host '--- plan receipt (not written under -WhatIf) ---'
        Write-Output $json
        return
    }
    try {
        $dir = Join-Path $DataDir 'receipts'
        # Includes DataDir itself and every ancestor. Check before creation so
        # even an absent receipts folder below a junction cannot redirect writes.
        Assert-NoReparse $dir
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        Assert-NoReparse $dir
        $path = Join-Path $dir ("setup-tray-{0}-{1}.json" -f [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ'), [Guid]::NewGuid().ToString('N'))
        Assert-NoReparse $path
        # Never replace an existing file, including a planted link or hard link.
        $stream = [IO.File]::Open($path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try {
            $bytes = [Text.UTF8Encoding]::new($false).GetBytes($json)
            $stream.Write($bytes, 0, $bytes.Length)
        }
        finally { $stream.Dispose() }
        Write-Host "Receipt: $path"
    }
    catch { Write-Warning "Skipping setup receipt: $($_.Exception.Message)" -WarningAction Continue }
}

try {
    Assert-FixedInstallPath
    if ($MachinePhase -or ($AgentOnly -and $IsElevated)) {
        Assert-InteractiveUserMatches
        if (-not $IsElevated -and -not $IsWhatIf) {
            $refused = $true
            throw '-MachinePhase must run elevated.'
        }
        if ($ProtectedPhase -or $AgentOnly) {
            if (-not (Test-SamePath $PSCommandPath (Join-Path $MachineDir 'setup-tray.ps1'))) {
                throw 'Privileged repair/registration must run the protected Program Files setup script.'
            }
            $SourceDir = $MachineDir
            $AppDir = $null
            Assert-ProtectedFiles
            Invoke-MachineSteps $false
        }
        else {
            # Initial package trust boundary. Read/validate/copy source exactly
            # here; everything executed after the copy lives in Program Files.
            $sources = Get-PackageSources
            if ($AppOnly -and -not (Test-Path -LiteralPath (Join-Path $MachineDir 'setup-tray.ps1') -PathType Leaf)) { throw 'Run full setup before -AppOnly.' }
            Stop-AgentProcesses
            if (-not $AppOnly) { Remove-BubblePlayServices }
            Install-MachineFiles $sources
            if (-not $IsWhatIf) {
                $protected = Join-Path $MachineDir 'setup-tray.ps1'
                $parameters = @{ MachinePhase = $true; ProtectedPhase = $true; SourceDir = $MachineDir; NoLaunch = $true }
                if ($SkipAgent) { $parameters.SkipAgent = $true }
                if ($KeepServices) { $parameters.KeepServices = $true }
                if ($AppOnly) { $parameters.AppOnly = $true }
                & $protected @parameters
            }
        }
    }
    else {
        if ($IsElevated) {
            $refused = $true
            throw 'Run setup from a normal desktop PowerShell. Only the machine child may run elevated.'
        }
        Assert-InteractiveUserMatches
        if ($AgentOnly) {
            $SourceDir = $MachineDir
            Assert-ProtectedFiles
            $work = Get-MachineWork $false
            if ($work.Count) { Invoke-ElevatedMachinePhase $work }
            else { Add-Step 'agent' 'unchanged' 'Protected helpers and their tasks are current.' }
        }
        else {
            if (-not $AppOnly) {
                # Capture before config creation or the machine copy would make
                # every fresh install look like an update.
                $existingUserInstall = Test-UserInstallExists
                $configState = Update-HostConfig
                if ($configState -eq 'machine') { throw 'The legacy service config is unreadable as this user. Supply -WorkerUrl and -Token; elevation never reads or writes user configuration.' }
            }
            $sources = $null
            try { $sources = Get-PackageSources }
            catch { if ($IsWhatIf) { Add-Step 'package' 'blocked' $_.Exception.Message } else { throw } }
            Stop-RunningTray
            $work = Get-MachineWork $false
            Invoke-ElevatedMachinePhase $work
            if (-not $IsWhatIf) {
                Assert-ProtectedFiles
                foreach ($relative in $sources.Keys) {
                    if ((Get-Sha256 (Join-Path $MachineDir $relative)) -ne (Get-Sha256 $sources[$relative])) { throw "Protected package verification failed: $relative" }
                }
            }
            if (-not $AppOnly) {
                Set-UserRegistration $existingUserInstall
                if ($sources) { Remove-LegacyUserFiles $sources }
                $stale = Join-Path $DataDir 'elevated-runner.json'
                if ((Test-Path -LiteralPath $stale) -and $PSCmdlet.ShouldProcess($stale, 'Delete stale legacy heartbeat')) { Remove-Item -LiteralPath $stale -Force }
            }
            Start-Tray
        }
    }
    $receipt.result = if ($IsWhatIf) { 'planned' } else { 'success' }
}
catch {
    $receipt.result = 'failed'
    $receipt.error = $_.Exception.Message
    throw
}
finally {
    # Machine phases never follow LOCALAPPDATA, config or receipt paths.
    if (-not $refused -and -not $MachinePhase -and -not $IsElevated) { Write-Receipt }
}
