#Requires -Version 5.1
<#
.SYNOPSIS
    Remove the protected Program Files install with one UAC prompt.
.DESCRIPTION
    Only the protected Program Files script runs elevated. It removes machine
    tasks, policy and program files. The unelevated parent removes HKCU entries,
    legacy per-user binaries and, with -RemoveData, LOCALAPPDATA\BubblePlay.
    Links at every depth are refused. -WhatIf changes nothing.
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param([string]$InstallDir, [switch]$RemoveData, [switch]$MachinePhase)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not [Environment]::Is64BitProcess) { throw 'Use 64-bit PowerShell for the Windows x64 BubblePlay package.' }
$SystemDirectory = [Environment]::GetFolderPath('System')
$SystemModuleRoot = [IO.Path]::Combine($PSHOME, 'Modules')
$WindowsModuleRoot = [IO.Path]::Combine($SystemDirectory, 'WindowsPowerShell\v1.0\Modules')
$IsElevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if ($IsElevated -or $MachinePhase) {
    $env:PSModulePath = $SystemModuleRoot + ';' + $WindowsModuleRoot
    $PSModuleAutoLoadingPreference = 'None'
    foreach ($name in 'Microsoft.PowerShell.Utility', 'Microsoft.PowerShell.Management', 'Microsoft.PowerShell.Security') {
        Import-Module ([IO.Path]::Combine($SystemModuleRoot, $name, "$name.psd1")) -ErrorAction Stop
    }
}
$ProgramFilesRoot = [Environment]::GetFolderPath('ProgramFiles')
$MachineDir = Join-Path $ProgramFilesRoot 'BubblePlay'
if (-not $InstallDir) { $InstallDir = $MachineDir }
$InstallDir = [IO.Path]::GetFullPath($InstallDir).TrimEnd('\')
if ($InstallDir -ine $MachineDir.TrimEnd('\')) { throw "InstallDir must be $MachineDir." }
$TaskPath = '\BubblePlay\'
$Tasks = @('AdminAgent', 'SecureDesktop', 'AdminModeOff', 'ElevatedHost', 'LaunchTray')
$AdminModeRegistryPath = 'SOFTWARE\BubblePlay'
$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$UninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\BubblePlay'
$RunValue = 'BubblePlayTray'
$LegacyUserDir = $null; $DataDir = $null
if (-not $MachinePhase -and -not $IsElevated) {
    $LegacyUserDir = Join-Path $env:LOCALAPPDATA 'Programs\BubblePlay'
    $DataDir = Join-Path $env:LOCALAPPDATA 'BubblePlay'
}
$PowerShellExe = Join-Path $SystemDirectory 'WindowsPowerShell\v1.0\powershell.exe'
$IsWhatIf = [bool]$WhatIfPreference
$IsElevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
$TrustedSids = @('S-1-5-18', 'S-1-5-32-544', 'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464')
$WriteRights = [int64]([Security.AccessControl.FileSystemRights]::WriteData -bor [Security.AccessControl.FileSystemRights]::AppendData -bor
    [Security.AccessControl.FileSystemRights]::WriteExtendedAttributes -bor [Security.AccessControl.FileSystemRights]::WriteAttributes -bor
    [Security.AccessControl.FileSystemRights]::Delete -bor [Security.AccessControl.FileSystemRights]::DeleteSubdirectoriesAndFiles -bor
    [Security.AccessControl.FileSystemRights]::ChangePermissions -bor [Security.AccessControl.FileSystemRights]::TakeOwnership) -bor 0x40000000 -bor 0x10000000
function Invoke-Schtasks([string[]]$Arguments) {
    $ErrorActionPreference = 'Continue'
    $null = & (Join-Path $SystemDirectory 'schtasks.exe') @Arguments 2>&1
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
    catch { return $true }
    finally { $base.Dispose() }
}

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


function Assert-ProtectedUninstaller {
    $expected = Join-Path $MachineDir 'uninstall-tray.ps1'
    Assert-NoReparse $expected
    if (-not (Test-Path -LiteralPath $expected -PathType Leaf)) { throw 'Protected uninstaller is missing. Run current setup to migrate/repair this installation first.' }
    foreach ($path in @($ProgramFilesRoot, $MachineDir, $expected)) {
        $findings = Get-UnsafeAclFindings $path
        if ($findings.Count) { throw "Unsafe protected uninstaller: $($findings -join '; ')" }
    }
}
function Stop-InstallProcesses([string]$Root) {
    Get-Process bubbleplay_tray, bubbleplay-host, bubbleplay_app, bubbleplay_admin_agent, bubbleplay_secure_helper -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and $_.Path.StartsWith($Root.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase) } |
        Stop-Process -Force -ErrorAction SilentlyContinue
}
function Invoke-MachineSteps {
    Assert-ProtectedUninstaller
    # Preflight the entire tree before removing tasks or policy. Each deletion
    # rechecks ancestors and children, and .NET nonrecursive deletion cannot
    # traverse a junction on Windows PowerShell 5.1.
    $null = @(Get-SafeTreeFiles $MachineDir)
    foreach ($item in Get-SafeTreeFiles $MachineDir -IncludeDirectories) {
        $findings = Get-UnsafeAclFindings $item.FullName
        if ($findings.Count) { throw "Unsafe protected uninstall tree: $($findings -join '; ')" }
    }
    foreach ($name in $Tasks) {
        if (-not (Test-Task $name)) { continue }
        if ($PSCmdlet.ShouldProcess("$TaskPath$name", 'End and unregister task')) {
            if ($name -ne 'SecureDesktop') { [void](Invoke-Schtasks @('/end', '/tn', "$TaskPath$name")) }
            $code = Invoke-Schtasks @('/delete', '/tn', "$TaskPath$name", '/f')
            if ($code -ne 0) { throw "Failed to remove $TaskPath$name ($code)." }
        }
    }
    if ((Test-AdminModeKeyExists) -and $PSCmdlet.ShouldProcess('HKLM:\SOFTWARE\BubblePlay', 'Delete protected Admin mode policy')) {
        $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, [Microsoft.Win32.RegistryView]::Registry64)
        try { $base.DeleteSubKeyTree($AdminModeRegistryPath, $false) } finally { $base.Dispose() }
    }
    if ($PSCmdlet.ShouldProcess($MachineDir, 'Stop installed processes and delete protected program tree')) {
        Stop-InstallProcesses $MachineDir
        Set-Location -LiteralPath $SystemDirectory
        $deadline = [DateTime]::UtcNow.AddSeconds(10)
        while (Test-Path -LiteralPath $MachineDir) {
            try { Remove-SafeTree $MachineDir }
            catch { if ([DateTime]::UtcNow -gt $deadline) { throw }; Start-Sleep -Milliseconds 300 }
        }
    }
}
if ($MachinePhase) {
    if (-not $IsElevated -and -not $IsWhatIf) { throw '-MachinePhase must run elevated.' }
    if ([IO.Path]::GetFullPath($PSCommandPath) -ine (Join-Path $MachineDir 'uninstall-tray.ps1')) { throw 'Machine uninstall must execute the protected Program Files script.' }
    Invoke-MachineSteps
    return
}
if ($IsElevated) { throw 'Run uninstall from a normal desktop PowerShell; only the protected machine child runs elevated.' }
# The parent keeps its parsed script in memory while the machine child removes
# Program Files. No elevated TEMP copy or delayed command is ever created.
foreach ($root in @($MachineDir, $LegacyUserDir)) {
    if ($PSCmdlet.ShouldProcess($root, 'Quit installed tray and app')) {
        $tray = Join-Path $root 'bubbleplay_tray.exe'
        Assert-NoReparse $tray
        if (Test-Path -LiteralPath $tray -PathType Leaf) { Start-Process -FilePath $tray -ArgumentList '--quit' -Wait -WindowStyle Hidden }
        Stop-InstallProcesses $root
    }
}
Set-Location -LiteralPath $SystemDirectory
$machineWork = (Test-Path -LiteralPath $MachineDir) -or (Test-AdminModeKeyExists)
foreach ($name in $Tasks) { if (Test-Task $name) { $machineWork = $true } }
if ($machineWork -and $PSCmdlet.ShouldProcess($MachineDir, 'Elevate protected uninstaller once (UAC)')) {
    Assert-ProtectedUninstaller
    $protected = Join-Path $MachineDir 'uninstall-tray.ps1'
    $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$protected`"", '-MachinePhase')
    $child = Start-Process -FilePath $PowerShellExe -ArgumentList $arguments -Verb RunAs -Wait -PassThru -WindowStyle Hidden
    if ($child.ExitCode -ne 0) { throw "Machine uninstall failed with exit code $($child.ExitCode). User configuration was retained." }
}
$props = Get-ItemProperty -Path $RunKey -ErrorAction SilentlyContinue
if ($props -and $props.PSObject.Properties[$RunValue] -and $PSCmdlet.ShouldProcess("$RunKey\$RunValue", 'Remove user autostart')) { Remove-ItemProperty -Path $RunKey -Name $RunValue }
foreach ($item in @((Join-Path ([Environment]::GetFolderPath('Programs')) 'BubblePlay.lnk'), (Join-Path ([Environment]::GetFolderPath('SendTo')) 'BubblePlay.lnk'))) {
    if ((Test-Path -LiteralPath $item) -and $PSCmdlet.ShouldProcess($item, 'Remove user shortcut')) { Remove-Item -LiteralPath $item -Force }
}
if ((Test-Path -LiteralPath $UninstallKey) -and $PSCmdlet.ShouldProcess($UninstallKey, 'Remove user uninstall registration')) { Remove-Item -LiteralPath $UninstallKey -Recurse -Force }
if ((Test-Path -LiteralPath $LegacyUserDir) -and $PSCmdlet.ShouldProcess($LegacyUserDir, 'Delete legacy per-user program files')) { Remove-SafeTree $LegacyUserDir }
if ($RemoveData -and (Test-Path -LiteralPath $DataDir) -and $PSCmdlet.ShouldProcess($DataDir, 'Delete user config, logs and receipts')) { Remove-SafeTree $DataDir }
elseif (Test-Path -LiteralPath $DataDir) { Write-Host "User data kept in $DataDir (pass -RemoveData to delete)." }
