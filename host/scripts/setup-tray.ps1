#Requires -Version 5.1
<#
.SYNOPSIS
    Install BubblePlay as a per-user app (tray + host), plus the optional
    elevated admin input agent, with at most one UAC prompt.

.DESCRIPTION
    Run from a normal (non-elevated) PowerShell as the desktop user.

    User part (no elevation):
      - Quit a running tray (its host and admin agent exit with it).
      - Copy bubbleplay-host.exe, bubbleplay_tray.exe, the staged
        bubbleplay_admin_agent.exe, bubbleplay_secure_helper.exe, setup-tray.ps1 and uninstall-tray.ps1 to
        %LOCALAPPDATA%\Programs\BubblePlay.
      - Write %LOCALAPPDATA%\BubblePlay\host.json (worker URL + tokens,
        CurrentUser DPAPI) from -WorkerUrl/-Token/-HostGateToken or an
        existing host.json. Both tokens must contain at least 32 characters.
      - HKCU Run autostart, a Start-menu shortcut, an HKCU "Apps" uninstall
        entry (uninstall-tray.ps1) and tray.json.
      - Start the tray.

    Machine part (ONE UAC prompt, only when something needs it):
      - Install the admin input agent to %ProgramFiles%\BubblePlay\agent
        (protected ACL: SYSTEM/Administrators full, Users read+execute;
        verified) and register the on-demand task \BubblePlay\AdminAgent
        (this user, RunLevel Highest, no triggers; the user may only query,
        run and end it). The tray starts it while Admin mode is on.
      - Install bubbleplay_secure_helper.exe beside the admin agent; register
        \BubblePlay\SecureDesktop as SYSTEM, action --launch, no triggers.
        Create HKLM\SOFTWARE\BubblePlay with protected DACL (SYSTEM and
        Administrators full control, Users read only), initialised AdminMode=0.
        Register \BubblePlay\AdminModeOff as SYSTEM, --set-admin-mode off,
        no triggers. Both SYSTEM tasks use the same protected DACL.
        Its protected task DACL grants only this installing user read+execute
        and SYSTEM/Administrators full control, and is verified after setup.
      - Remove the old elevated whole-host path: the \BubblePlay\ElevatedHost
        task and %ProgramFiles%\BubblePlay\elevated.
      - Remove the old BubblePlay services, their hash-matched binaries and
        Stream Lab QoS policies; migrate the service config into host.json
        when no host.json exists yet (ProgramData is left untouched).

    -AgentOnly runs just the agent install + old elevated cleanup (used by
    the tray's "Admin agent needs update" item and update-tray.ps1
    -IncludeAgent). Both install the admin agent AND secure-desktop helper
    in one elevation. -SkipAgent installs neither helper
    and removes an installed one. -WhatIf previews everything (no
    elevation, nothing written). A receipt JSON is written per run to
    %LOCALAPPDATA%\BubblePlay\receipts.

    The secure-desktop helper runs as SYSTEM only for viewer sessions. Admin
    mode off allows view and UAC Deny; Admin mode on also allows full input.
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string]$SourceDir,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\BubblePlay'),
    [string]$ServiceDataDir = (Join-Path $env:ProgramData 'BubblePlay'),
    [string]$WorkerUrl,
    [Security.SecureString]$Token,
    [Security.SecureString]$HostGateToken,
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
    [switch]$NoLaunch
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($IncludeAgent -and $SkipAgent) { throw '-IncludeAgent and -SkipAgent cannot be combined.' }
Add-Type -AssemblyName System.Security
# Load modules up front with WhatIf off so module import noise (alias creation)
# does not show up as fake plan items; queries below pass -WhatIf:$false too.
& {
    $WhatIfPreference = $false
    foreach ($module in 'CimCmdlets', 'ScheduledTasks', 'NetQos') { Import-Module $module -ErrorAction SilentlyContinue }
}

$ProgramFilesRoot = if ($env:ProgramW6432) { $env:ProgramW6432 } else { $env:ProgramFiles }
$MachineDir = Join-Path $ProgramFilesRoot 'BubblePlay'
$AgentDir = Join-Path $MachineDir 'agent'
$AgentExeName = 'bubbleplay_admin_agent.exe'
$AgentExe = Join-Path $AgentDir $AgentExeName
$HelperExeName = 'bubbleplay_secure_helper.exe'
$HelperExe = Join-Path $AgentDir $HelperExeName
$PrivilegedBinaries = @($AgentExeName, $HelperExeName)
$LegacyElevatedDir = Join-Path $MachineDir 'elevated'
# Binaries install-service.ps1 placed in $MachineDir.
$ServiceBinaries = @('bubbleplay_broker.exe', 'bubbleplay_transport.exe', 'bubbleplay_agent.exe',
    'bubbleplay_input_proxy.exe', 'bubbleplay_lab_helper.exe', 'bubbleplay-host.exe')
$TrustedSids = @('S-1-5-18', 'S-1-5-32-544', 'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464')
$ServiceNames = @('BubblePlayLabHelper', 'BubblePlayTransport', 'BubblePlayBroker')
$ServiceProcessNames = @('bubbleplay_broker', 'bubbleplay_transport', 'bubbleplay_agent',
    'bubbleplay_input_proxy', 'bubbleplay_lab_helper', 'bubbleplay_lab_activity_guard', 'bubbleplay-host')
$UserBinaries = @('bubbleplay-host.exe', 'bubbleplay_tray.exe') + $PrivilegedBinaries
$UserScripts = @('setup-tray.ps1', 'uninstall-tray.ps1')
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

if (-not $SourceDir) { $SourceDir = Join-Path $PSScriptRoot '..\target\release' }
$SourceDir = [IO.Path]::GetFullPath($SourceDir)
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
$DataDir = Join-Path $env:LOCALAPPDATA 'BubblePlay'
$HostConfigPath = Join-Path $DataDir 'host.json'
$TraySettingsPath = Join-Path $DataDir 'tray.json'
$TrayExe = Join-Path $InstallDir 'bubbleplay_tray.exe'
$PowerShellExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
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
    $plain = [Security.Cryptography.ProtectedData]::Unprotect((ConvertFrom-Hex $Hex), $null,
        [Security.Cryptography.DataProtectionScope]::CurrentUser)
    try { return [Text.Encoding]::UTF8.GetString($plain) } finally { [Array]::Clear($plain, 0, $plain.Length) }
}

function Protect-UserHex([string]$Plain) {
    if (-not $Plain) { return $null }
    $bytes = [Text.Encoding]::UTF8.GetBytes($Plain)
    try {
        return ConvertTo-Hex ([Security.Cryptography.ProtectedData]::Protect($bytes, $null,
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
    $exe = Join-Path $env:SystemRoot 'System32\schtasks.exe'
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
        $plain = [Security.Cryptography.ProtectedData]::Unprotect((ConvertFrom-Hex $Hex), $null,
            [Security.Cryptography.DataProtectionScope]::LocalMachine)
        try { [Text.Encoding]::UTF8.GetString($plain) } finally { [Array]::Clear($plain, 0, $plain.Length) }
    }
    $gateHex = if ($json.PSObject.Properties['hostGateTokenDpapiHex']) { $json.hostGateTokenDpapiHex } else { $null }
    return [pscustomobject]@{
        Path          = $path
        Readable      = $true
        Worker        = [string]$json.worker
        Token         = & $unprotect ([string]$json.tokenDpapiHex)
        HostGateToken = & $unprotect ([string]$gateHex)
    }
}

function Get-ExistingHostConfig {
    if (-not (Test-Path -LiteralPath $HostConfigPath -PathType Leaf)) { return $null }
    try {
        $json = Get-Content -Raw -LiteralPath $HostConfigPath | ConvertFrom-Json
        $gateHex = if ($json.PSObject.Properties['hostGateTokenDpapiHex']) { $json.hostGateTokenDpapiHex } else { $null }
        $monitorValue = if ($json.PSObject.Properties['monitor']) { $json.monitor } else { $null }
        return [pscustomobject]@{
            Worker        = [string]$json.worker
            Token         = Unprotect-Hex ([string]$json.tokenDpapiHex)
            HostGateToken = Unprotect-Hex ([string]$gateHex)
            Monitor       = $monitorValue
        }
    }
    catch {
        Write-Warning "Existing host.json is unreadable and will be replaced: $($_.Exception.Message)"
        return $null
    }
}

# Returns 'machine' when only the elevated part can read the config (the old
# service config under ProgramData); otherwise writes/plans host.json itself.
function Assert-HostCredentials([string]$RegistrationToken, [string]$EndpointToken) {
    # Keep these minimums aligned with worker/src/index.js MIN_SECRET_LENGTHS.
    if ([string]::IsNullOrWhiteSpace($RegistrationToken) -or $RegistrationToken.Length -lt 32) {
        throw 'Host registration token (TOKEN / -Token) must contain at least 32 characters.'
    }
    if ([string]::IsNullOrWhiteSpace($EndpointToken) -or $EndpointToken.Length -lt 32) {
        throw 'Host endpoint token (HOST_GATE_TOKEN / -HostGateToken) must contain at least 32 characters.'
    }
}

function Update-HostConfig {
    $existing = Get-ExistingHostConfig
    $service = Get-ServiceConfig
    $explicitToken = ConvertFrom-Secure $Token
    $explicitGate = ConvertFrom-Secure $HostGateToken
    $hasExplicitToken = $null -ne $Token
    $hasExplicitGate = $null -ne $HostGateToken

    $source = $null; $worker = $null; $tok = $null; $gate = $null
    if ($WorkerUrl -or $hasExplicitToken -or $hasExplicitGate) {
        $source = 'parameters'
        $worker = if ($WorkerUrl) { $WorkerUrl } elseif ($existing) { $existing.Worker } elseif ($service -and $service.Readable) { $service.Worker } else { $null }
        $tok = if ($hasExplicitToken) { $explicitToken } elseif ($existing) { $existing.Token } elseif ($service -and $service.Readable) { $service.Token } else { $null }
        $gate = if ($hasExplicitGate) { $explicitGate } elseif ($existing) { $existing.HostGateToken } elseif ($service -and $service.Readable) { $service.HostGateToken } else { $null }
    }
    elseif ($existing) {
        $source = 'existing host.json'
        $worker = $existing.Worker; $tok = $existing.Token; $gate = $existing.HostGateToken
    }
    elseif ($service -and $service.Readable) {
        $source = "service config $($service.Path)"
        $worker = $service.Worker; $tok = $service.Token; $gate = $service.HostGateToken
    }
    elseif ($service -and -not $service.Readable) {
        return 'machine'
    }
    else {
        throw 'No existing configuration found. Pass -WorkerUrl wss://.../host-ws, -Token and -HostGateToken (SecureString).'
    }
    if (-not $worker -or $worker -notmatch '^wss?://') { throw "Worker URL must be ws:// or wss:// (got '$worker')." }
    Assert-HostCredentials $tok $gate
    $monitorValue = if ($PSBoundParameters.ContainsKey('Monitor') -or $Monitor) { $Monitor } elseif ($existing) { $existing.Monitor } else { $null }

    if ($existing -and $existing.Worker -eq $worker -and $existing.Token -eq $tok -and
        $existing.HostGateToken -eq $gate -and $existing.Monitor -eq $monitorValue) {
        Add-Step 'config' 'unchanged' "$HostConfigPath already matches ($source)."
        return 'done'
    }
    if ($PSCmdlet.ShouldProcess($HostConfigPath, "Write host config from $source (secrets CurrentUser-DPAPI)")) {
        $config = [ordered]@{
            worker                = $worker
            tokenDpapiHex         = Protect-UserHex $tok
            hostGateTokenDpapiHex = Protect-UserHex $gate
            monitor               = if ($monitorValue) { $monitorValue } else { $null }
        }
        New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
        $temp = "$HostConfigPath.tmp"
        [IO.File]::WriteAllText($temp, ($config | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
        Move-Item -Force -LiteralPath $temp -Destination $HostConfigPath
        # Round-trip to prove the tray (same user) can decrypt it.
        $check = Get-ExistingHostConfig
        if (-not $check -or $check.Token -ne $tok -or $check.HostGateToken -ne $gate) { throw 'host.json round-trip verification failed.' }
        Add-Step 'config' 'done' "Wrote $HostConfigPath from $source (worker $worker, gate token: $([bool]$gate))."
    }
    else {
        Add-Step 'config' 'planned' "Write $HostConfigPath from $source (worker $worker, gate token: $([bool]$gate))."
    }
    return 'done'
}

# --- Stop the running tray ---------------------------------------------------------
function Stop-RunningTray {
    $running = @(Get-Process bubbleplay_tray -ErrorAction SilentlyContinue)
    if ($running.Count -eq 0) { Add-Step 'stop-tray' 'skipped' 'Tray is not running.'; return }
    if (-not $PSCmdlet.ShouldProcess('bubbleplay_tray', 'Quit running tray (its host and admin agent exit with it)')) {
        Add-Step 'stop-tray' 'planned' "Quit $($running.Count) running tray process(es); the host and admin agent exit with the tray."
        return
    }
    $quitter = if (Test-Path -LiteralPath $TrayExe) { $TrayExe } else { $running[0].Path }
    if ($quitter) { Start-Process -FilePath $quitter -ArgumentList '--quit' -Wait -WindowStyle Hidden }
    Start-Sleep -Milliseconds 500
    Get-Process bubbleplay_tray -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Get-Process bubbleplay-host, bubbleplay_admin_agent -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and $_.Path.StartsWith($InstallDir, [StringComparison]::OrdinalIgnoreCase) } |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Add-Step 'stop-tray' 'done' 'Tray and its host stopped.'
}

# --- Per-user install ----------------------------------------------------------------
function Install-UserFiles {
    $hashes = [ordered]@{}
    $changed = @()
    $sources = @{}
    foreach ($name in $UserBinaries) { $sources[$name] = Join-Path $SourceDir $name }
    foreach ($name in $UserScripts) { $sources[$name] = Join-Path $PSScriptRoot $name }
    foreach ($name in @($UserBinaries + $UserScripts)) {
        $source = $sources[$name]
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            if ($IsWhatIf) { Add-Step 'user-files' 'blocked' "Missing $source (build first)."; return }
            throw "Missing $source. Build first: cargo build --offline --locked --release --bins --manifest-path host/Cargo.toml"
        }
        $target = Join-Path $InstallDir $name
        if (Test-SamePath $source $target) { continue }
        $hash = Get-Sha256 $source
        $hashes[$name] = $hash
        if ((Get-Sha256OrNull $target) -ne $hash) { $changed += $name }
    }
    $receipt.files = $hashes
    if ($changed.Count -eq 0) { Add-Step 'user-files' 'unchanged' "$InstallDir is current."; return }
    if ($PSCmdlet.ShouldProcess($InstallDir, "Copy $($changed -join ', ')")) {
        New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
        foreach ($name in $changed) {
            $target = Join-Path $InstallDir $name
            $temp = "$target.new"
            Copy-Item -Force -LiteralPath $sources[$name] -Destination $temp
            Move-Item -Force -LiteralPath $temp -Destination $target
            if ((Get-Sha256 $target) -ne $hashes[$name]) { throw "Hash mismatch after copying $name." }
        }
        Add-Step 'user-files' 'done' "Copied $($changed -join ', ') to $InstallDir."
    }
    else { Add-Step 'user-files' 'planned' "Copy $($changed -join ', ') to $InstallDir (per-user, no admin rights)." }
}

function Set-UserRegistration {
    # Autostart
    $command = "`"$TrayExe`""
    if ((Get-RunValue) -eq $command) { Add-Step 'autostart' 'unchanged' "HKCU Run\$RunValue already set." }
    elseif ($PSCmdlet.ShouldProcess("$RunKey\$RunValue", "Set to $command")) {
        New-Item -Path $RunKey -Force -ErrorAction SilentlyContinue | Out-Null
        Set-ItemProperty -Path $RunKey -Name $RunValue -Value $command
        Add-Step 'autostart' 'done' "HKCU Run\$RunValue = $command"
    }
    else { Add-Step 'autostart' 'planned' "Set HKCU Run\$RunValue = $command" }

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

    # Admin mode is machine policy; never retain or migrate tray.json adminMode.
    $settings = [ordered]@{ startAtLogin = $true }
    if ($PSCmdlet.ShouldProcess($TraySettingsPath, 'Write startAtLogin=true')) {
        New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
        [IO.File]::WriteAllText($TraySettingsPath, ($settings | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
        Add-Step 'tray-settings' 'done' 'startAtLogin=true'
    }
    else { Add-Step 'tray-settings' 'planned' "Write $TraySettingsPath (startAtLogin=true)." }
}

function Start-Tray {
    if ($NoLaunch) { Add-Step 'launch' 'skipped' '-NoLaunch given.'; return }
    if (-not $IsElevated) {
        if (-not $PSCmdlet.ShouldProcess($TrayExe, 'Start tray')) { Add-Step 'launch' 'planned' "Start $TrayExe."; return }
        Start-Process -FilePath $TrayExe -WorkingDirectory $InstallDir
    }
    else {
        # Run elevated: start the tray NON-elevated through a one-shot Limited task.
        if (-not $PSCmdlet.ShouldProcess($TrayExe, 'Launch tray non-elevated via one-shot Limited task')) {
            Add-Step 'launch' 'planned' "Start $TrayExe as $UserName (RunLevel Limited) through a temporary task, then delete the task."
            return
        }
        $action = New-ScheduledTaskAction -Execute $TrayExe -WorkingDirectory $InstallDir
        $principal = New-ScheduledTaskPrincipal -UserId $UserName -LogonType Interactive -RunLevel Limited
        $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -Priority 4
        Register-ScheduledTask -TaskPath $TaskPath -TaskName $LaunchTaskName -Action $action -Principal $principal -Settings $settings -Force | Out-Null
        try { Start-ScheduledTask -TaskPath $TaskPath -TaskName $LaunchTaskName }
        finally {
            Start-Sleep -Seconds 2
            # Deleting the task definition does not stop the process it started.
            Unregister-ScheduledTask -TaskPath $TaskPath -TaskName $LaunchTaskName -Confirm:$false -ErrorAction SilentlyContinue
        }
    }
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    do { Start-Sleep -Milliseconds 500 } until ((Get-Process bubbleplay_tray -ErrorAction SilentlyContinue) -or [DateTime]::UtcNow -gt $deadline)
    $tray = Get-Process bubbleplay_tray -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $tray) { throw 'The tray did not start. Start it from the Start menu (BubblePlay) or sign out and back in.' }
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
        if (-not (Test-Task $AgentTaskName)) { $work.Add("register on-demand task $TaskPath$AgentTaskName (user $UserName, RunLevel Highest, user may only query/run/end it)") }
        if (-not (Test-SecureTaskCurrent)) { $work.Add("register/repair on-demand task $TaskPath$HelperTaskName (SYSTEM, --launch; protected DACL: this user read+execute, SYSTEM/Administrators full; verified)") }
        if (-not (Test-SystemHelperTaskCurrent $AdminModeOffTaskName '--set-admin-mode off')) { $work.Add("register/repair on-demand task $TaskPath$AdminModeOffTaskName (SYSTEM, --set-admin-mode off; protected DACL: this user read+execute, SYSTEM/Administrators full; verified)") }
        if (-not (Test-AdminModeKeyCurrent)) { $work.Add("create/repair $AdminModeKey (protected DACL: SYSTEM/Administrators full, Users read only; initialise AdminMode DWORD=0; no tray.json migration)") }
    }
    if (Test-Task $LegacyTaskName) { $work.Add("remove the old elevated-host task $TaskPath$LegacyTaskName") }
    if (Test-Path -LiteralPath $LegacyElevatedDir) { $work.Add("remove the old elevated host copy $LegacyElevatedDir") }
    if (-not $AgentOnly) {
        if ($ConfigNeedsMachine) { $work.Add("migrate $ServiceDataDir\transport.json into $HostConfigPath (CurrentUser DPAPI)") }
        if (-not $KeepServices) {
            $services = @($ServiceNames | Where-Object { Get-Service -Name $_ -ErrorAction SilentlyContinue })
            if ($services.Count) { $work.Add("stop and delete services $($services -join ', ') (ProgramData is kept)") }
            $binaries = @()
            try { $binaries = @($ServiceBinaries | Where-Object { Test-Path -LiteralPath (Join-Path $MachineDir $_) -PathType Leaf }) } catch { }
            if ($binaries.Count) { $work.Add("delete old service binaries in $MachineDir that match installed-hashes.json ($($binaries -join ', '))") }
        }
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
    $roots = @($AgentDir, $LegacyElevatedDir)
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    do {
        $busy = @(Get-Process bubbleplay_admin_agent, bubbleplay_secure_helper, bubbleplay_tray, bubbleplay-host -ErrorAction SilentlyContinue |
                Where-Object { $p = $_.Path; $p -and @($roots | Where-Object { $p.StartsWith("$($_.TrimEnd('\'))\", [StringComparison]::OrdinalIgnoreCase) }).Count })
        if ($busy.Count -eq 0) { return }
        $busy | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 300
    } while ([DateTime]::UtcNow -lt $deadline)
}

function Set-ProtectedDirAcl([string]$Path) {
    $acl = New-Object Security.AccessControl.DirectorySecurity
    $acl.SetOwner([Security.Principal.SecurityIdentifier]'S-1-5-32-544')
    $acl.SetAccessRuleProtection($true, $false)
    $inherit = [Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
    $none = [Security.AccessControl.PropagationFlags]::None
    foreach ($entry in @(@('S-1-5-18', 'FullControl'), @('S-1-5-32-544', 'FullControl'), @('S-1-5-32-545', 'ReadAndExecute'))) {
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
                [Security.Principal.SecurityIdentifier]$entry[0], [Security.AccessControl.FileSystemRights]$entry[1],
                $inherit, $none, [Security.AccessControl.AccessControlType]::Allow))
    }
    Set-Acl -LiteralPath $Path -AclObject $acl
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
            Remove-Item -Recurse -Force -LiteralPath $AgentDir
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
        Set-ProtectedDirAcl $dir
    }
    foreach ($path in @($AgentExe, $HelperExe, "$AgentExe.new", "$HelperExe.new", (Join-Path $AgentDir 'logs'))) {
        if ((Test-Path -LiteralPath $path) -and ((Get-Item -LiteralPath $path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw "$path is a reparse point." }
    }
    foreach ($name in $PrivilegedBinaries) {
        $target = Join-Path $AgentDir $name
        if ((Get-Sha256OrNull $target) -ne $hashes[$name]) {
            $temp = "$target.new"
            Copy-Item -Force -LiteralPath (Join-Path $SourceDir $name) -Destination $temp
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
            -not (Test-SamePath $task.Actions[0].Execute $HelperExe) -or $task.Actions[0].Arguments -ne $Arguments -or $task.Triggers) { return $false }
        $scheduler = New-Object -ComObject Schedule.Service
        $scheduler.Connect()
        return Test-SecureTaskDacl ($scheduler.GetFolder($TaskPath.TrimEnd('\')).GetTask($Name).GetSecurityDescriptor(4))
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
    if (-not $PSCmdlet.ShouldProcess($AdminModeKey, 'Create/repair protected policy key and initialise AdminMode DWORD=0 (no tray.json migration)')) {
        Add-Step 'admin-mode-policy' 'planned' "Create/repair $AdminModeKey; protected DACL SYSTEM/Administrators full, Users read only; AdminMode DWORD=0."
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
    $base = Open-AdminModeRegistry
    try {
        $key = $base.CreateSubKey($AdminModeRegistryPath, [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree,
            [Microsoft.Win32.RegistryOptions]::None, $acl)
        try {
            $key.SetAccessControl($acl)
            $key.SetValue('AdminMode', 0, [Microsoft.Win32.RegistryValueKind]::DWord)
        }
        finally { $key.Dispose() }
    }
    finally { $base.Dispose() }
    if (-not (Test-AdminModeKeyCurrent)) { throw 'Admin mode registry type/owner/protected DACL verification failed.' }
    Add-Step 'admin-mode-policy' 'done' "$AdminModeKey initialised OFF; protected DACL verified."
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
    $task.SetSecurityDescriptor("D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;FRFX;;;$UserSid)", 0)
    Add-Step 'agent-task' 'done' "Registered $fullName for $UserName."
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
                try { Remove-Item -Recurse -Force -LiteralPath $LegacyElevatedDir; break }
                catch { if ([DateTime]::UtcNow -gt $deadline) { throw }; Start-Sleep -Milliseconds 500 }
            }
            Add-Step 'legacy-dir' 'done' "Deleted $LegacyElevatedDir."
        }
        else { Add-Step 'legacy-dir' 'planned' "Delete $LegacyElevatedDir." }
    }
    $stale = Join-Path $DataDir 'elevated-runner.json'
    if ((Test-Path -LiteralPath $stale) -and $PSCmdlet.ShouldProcess($stale, 'Delete stale heartbeat')) { Remove-Item -Force -LiteralPath $stale }
}

function Remove-BubblePlayServices {
    if ($KeepServices) { Add-Step 'services' 'skipped' '-KeepServices given.'; return }
    $present = @($ServiceNames | Where-Object { Get-Service -Name $_ -ErrorAction SilentlyContinue })
    if ($present.Count -eq 0) {
        Add-Step 'services' 'unchanged' 'No BubblePlay services registered.'
    }
    elseif ($PSCmdlet.ShouldProcess(($present -join ', '), 'Stop and delete service registrations')) {
        $sc = Join-Path $env:SystemRoot 'System32\sc.exe'
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

    $leftovers = @(Get-Process -Name $ServiceProcessNames -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -and $_.Path.StartsWith($MachineDir, [StringComparison]::OrdinalIgnoreCase) })
    if ($leftovers.Count -gt 0) {
        if ($PSCmdlet.ShouldProcess(($leftovers | ForEach-Object { "$($_.Name):$($_.Id)" }) -join ', ', 'Stop leftover service processes')) {
            $leftovers | Stop-Process -Force
            Add-Step 'service-processes' 'done' "Stopped $($leftovers.Count) leftover process(es)."
        }
        else { Add-Step 'service-processes' 'planned' "Stop $($leftovers.Count) leftover process(es) under $MachineDir." }
    }

    Remove-ServiceBinaryLeftovers

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

function Remove-ServiceBinaryLeftovers {
    try {
        $present = @($ServiceBinaries | Where-Object { Test-Path -LiteralPath (Join-Path $MachineDir $_) -PathType Leaf })
    }
    catch {
        if ($IsElevated) { throw }
        Add-Step 'service-binaries' 'planned' "$MachineDir is not readable without elevation; known service binaries there will be deleted only if they match installed-hashes.json."
        return
    }
    if ($present.Count -eq 0) { Add-Step 'service-binaries' 'unchanged' "No old service binaries in $MachineDir."; return }
    $manifestPath = Join-Path $ServiceDataDir 'installed-hashes.json'
    $manifest = @{}
    try {
        foreach ($entry in @(Get-Content -Raw -LiteralPath $manifestPath | ConvertFrom-Json)) { $manifest[[string]$entry.file] = [string]$entry.sha256 }
    }
    catch {
        $state = if ($IsElevated) { 'kept' } else { 'planned' }
        Add-Step 'service-binaries' $state "Cannot read $manifestPath; old binaries are only deleted when they match it ($($present -join ', '))."
        return
    }
    $matching = @(); $kept = @()
    foreach ($name in $present) {
        $path = Join-Path $MachineDir $name
        if ($manifest.ContainsKey($name) -and $manifest[$name] -eq (Get-Sha256 $path)) { $matching += $name } else { $kept += $name }
    }
    $keptNote = if ($kept.Count) { " Kept (hash differs from manifest): $($kept -join ', ')." } else { '' }
    if ($matching.Count -eq 0) { Add-Step 'service-binaries' 'kept' "No binaries match the install manifest.$keptNote"; return }
    if ($PSCmdlet.ShouldProcess(($matching -join ', '), "Delete old service binaries from $MachineDir")) {
        foreach ($name in $matching) { Remove-Item -Force -LiteralPath (Join-Path $MachineDir $name) }
        Add-Step 'service-binaries' 'done' "Deleted $($matching -join ', ').$keptNote"
    }
    else { Add-Step 'service-binaries' 'planned' "Delete $($matching -join ', ') (hash-matched).$keptNote" }
}

# The elevated part, run inline (already elevated) or in the UAC child.
function Invoke-MachineSteps([bool]$ConfigNeedsMachine) {
    if ($ConfigNeedsMachine) { [void](Update-HostConfig) }
    Stop-AgentProcesses
    Install-Agent
    Register-AgentTask
    Register-SecureTask
    Install-AdminModeKey
    Register-AdminModeOffTask
    Remove-LegacyElevated
    if (-not $AgentOnly) { Remove-BubblePlayServices }
}

# One UAC prompt: re-run this script elevated for the machine part only.
function Invoke-ElevatedMachinePhase([Collections.Generic.List[string]]$Work) {
    $summary = "one UAC prompt runs setup-tray.ps1 -MachinePhase to: " + ($Work -join '; ')
    if (-not $PSCmdlet.ShouldProcess('setup-tray.ps1 -MachinePhase', 'Elevate once (UAC)')) {
        Add-Step 'elevate' 'planned' $summary
        return
    }
    $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-MachinePhase',
        '-SourceDir', "`"$($SourceDir.TrimEnd('\'))`"", '-InstallDir', "`"$($InstallDir.TrimEnd('\'))`"",
        '-ServiceDataDir', "`"$($ServiceDataDir.TrimEnd('\'))`"")
    if ($AgentOnly) { $arguments += '-AgentOnly' }
    if ($SkipAgent) { $arguments += '-SkipAgent' }
    if ($KeepServices) { $arguments += '-KeepServices' }
    Add-Step 'elevate' 'running' $summary
    try { $child = Start-Process -FilePath $PowerShellExe -ArgumentList $arguments -Verb RunAs -Wait -PassThru -WindowStyle Hidden }
    catch { throw "The administrator prompt was declined or failed: $($_.Exception.Message)" }
    if ($child.ExitCode -ne 0) { throw "The elevated part failed with exit code $($child.ExitCode); see setup-tray-machine-*.json in $DataDir\receipts." }
    Add-Step 'elevate' 'done' 'Elevated part finished (receipt setup-tray-machine-*.json).'
}

function Write-Receipt {
    $receipt.finishedUtc = [DateTimeOffset]::UtcNow.ToString('o')
    $json = $receipt | ConvertTo-Json -Depth 6
    if ($IsWhatIf) {
        Write-Host '--- plan receipt (not written under -WhatIf) ---'
        Write-Output $json
        return
    }
    $dir = Join-Path $DataDir 'receipts'
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $kind = if ($MachinePhase) { 'setup-tray-machine' } else { 'setup-tray' }
    $path = Join-Path $dir ("{0}-{1}.json" -f $kind, [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ'))
    [IO.File]::WriteAllText($path, $json, [Text.UTF8Encoding]::new($false))
    Write-Host "Receipt: $path"
}

try {
    if ($MachinePhase) {
        if (-not $IsElevated -and -not $IsWhatIf) {
            $refused = $true
            throw '-MachinePhase must run elevated (setup-tray.ps1 starts it through one UAC prompt).'
        }
        Assert-InteractiveUserMatches
        $configNeedsMachine = (-not $AgentOnly) -and -not (Test-Path -LiteralPath $HostConfigPath -PathType Leaf)
        Invoke-MachineSteps $configNeedsMachine
    }
    elseif ($AgentOnly) {
        Assert-InteractiveUserMatches
        $work = Get-MachineWork $false
        if ($work.Count -eq 0) { Add-Step 'agent' 'unchanged' 'Admin agent, secure helper, their tasks and the old elevated cleanup are already current.' }
        elseif ($IsElevated) { Invoke-MachineSteps $false }
        else { Invoke-ElevatedMachinePhase $work }
    }
    else {
        if ($IsElevated) { Add-Step 'elevation' 'warning' 'Run elevated: machine steps run inline; the tray is started non-elevated.' }
        Assert-InteractiveUserMatches
        $configState = Update-HostConfig
        $configNeedsMachine = $configState -eq 'machine'
        $work = Get-MachineWork $configNeedsMachine
        Stop-RunningTray
        # User files first: a missing build fails before anything machine-wide changes.
        Install-UserFiles
        if ($work.Count -eq 0) { Add-Step 'machine' 'unchanged' 'Nothing needs administrator rights (no UAC prompt).' }
        elseif ($IsElevated) { Invoke-MachineSteps $configNeedsMachine }
        else { Invoke-ElevatedMachinePhase $work }
        if ($configNeedsMachine -and -not $IsWhatIf -and -not (Test-Path -LiteralPath $HostConfigPath -PathType Leaf)) {
            throw "The service config could not be migrated to $HostConfigPath; pass -WorkerUrl, -Token and -HostGateToken."
        }
        Set-UserRegistration
        Start-Tray
    }
    $receipt.result = if ($IsWhatIf) { 'planned' } else { 'success' }
}
catch {
    $receipt.result = 'failed'
    $receipt.error = $_.Exception.Message
    throw
}
finally {
    # A refused run changes nothing, not even the receipt folder.
    if (-not $refused) { Write-Receipt }
}
