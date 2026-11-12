# CI only: install the secure helper the way setup-tray.ps1 does, so
# host/tests/hung_foreground_input.rs can drive the real host -> helper path.
# Refuses to run outside GitHub Actions: it writes Program Files, HKLM and a
# SYSTEM scheduled task.
param([Parameter(Mandatory)][string]$Exe)
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true') { throw 'ci-install-secure-helper.ps1 runs on GitHub-hosted runners only.' }

$machine = Join-Path $env:ProgramW6432 'BubblePlay'
$agent = Join-Path $machine 'agent'
$logs = Join-Path $agent 'logs'

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

function Set-AdministratorsOwner([string]$Path) {
    $acl = Get-Acl -LiteralPath $Path
    $acl.SetOwner([Security.Principal.SecurityIdentifier]'S-1-5-32-544')
    Set-Acl -LiteralPath $Path -AclObject $acl
}

New-Item -ItemType Directory -Force $agent | Out-Null
Set-ProtectedDirAcl $machine
Set-ProtectedDirAcl $agent
$helper = Join-Path $agent 'bubbleplay_secure_helper.exe'
Copy-Item -LiteralPath $Exe -Destination $helper -Force
New-Item -ItemType Directory -Force $logs | Out-Null
Set-AdministratorsOwner $helper
Set-AdministratorsOwner $logs

$action = New-ScheduledTaskAction -Execute $helper -Argument '--launch' -WorkingDirectory $agent
$principal = New-ScheduledTaskPrincipal -UserId 'S-1-5-18' -LogonType ServiceAccount -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -Priority 4
Register-ScheduledTask -TaskPath '\BubblePlay\' -TaskName 'SecureDesktop' -Action $action -Principal $principal -Settings $settings -Force | Out-Null

# Admin mode bound to the runner's account (the console user on a hosted runner).
$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$key = 'HKLM:\SOFTWARE\BubblePlay'
New-Item -Path $key -Force | Out-Null
New-ItemProperty -Path $key -Name AdminModeSid -PropertyType String -Value $sid -Force | Out-Null
New-ItemProperty -Path $key -Name AdminMode -PropertyType DWord -Value 1 -Force | Out-Null

$session = (Get-Process -Id $PID).SessionId
Write-Host "secure helper installed at $helper; admin mode bound to $sid; runner session $session"
