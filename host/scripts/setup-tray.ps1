#Requires -Version 5.1
<#
.SYNOPSIS
    One-time elevated migration from the BubblePlay Windows services to the
    user-space tray app (bubbleplay_tray.exe).

.DESCRIPTION
    Run once from an elevated PowerShell window signed in as the desktop user
    that BubblePlay should control. Steps (each idempotent):
      1. Migrate worker URL + tokens from %ProgramData%\BubblePlay\transport.json
         (machine DPAPI, service-only ACL) into %LOCALAPPDATA%\BubblePlay\host.json
         with the secrets re-protected by CurrentUser DPAPI.
      2. Quit a running tray / elevated host so binaries can be replaced.
      3. Copy bubbleplay-host.exe and bubbleplay_tray.exe to
         %LOCALAPPDATA%\Programs\BubblePlay (user-writable: later updates via
         update-tray.ps1 need no elevation).
      4. Copy the same two binaries to %ProgramFiles%\BubblePlay\elevated
         (protected ACL: SYSTEM/Administrators full, Users read+execute; verified)
         and register the on-demand task \BubblePlay\ElevatedHost (this user,
         RunLevel Highest, no triggers) that runs THAT admin-owned copy with
         --run-elevated-host. The elevated runner only starts the host beside
         itself, never from %LOCALAPPDATA%.
      5. Stop and delete BubblePlayLabHelper, BubblePlayTransport, BubblePlayBroker,
         stop leftover service-side processes, remove leftover Stream Lab QoS
         policies, and delete old service binaries in %ProgramFiles%\BubblePlay
         only when their SHA-256 matches %ProgramData%\BubblePlay\installed-hashes.json.
         ProgramData is left untouched.
      6. Set the HKCU Run entry and tray.json startAtLogin=true.
      7. Launch the tray NON-elevated through a one-shot Limited task.
    A receipt JSON is written to %LOCALAPPDATA%\BubblePlay\receipts.
    Use -WhatIf (works without elevation) to print the plan without changing anything.

    -ElevatedBinariesOnly refreshes just step 4 (used by update-tray.ps1
    -IncludeElevated). -SkipElevatedTask opts out of Admin mode entirely.
    Admin mode lets input reach elevated windows (UIPI); the secure desktop
    (UAC prompts, lock screen) is still NOT captured.
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
    [switch]$SkipElevatedTask,
    [switch]$ElevatedBinariesOnly,
    [switch]$NoLaunch
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Security
# Load modules up front with WhatIf off so module import noise (alias creation)
# does not show up as fake plan items; queries below pass -WhatIf:$false too.
& {
    $WhatIfPreference = $false
    foreach ($module in 'CimCmdlets', 'ScheduledTasks', 'NetQos') { Import-Module $module -ErrorAction SilentlyContinue }
}

$ServiceInstallDir = Join-Path $env:ProgramFiles 'BubblePlay'
$ElevatedDir = Join-Path $ServiceInstallDir 'elevated'
$ElevatedTrayExe = Join-Path $ElevatedDir 'bubbleplay_tray.exe'
# Binaries install-service.ps1 placed in $ServiceInstallDir.
$ServiceBinaries = @('bubbleplay_broker.exe', 'bubbleplay_transport.exe', 'bubbleplay_agent.exe',
    'bubbleplay_input_proxy.exe', 'bubbleplay_lab_helper.exe', 'bubbleplay-host.exe')
$TrustedSids = @('S-1-5-18', 'S-1-5-32-544', 'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464')
$ServiceNames = @('BubblePlayLabHelper', 'BubblePlayTransport', 'BubblePlayBroker')
$ServiceProcessNames = @('bubbleplay_broker', 'bubbleplay_transport', 'bubbleplay_agent',
    'bubbleplay_input_proxy', 'bubbleplay_lab_helper', 'bubbleplay_lab_activity_guard', 'bubbleplay-host')
$Binaries = @('bubbleplay-host.exe', 'bubbleplay_tray.exe')
$TaskPath = '\BubblePlay\'
$ElevatedTaskName = 'ElevatedHost'
$LaunchTaskName = 'LaunchTray'
$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$RunValue = 'BubblePlayTray'

if (-not $SourceDir) { $SourceDir = Join-Path $PSScriptRoot '..\target\release' }
$SourceDir = [IO.Path]::GetFullPath($SourceDir)
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
$DataDir = Join-Path $env:LOCALAPPDATA 'BubblePlay'
$HostConfigPath = Join-Path $DataDir 'host.json'
$TraySettingsPath = Join-Path $DataDir 'tray.json'
$TrayExe = Join-Path $InstallDir 'bubbleplay_tray.exe'
$IsWhatIf = [bool]$WhatIfPreference

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$IsElevated = ([Security.Principal.WindowsPrincipal]$identity).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
$UserSid = $identity.User.Value
$UserName = $identity.Name

$refused = $false
$receipt = [ordered]@{
    schema      = 'bubbleplay.setup-tray.v1'
    startedUtc  = [DateTimeOffset]::UtcNow.ToString('o')
    finishedUtc = $null
    whatIf      = $IsWhatIf
    elevated    = $IsElevated
    user        = $UserName
    userSid     = $UserSid
    sourceDir   = $SourceDir
    installDir  = $InstallDir
    elevatedDir = $ElevatedDir
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
    $output = & $exe @Arguments 2>&1
    return [pscustomobject]@{ ExitCode = $LASTEXITCODE; Output = ($output | Out-String).Trim() }
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
        throw "This elevated session runs as $UserName ($UserSid) but the desktop belongs to $($owners -join ', '). " +
        'Elevate from the desktop user''s own account (UAC consent, not a different admin login) so CurrentUser DPAPI, HKCU and %LOCALAPPDATA% belong to that user.'
    }
    Add-Step 'identity' 'ok' "$UserName owns the interactive desktop."
}

# --- Step 1: configuration ------------------------------------------------------
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

function Update-HostConfig {
    $existing = Get-ExistingHostConfig
    $service = Get-ServiceConfig
    $explicitToken = ConvertFrom-Secure $Token
    $explicitGate = ConvertFrom-Secure $HostGateToken

    $source = $null; $worker = $null; $tok = $null; $gate = $null
    if ($WorkerUrl -or $explicitToken) {
        $source = 'parameters'
        $worker = if ($WorkerUrl) { $WorkerUrl } elseif ($existing) { $existing.Worker } elseif ($service -and $service.Readable) { $service.Worker } else { $null }
        $tok = if ($explicitToken) { $explicitToken } elseif ($existing) { $existing.Token } elseif ($service -and $service.Readable) { $service.Token } else { $null }
        $gate = if ($explicitGate) { $explicitGate } elseif ($existing) { $existing.HostGateToken } elseif ($service -and $service.Readable) { $service.HostGateToken } else { $null }
    }
    elseif ($service -and $service.Readable) {
        $source = "service config $($service.Path)"
        $worker = $service.Worker; $tok = $service.Token; $gate = $service.HostGateToken
    }
    elseif ($existing) {
        $source = 'existing host.json'
        $worker = $existing.Worker; $tok = $existing.Token; $gate = $existing.HostGateToken
    }
    elseif ($service -and -not $service.Readable) {
        if ($IsWhatIf) {
            Add-Step 'config' 'planned' "Would migrate $($service.Path) (not readable without elevation) -> $HostConfigPath (CurrentUser DPAPI)."
            return
        }
        throw "Cannot read $($service.Path): $($service.Error)"
    }
    else {
        if ($IsWhatIf) {
            Add-Step 'config' 'blocked' 'No service config or host.json found; the real run needs -WorkerUrl and -Token.'
            return
        }
        throw 'No existing configuration found. Pass -WorkerUrl wss://.../host-ws and -Token (SecureString).'
    }
    if (-not $worker -or $worker -notmatch '^wss?://') { throw "Worker URL must be ws:// or wss:// (got '$worker')." }
    if (-not $tok) { throw 'No host registration token available.' }
    $monitorValue = if ($PSBoundParameters.ContainsKey('Monitor') -or $Monitor) { $Monitor } elseif ($existing) { $existing.Monitor } else { $null }

    if ($existing -and $existing.Worker -eq $worker -and $existing.Token -eq $tok -and
        $existing.HostGateToken -eq $gate -and $existing.Monitor -eq $monitorValue) {
        Add-Step 'config' 'unchanged' "$HostConfigPath already matches ($source)."
        return
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
        if (-not $check -or $check.Token -ne $tok) { throw 'host.json round-trip verification failed.' }
        Add-Step 'config' 'done' "Wrote $HostConfigPath from $source (worker $worker, gate token: $([bool]$gate))."
    }
    else {
        Add-Step 'config' 'planned' "Write $HostConfigPath from $source (worker $worker, gate token: $([bool]$gate))."
    }
}

# --- Step 2: stop the running tray ---------------------------------------------
function Stop-RunningTray {
    $running = @(Get-Process bubbleplay_tray -ErrorAction SilentlyContinue)
    if ($running.Count -eq 0) { Add-Step 'stop-tray' 'skipped' 'Tray is not running.'; return }
    if (-not $PSCmdlet.ShouldProcess('bubbleplay_tray', 'Quit running tray and elevated host')) {
        Add-Step 'stop-tray' 'planned' "Quit $($running.Count) running tray process(es)."
        return
    }
    $quitter = if (Test-Path -LiteralPath $TrayExe) { $TrayExe } else { $running[0].Path }
    if ($quitter) { Start-Process -FilePath $quitter -ArgumentList '--quit' -Wait -WindowStyle Hidden }
    [void](Invoke-Schtasks @('/end', '/tn', "$TaskPath$ElevatedTaskName"))
    Start-Sleep -Milliseconds 500
    Get-Process bubbleplay_tray -ErrorAction SilentlyContinue | Stop-Process -Force
    Get-Process bubbleplay-host -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and ($_.Path.StartsWith($InstallDir, [StringComparison]::OrdinalIgnoreCase) -or
                $_.Path.StartsWith($ElevatedDir, [StringComparison]::OrdinalIgnoreCase)) } |
        Stop-Process -Force
    Add-Step 'stop-tray' 'done' 'Tray and its host stopped.'
}

# --- Step 3: remove services ----------------------------------------------------
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

    $serviceRoot = Join-Path $env:ProgramFiles 'BubblePlay'
    $leftovers = @(Get-Process -Name $ServiceProcessNames -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -and $_.Path.StartsWith($serviceRoot, [StringComparison]::OrdinalIgnoreCase) })
    if ($leftovers.Count -gt 0) {
        if ($PSCmdlet.ShouldProcess(($leftovers | ForEach-Object { "$($_.Name):$($_.Id)" }) -join ', ', 'Stop leftover service processes')) {
            $leftovers | Stop-Process -Force
            Add-Step 'service-processes' 'done' "Stopped $($leftovers.Count) leftover process(es)."
        }
        else { Add-Step 'service-processes' 'planned' "Stop $($leftovers.Count) leftover process(es) under $serviceRoot." }
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
        $present = @($ServiceBinaries | Where-Object { Test-Path -LiteralPath (Join-Path $ServiceInstallDir $_) -PathType Leaf })
    }
    catch {
        if ($IsElevated) { throw }
        Add-Step 'service-binaries' 'planned' "$ServiceInstallDir is not readable without elevation; known service binaries there will be deleted only if they match installed-hashes.json."
        return
    }
    if ($present.Count -eq 0) { Add-Step 'service-binaries' 'unchanged' "No old service binaries in $ServiceInstallDir."; return }
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
        $path = Join-Path $ServiceInstallDir $name
        if ($manifest.ContainsKey($name) -and $manifest[$name] -eq (Get-Sha256 $path)) { $matching += $name } else { $kept += $name }
    }
    $keptNote = if ($kept.Count) { " Kept (hash differs from manifest): $($kept -join ', ')." } else { '' }
    if ($matching.Count -eq 0) { Add-Step 'service-binaries' 'kept' "No binaries match the install manifest.$keptNote"; return }
    if ($PSCmdlet.ShouldProcess(($matching -join ', '), "Delete old service binaries from $ServiceInstallDir")) {
        foreach ($name in $matching) { Remove-Item -Force -LiteralPath (Join-Path $ServiceInstallDir $name) }
        Add-Step 'service-binaries' 'done' "Deleted $($matching -join ', ').$keptNote"
    }
    else { Add-Step 'service-binaries' 'planned' "Delete $($matching -join ', ') (hash-matched).$keptNote" }
}

# --- Step 4: binaries -----------------------------------------------------------
function Install-Binaries {
    $hashes = [ordered]@{}
    $changed = @()
    foreach ($name in $Binaries) {
        $source = Join-Path $SourceDir $name
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            if ($IsWhatIf) { Add-Step 'binaries' 'blocked' "Missing $source (build first)."; return }
            throw "Missing $source. Build first: cargo build --offline --locked --release --bins --manifest-path host/Cargo.toml"
        }
        $hash = Get-Sha256 $source
        $hashes[$name] = $hash
        $target = Join-Path $InstallDir $name
        if (-not (Test-Path -LiteralPath $target) -or (Get-Sha256 $target) -ne $hash) {
            $changed += $name
        }
    }
    $receipt.binaries = $hashes
    if ($changed.Count -eq 0) { Add-Step 'binaries' 'unchanged' "$InstallDir is current."; return }
    if ($PSCmdlet.ShouldProcess($InstallDir, "Copy $($changed -join ', ')")) {
        New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
        foreach ($name in $changed) {
            $target = Join-Path $InstallDir $name
            $temp = "$target.new"
            Copy-Item -Force -LiteralPath (Join-Path $SourceDir $name) -Destination $temp
            Move-Item -Force -LiteralPath $temp -Destination $target
            if ((Get-Sha256 $target) -ne $hashes[$name]) { throw "Hash mismatch after copying $name." }
        }
        Add-Step 'binaries' 'done' "Copied $($changed -join ', ') to $InstallDir."
    }
    else { Add-Step 'binaries' 'planned' "Copy $($changed -join ', ') from $SourceDir to $InstallDir." }
}

# --- Step 4a: admin-owned elevated copy -----------------------------------------
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

function Assert-ElevatedDirSafe {
    $findings = @()
    foreach ($path in @($ElevatedDir) + @($Binaries | ForEach-Object { Join-Path $ElevatedDir $_ })) {
        if (Test-Path -LiteralPath $path) { $findings += Get-UnsafeAclFindings $path }
    }
    if ($findings.Count -gt 0) { throw "Elevated binary folder is not admin-only: $($findings -join '; ')" }
}

function Set-ElevatedDirAcl {
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
    Set-Acl -LiteralPath $ElevatedDir -AclObject $acl
}

function Install-ElevatedBinaries {
    if ($SkipElevatedTask) { Add-Step 'elevated-binaries' 'skipped' '-SkipElevatedTask given.'; return }
    $changed = @()
    foreach ($name in $Binaries) {
        $source = Join-Path $SourceDir $name
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            if ($IsWhatIf) { Add-Step 'elevated-binaries' 'blocked' "Missing $source (build first)."; return }
            throw "Missing $source."
        }
        $target = Join-Path $ElevatedDir $name
        $current = $null
        try { if (Test-Path -LiteralPath $target) { $current = Get-Sha256 $target } } catch { }
        if ($current -ne (Get-Sha256 $source)) { $changed += $name }
    }
    if (-not $PSCmdlet.ShouldProcess($ElevatedDir, "Install admin-only copy of $($Binaries -join ', ')")) {
        $what = if ($changed.Count) { "Copy $($changed -join ', ')" } else { 'Binaries current' }
        Add-Step 'elevated-binaries' 'planned' "$what -> $ElevatedDir; set protected ACL (SYSTEM/Administrators full, Users read+execute) and verify no user write access."
        return
    }
    foreach ($dir in @($ServiceInstallDir, $ElevatedDir)) {
        if (-not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
        if ((Get-Item -LiteralPath $dir -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "$dir is a reparse point." }
    }
    Set-ElevatedDirAcl
    foreach ($name in $changed) {
        $target = Join-Path $ElevatedDir $name
        $temp = "$target.new"
        Copy-Item -Force -LiteralPath (Join-Path $SourceDir $name) -Destination $temp
        Move-Item -Force -LiteralPath $temp -Destination $target
    }
    # Reset file ACLs to the (protected) folder's inherited entries, then verify.
    foreach ($name in $Binaries) {
        $target = Join-Path $ElevatedDir $name
        $fileAcl = Get-Acl -LiteralPath $target
        $fileAcl.SetOwner([Security.Principal.SecurityIdentifier]'S-1-5-32-544')
        $fileAcl.SetAccessRuleProtection($false, $false)
        foreach ($rule in @($fileAcl.GetAccessRules($true, $false, [Security.Principal.SecurityIdentifier]))) { [void]$fileAcl.RemoveAccessRule($rule) }
        Set-Acl -LiteralPath $target -AclObject $fileAcl
        if ((Get-Sha256 $target) -ne (Get-Sha256 (Join-Path $SourceDir $name))) { throw "Hash mismatch after copying $name." }
    }
    Assert-ElevatedDirSafe
    $detail = if ($changed.Count) { "Copied $($changed -join ', ')" } else { 'Binaries current' }
    Add-Step 'elevated-binaries' 'done' "$detail in $ElevatedDir; ACL verified admin-only."
}

# --- Step 5: elevated task ------------------------------------------------------
function Register-ElevatedTask {
    $fullName = "$TaskPath$ElevatedTaskName"
    if ($SkipElevatedTask) {
        $exists = (Invoke-Schtasks @('/query', '/tn', $fullName)).ExitCode -eq 0
        if ($exists -and $PSCmdlet.ShouldProcess($fullName, 'Unregister (because -SkipElevatedTask)')) {
            Unregister-ScheduledTask -TaskPath $TaskPath -TaskName $ElevatedTaskName -Confirm:$false
            Add-Step 'elevated-task' 'done' "Removed $fullName (-SkipElevatedTask)."
        }
        else { Add-Step 'elevated-task' 'skipped' '-SkipElevatedTask given; Admin mode will be unavailable.' }
        return
    }
    if (-not $PSCmdlet.ShouldProcess($fullName, "Register on-demand RunLevel Highest task for $UserName")) {
        Add-Step 'elevated-task' 'planned' "Register $fullName -> `"$ElevatedTrayExe`" --run-elevated-host (user $UserName, Highest, on demand, grant user read+run)."
        return
    }
    if (-not (Test-Path -LiteralPath $ElevatedTrayExe)) { throw "Missing $ElevatedTrayExe." }
    Assert-ElevatedDirSafe
    $action = New-ScheduledTaskAction -Execute $ElevatedTrayExe -Argument '--run-elevated-host' -WorkingDirectory $ElevatedDir
    $principal = New-ScheduledTaskPrincipal -UserId $UserName -LogonType Interactive -RunLevel Highest
    # Priority 4 = normal; the Task Scheduler default (7) would starve the encoder.
    $settings = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -ExecutionTimeLimit ([TimeSpan]::Zero) `
        -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -Priority 4
    Register-ScheduledTask -TaskPath $TaskPath -TaskName $ElevatedTaskName -Action $action -Principal $principal `
        -Settings $settings -Description 'BubblePlay host with administrator rights (started by the tray in Admin mode).' -Force | Out-Null
    # Let the (non-elevated) user query, run and end it; only admins may change it.
    $scheduler = New-Object -ComObject Schedule.Service
    $scheduler.Connect()
    $task = $scheduler.GetFolder($TaskPath.TrimEnd('\')).GetTask($ElevatedTaskName)
    $task.SetSecurityDescriptor("D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;FRFX;;;$UserSid)", 0)
    Add-Step 'elevated-task' 'done' "Registered $fullName for $UserName."
}

# --- Step 6: autostart ------------------------------------------------------------
function Set-Autostart {
    $command = "`"$TrayExe`""
    $current = Get-RunValue
    $settings = [ordered]@{ adminMode = $false; startAtLogin = $true }
    if (Test-Path -LiteralPath $TraySettingsPath) {
        try {
            $old = Get-Content -Raw -LiteralPath $TraySettingsPath | ConvertFrom-Json
            if ($old.PSObject.Properties['adminMode']) { $settings.adminMode = [bool]$old.adminMode }
        }
        catch { }
    }
    if ($SkipElevatedTask) { $settings.adminMode = $false }
    if ($current -eq $command) { Add-Step 'autostart' 'unchanged' "HKCU Run\$RunValue already set." }
    elseif ($PSCmdlet.ShouldProcess("$RunKey\$RunValue", "Set to $command")) {
        New-Item -Path $RunKey -Force -ErrorAction SilentlyContinue | Out-Null
        Set-ItemProperty -Path $RunKey -Name $RunValue -Value $command
        Add-Step 'autostart' 'done' "HKCU Run\$RunValue = $command"
    }
    else { Add-Step 'autostart' 'planned' "Set HKCU Run\$RunValue = $command" }
    if ($PSCmdlet.ShouldProcess($TraySettingsPath, 'Write startAtLogin=true')) {
        New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
        [IO.File]::WriteAllText($TraySettingsPath, ($settings | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
        Add-Step 'tray-settings' 'done' "adminMode=$($settings.adminMode), startAtLogin=true"
    }
    else { Add-Step 'tray-settings' 'planned' "Write $TraySettingsPath (adminMode=$($settings.adminMode), startAtLogin=true)." }
}

# --- Step 7: launch non-elevated -------------------------------------------------
function Start-TrayUnelevated {
    if ($NoLaunch) { Add-Step 'launch' 'skipped' '-NoLaunch given.'; return }
    if (-not $PSCmdlet.ShouldProcess($TrayExe, 'Launch tray non-elevated via one-shot Limited task')) {
        Add-Step 'launch' 'planned' "Start $TrayExe as $UserName (RunLevel Limited) through a temporary task, then delete the task."
        return
    }
    $action = New-ScheduledTaskAction -Execute $TrayExe -WorkingDirectory $InstallDir
    $principal = New-ScheduledTaskPrincipal -UserId $UserName -LogonType Interactive -RunLevel Limited
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -Priority 4
    Register-ScheduledTask -TaskPath $TaskPath -TaskName $LaunchTaskName -Action $action -Principal $principal -Settings $settings -Force | Out-Null
    try {
        Start-ScheduledTask -TaskPath $TaskPath -TaskName $LaunchTaskName
        $deadline = [DateTime]::UtcNow.AddSeconds(15)
        do { Start-Sleep -Milliseconds 500 } until ((Get-Process bubbleplay_tray -ErrorAction SilentlyContinue) -or [DateTime]::UtcNow -gt $deadline)
    }
    finally {
        # Deleting the task definition does not stop the process it started.
        Unregister-ScheduledTask -TaskPath $TaskPath -TaskName $LaunchTaskName -Confirm:$false -ErrorAction SilentlyContinue
    }
    $tray = Get-Process bubbleplay_tray -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $tray) { throw 'The tray did not start. Start it from the Start menu Run entry or sign out and back in.' }
    Add-Step 'launch' 'done' "Tray running (pid $($tray.Id))."
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
    $path = Join-Path $dir ("setup-tray-{0}.json" -f [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ'))
    [IO.File]::WriteAllText($path, $json, [Text.UTF8Encoding]::new($false))
    Write-Host "Receipt: $path"
}

try {
    if (-not $IsElevated -and -not $IsWhatIf) {
        $refused = $true
        throw 'setup-tray.ps1 makes machine-level changes (services, scheduled task). Run it from an elevated PowerShell, or add -WhatIf to preview the plan.'
    }
    if (-not $IsElevated) { Add-Step 'elevation' 'warning' 'Not elevated: preview only; the real run must be elevated.' }
    Assert-InteractiveUserMatches
    if ($ElevatedBinariesOnly) {
        Stop-RunningTray
        Install-ElevatedBinaries
        Register-ElevatedTask
    }
    else {
        Update-HostConfig
        Stop-RunningTray
        # Binaries and task first: a missing build must fail before the services go away.
        Install-Binaries
        Install-ElevatedBinaries
        Register-ElevatedTask
        Remove-BubblePlayServices
        Set-Autostart
        Start-TrayUnelevated
    }
    $receipt.result = if ($IsWhatIf) { 'planned' } else { 'success' }
}
catch {
    $receipt.result = 'failed'
    $receipt.error = $_.Exception.Message
    throw
}
finally {
    # A refused non-elevated run changes nothing, not even the receipt folder.
    if (-not $refused) { Write-Receipt }
}
