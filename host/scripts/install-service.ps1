[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'High')]
param(
    [ValidateSet('Install', 'Upgrade', 'Repair', 'Status', 'Uninstall')]
    [string]$Action = 'Install',

    [string]$SourceDir,
    [string]$InstallDir = (Join-Path $env:ProgramFiles 'BubblePlay'),
    [string]$DataDir = (Join-Path $env:ProgramData 'BubblePlay'),

    [string]$WorkerUrl,
    [Security.SecureString]$Token,
    [switch]$StartServices,
    [switch]$ResetLabOwner,
    [switch]$ConfirmRemoval
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Security

$BrokerName = 'BubblePlayBroker'
$TransportName = 'BubblePlayTransport'
$LabName = 'BubblePlayLabHelper'
$TransportServiceSid = 'S-1-5-80-36460983-23004156-1113575813-106319344-2112134846'
$LabConfigName = 'lab-service.json'
if (-not $SourceDir) {
    $SourceDir = Join-Path $PSScriptRoot '..\target\release'
}
$RequiredBinaries = @(
    'bubbleplay_broker.exe',
    'bubbleplay_transport.exe',
    'bubbleplay_agent.exe',
    'bubbleplay_input_proxy.exe',
    'bubbleplay_lab_helper.exe',
    'bubbleplay-host.exe'
)

function Test-IsAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Assert-Administrator {
    if (-not (Test-IsAdministrator)) {
        throw 'Run this script from an elevated PowerShell window.'
    }
}

function Test-ServiceExists([string]$Name) {
    return $null -ne (Get-Service -Name $Name -ErrorAction SilentlyContinue)
}

function Assert-NoReparsePath([string]$Path) {
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            $item = Get-Item -Force -LiteralPath $cursor
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Refusing reparse-point path in privileged operation: $cursor"
            }
        }
        $parent = [IO.Path]::GetDirectoryName($cursor)
        if (-not $parent -or $parent -eq $cursor) { break }
        $cursor = $parent
    }
}

function Assert-SafeLayout {
    foreach ($path in @($SourceDir, $InstallDir, $DataDir)) {
        Assert-NoReparsePath $path
        $root = [IO.Path]::GetPathRoot($path)
        if ($path.TrimEnd('\') -eq $root.TrimEnd('\')) {
            throw "Refusing to use a filesystem root as a BubblePlay directory: $path"
        }
    }
    $programFilesRoot = [IO.Path]::GetFullPath($env:ProgramFiles).TrimEnd('\') + '\'
    $programDataRoot = [IO.Path]::GetFullPath($env:ProgramData).TrimEnd('\') + '\'
    if (-not $InstallDir.StartsWith($programFilesRoot, [StringComparison]::OrdinalIgnoreCase)) {
        throw "InstallDir must remain below Program Files: $programFilesRoot"
    }
    if (-not $DataDir.StartsWith($programDataRoot, [StringComparison]::OrdinalIgnoreCase)) {
        throw "DataDir must remain below ProgramData: $programDataRoot"
    }
    $installPrefix = $InstallDir.TrimEnd('\') + '\'
    $dataPrefix = $DataDir.TrimEnd('\') + '\'
    if ($InstallDir.StartsWith($dataPrefix, [StringComparison]::OrdinalIgnoreCase) -or
        $DataDir.StartsWith($installPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'InstallDir and DataDir must not contain one another.'
    }
    $sourcePrefix = $SourceDir.TrimEnd('\') + '\'
    if ($SourceDir.Equals($InstallDir, [StringComparison]::OrdinalIgnoreCase) -or
        $SourceDir.Equals($DataDir, [StringComparison]::OrdinalIgnoreCase) -or
        $SourceDir.StartsWith($installPrefix, [StringComparison]::OrdinalIgnoreCase) -or
        $SourceDir.StartsWith($dataPrefix, [StringComparison]::OrdinalIgnoreCase) -or
        $InstallDir.StartsWith($sourcePrefix, [StringComparison]::OrdinalIgnoreCase) -or
        $DataDir.StartsWith($sourcePrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'SourceDir must be separate from InstallDir and DataDir.'
    }
}

function Assert-TransportServiceSid {
    $sc = Join-Path $env:SystemRoot 'System32\sc.exe'
    $output = & $sc showsid $TransportName 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw 'Unable to resolve the BubblePlayTransport service SID.'
    }
    $match = [regex]::Match(($output -join ' '), 'S-1-5-80-(?:\d+-){4}\d+')
    if (-not $match.Success) {
        throw 'sc.exe did not return a BubblePlayTransport service SID.'
    }
    $actual = $match.Value
    if ($actual -ne $TransportServiceSid) {
        throw "Unexpected BubblePlayTransport service SID: $actual"
    }
}

function New-DirectoryRule(
    [string]$Sid,
    [Security.AccessControl.FileSystemRights]$Rights
) {
    return [Security.AccessControl.FileSystemAccessRule]::new(
        [Security.Principal.SecurityIdentifier]::new($Sid),
        $Rights,
        ([Security.AccessControl.InheritanceFlags]::ContainerInherit -bor [Security.AccessControl.InheritanceFlags]::ObjectInherit),
        [Security.AccessControl.PropagationFlags]::None,
        [Security.AccessControl.AccessControlType]::Allow
    )
}

function Assert-ExactAcl(
    [string]$Path,
    [hashtable]$Expected
) {
    $acl = Get-Acl -LiteralPath $Path
    if (-not $acl.AreAccessRulesProtected) {
        throw "ACL inheritance remains enabled: $Path"
    }
    $rules = @($acl.GetAccessRules($true, $false, [Security.Principal.SecurityIdentifier]))
    if ($rules.Count -ne $Expected.Count) {
        throw "Unexpected explicit ACE count on '$Path': expected $($Expected.Count), got $($rules.Count)"
    }
    foreach ($sid in $Expected.Keys) {
        $matching = @($rules | Where-Object { $_.IdentityReference.Value -eq $sid })
        $normalized = [Security.AccessControl.FileSystemAccessRule]::new(
            [Security.Principal.SecurityIdentifier]::new([string]$sid),
            [Security.AccessControl.FileSystemRights]$Expected[$sid],
            [Security.AccessControl.AccessControlType]::Allow
        ).FileSystemRights
        if ($matching.Count -ne 1 -or
            $matching[0].AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow -or
            [int64]$matching[0].FileSystemRights -ne [int64]$normalized) {
            throw "ACL on '$Path' does not contain the exact expected allow ACE for $sid"
        }
    }
}

function Set-ExactDirectoryAcl(
    [string]$Path,
    [Security.AccessControl.FileSystemRights]$TransportRights,
    [string]$ReaderSid = '',
    [Security.AccessControl.FileSystemRights]$ReaderRights = [Security.AccessControl.FileSystemRights]0
) {
    Assert-NoReparsePath $Path
    $acl = [Security.AccessControl.DirectorySecurity]::new()
    $acl.SetAccessRuleProtection($true, $false)
    [void]$acl.AddAccessRule((New-DirectoryRule 'S-1-5-18' ([Security.AccessControl.FileSystemRights]::FullControl)))
    [void]$acl.AddAccessRule((New-DirectoryRule 'S-1-5-32-544' ([Security.AccessControl.FileSystemRights]::FullControl)))
    if ($TransportRights -ne [Security.AccessControl.FileSystemRights]0) {
        [void]$acl.AddAccessRule((New-DirectoryRule $TransportServiceSid $TransportRights))
    }
    if ($ReaderSid -and $ReaderRights -ne [Security.AccessControl.FileSystemRights]0) {
        [void]$acl.AddAccessRule((New-DirectoryRule $ReaderSid $ReaderRights))
    }
    Set-Acl -LiteralPath $Path -AclObject $acl
    $expected = @{
        'S-1-5-18' = [Security.AccessControl.FileSystemRights]::FullControl
        'S-1-5-32-544' = [Security.AccessControl.FileSystemRights]::FullControl
    }
    if ($TransportRights -ne [Security.AccessControl.FileSystemRights]0) {
        $expected[$TransportServiceSid] = $TransportRights
    }
    if ($ReaderSid -and $ReaderRights -ne [Security.AccessControl.FileSystemRights]0) {
        $expected[$ReaderSid] = $ReaderRights
    }
    Assert-ExactAcl $Path $expected
}

function Set-ExactFileAcl(
    [string]$Path,
    [Security.AccessControl.FileSystemRights]$TransportRights,
    [string]$ReaderSid = '',
    [Security.AccessControl.FileSystemRights]$ReaderRights = [Security.AccessControl.FileSystemRights]0
) {
    Assert-NoReparsePath $Path
    $acl = [Security.AccessControl.FileSecurity]::new()
    $acl.SetAccessRuleProtection($true, $false)
    $entries = @(
        @('S-1-5-18', [Security.AccessControl.FileSystemRights]::FullControl),
        @('S-1-5-32-544', [Security.AccessControl.FileSystemRights]::FullControl)
    )
    if ($TransportRights -ne [Security.AccessControl.FileSystemRights]0) {
        $entries += ,@($TransportServiceSid, $TransportRights)
    }
    if ($ReaderSid -and $ReaderRights -ne [Security.AccessControl.FileSystemRights]0) {
        $entries += ,@($ReaderSid, $ReaderRights)
    }
    foreach ($entry in $entries) {
        $rule = [Security.AccessControl.FileSystemAccessRule]::new(
            [Security.Principal.SecurityIdentifier]::new([string]$entry[0]),
            [Security.AccessControl.FileSystemRights]$entry[1],
            [Security.AccessControl.AccessControlType]::Allow
        )
        [void]$acl.AddAccessRule($rule)
    }
    Set-Acl -LiteralPath $Path -AclObject $acl
    $expected = @{
        'S-1-5-18' = [Security.AccessControl.FileSystemRights]::FullControl
        'S-1-5-32-544' = [Security.AccessControl.FileSystemRights]::FullControl
    }
    if ($TransportRights -ne [Security.AccessControl.FileSystemRights]0) {
        $expected[$TransportServiceSid] = $TransportRights
    }
    if ($ReaderSid -and $ReaderRights -ne [Security.AccessControl.FileSystemRights]0) {
        $expected[$ReaderSid] = $ReaderRights
    }
    Assert-ExactAcl $Path $expected
}

function Write-ProtectedUtf8File(
    [string]$Path,
    [string]$Content,
    [Security.AccessControl.FileSystemRights]$TransportRights
) {
    Assert-NoReparsePath $Path
    $directory = [IO.Path]::GetDirectoryName($Path)
    Assert-NoReparsePath $directory
    $temporary = Join-Path $directory ('.bubbleplay-' + [Guid]::NewGuid().ToString('N') + '.tmp')
    try {
        [IO.File]::WriteAllText($temporary, $Content, [Text.UTF8Encoding]::new($false))
        Set-ExactFileAcl $temporary $TransportRights
        Assert-NoReparsePath $Path
        Move-ProtectedReplacement $temporary $Path
        Set-ExactFileAcl $Path $TransportRights
    }
    finally {
        if (Test-Path -LiteralPath $temporary) {
            Remove-Item -Force -LiteralPath $temporary
        }
    }
}

function Move-ProtectedReplacement(
    [string]$Temporary,
    [string]$Destination
) {
    # Both paths are in the same already-protected directory. File.Replace is
    # atomic and exists in Windows PowerShell 5.1/.NET Framework; the two-arg
    # Move handles first install without relying on the .NET Core overwrite overload.
    if (Test-Path -LiteralPath $Destination) {
        # .NET Framework rejects a null backup path. Keep the mandatory backup
        # in the same protected directory and remove it immediately after the
        # atomic replacement succeeds.
        $directory = [IO.Path]::GetDirectoryName($Destination)
        $backup = Join-Path $directory ('.bubbleplay-' + [Guid]::NewGuid().ToString('N') + '.bak')
        try {
            [IO.File]::Replace($Temporary, $Destination, $backup, $true)
        }
        finally {
            if (Test-Path -LiteralPath $backup) {
                Remove-Item -Force -LiteralPath $backup
            }
        }
    }
    else {
        [IO.File]::Move($Temporary, $Destination)
    }
}

function Copy-ProtectedBinary(
    [string]$Source,
    [string]$Destination,
    [Security.AccessControl.FileSystemRights]$TransportRights = [Security.AccessControl.FileSystemRights]::ReadAndExecute
) {
    Assert-NoReparsePath $Source
    Assert-NoReparsePath $Destination
    $directory = [IO.Path]::GetDirectoryName($Destination)
    $temporary = Join-Path $directory ('.bubbleplay-' + [Guid]::NewGuid().ToString('N') + '.tmp')
    try {
        [IO.File]::Copy($Source, $temporary, $false)
        Set-ExactFileAcl $temporary $TransportRights
        Assert-NoReparsePath $Destination
        Move-ProtectedReplacement $temporary $Destination
        Set-ExactFileAcl $Destination $TransportRights
    }
    finally {
        if (Test-Path -LiteralPath $temporary) {
            Remove-Item -Force -LiteralPath $temporary
        }
    }
}

function Initialize-ProtectedDirectories {
    Assert-NoReparsePath $InstallDir
    Assert-NoReparsePath $DataDir
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    New-Item -ItemType Directory -Path $DataDir -Force | Out-Null
    Assert-NoReparsePath $InstallDir
    Assert-NoReparsePath $DataDir

    Set-ExactDirectoryAcl $InstallDir ([Security.AccessControl.FileSystemRights]::ReadAndExecute)
    Set-ExactDirectoryAcl $DataDir ([Security.AccessControl.FileSystemRights]::ReadAndExecute)

    $logsDir = Join-Path $DataDir 'logs'
    Assert-NoReparsePath $logsDir
    New-Item -ItemType Directory -Path $logsDir -Force | Out-Null
    Set-ExactDirectoryAcl $logsDir ([Security.AccessControl.FileSystemRights]::ReadAndExecute)
    $systemLogs = Join-Path $logsDir 'system'
    $transportLogs = Join-Path $logsDir 'transport'
    New-Item -ItemType Directory -Path $systemLogs -Force | Out-Null
    New-Item -ItemType Directory -Path $transportLogs -Force | Out-Null
    Set-ExactDirectoryAcl $systemLogs ([Security.AccessControl.FileSystemRights]0)
    Set-ExactDirectoryAcl $transportLogs ([Security.AccessControl.FileSystemRights]::Modify)
}

function Invoke-ServiceControl([string[]]$Arguments) {
    $sc = Join-Path $env:SystemRoot 'System32\sc.exe'
    & $sc @Arguments | Out-Host
    if ($LASTEXITCODE -ne 0) {
        throw "sc.exe failed with exit code ${LASTEXITCODE}: $($Arguments -join ' ')"
    }
}

function Stop-BubblePlayService([string]$Name) {
    $service = Get-Service -Name $Name -ErrorAction SilentlyContinue
    if ($null -eq $service -or $service.Status -eq 'Stopped') {
        return
    }
    Stop-Service -Name $Name -Force
    $service.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(20))
}

function ConvertTo-DpapiHex([Security.SecureString]$Secret) {
    $pointer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($Secret)
    $plainBytes = $null
    try {
        $plain = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($pointer)
        $plainBytes = [Text.Encoding]::UTF8.GetBytes($plain)
        $protected = [Security.Cryptography.ProtectedData]::Protect(
            $plainBytes,
            $null,
            [Security.Cryptography.DataProtectionScope]::LocalMachine
        )
        return (($protected | ForEach-Object { $_.ToString('X2') }) -join '')
    }
    finally {
        if ($null -ne $plainBytes) {
            [Array]::Clear($plainBytes, 0, $plainBytes.Length)
        }
        [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($pointer)
    }
}

function Update-ProtectedTransportConfig {
    if ($null -eq $Token -and $env:BUBBLEPLAY_TOKEN) {
        $script:Token = ConvertTo-SecureString $env:BUBBLEPLAY_TOKEN -AsPlainText -Force
    }
    $configPath = Join-Path $DataDir 'transport.json'
    Assert-NoReparsePath $configPath
    $existing = $null
    if (Test-Path -LiteralPath $configPath -PathType Leaf) {
        $existing = Get-Content -Raw -LiteralPath $configPath | ConvertFrom-Json
    }

    $effectiveWorker = if ($WorkerUrl) { $WorkerUrl } elseif ($existing) { $existing.worker } else { $null }
    $effectiveToken = if ($null -ne $Token) {
        ConvertTo-DpapiHex $Token
    }
    elseif ($existing) {
        $existing.tokenDpapiHex
    }
    else {
        $null
    }
    if (-not $effectiveWorker) {
        throw 'WorkerUrl is required for the first installation.'
    }
    if (-not $effectiveToken) {
        throw 'Token (as a SecureString) is required for the first installation.'
    }

    $config = [ordered]@{
        worker = $effectiveWorker
        tokenDpapiHex = $effectiveToken
    }
    $configJson = $config | ConvertTo-Json
    Write-ProtectedUtf8File $configPath $configJson ([Security.AccessControl.FileSystemRights]::Read)
}

function Resolve-LabOwnerSid {
    $configPath = Join-Path $DataDir $LabConfigName
    if ((Test-Path -LiteralPath $configPath -PathType Leaf) -and -not $ResetLabOwner) {
        Assert-NoReparsePath $configPath
        $config = Get-Content -Raw -LiteralPath $configPath | ConvertFrom-Json
        $properties = @($config.PSObject.Properties.Name | Sort-Object)
        if (($properties -join ',') -ne 'ownerSid,schemaVersion' -or [int]$config.schemaVersion -ne 1) {
            throw "Invalid protected lab owner configuration: $configPath"
        }
        try {
            return [Security.Principal.SecurityIdentifier]::new([string]$config.ownerSid).Value
        }
        catch {
            throw "Invalid ownerSid in protected lab owner configuration: $configPath"
        }
    }
    return [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
}

function Update-ProtectedLabConfig {
    $ownerSid = Resolve-LabOwnerSid
    $configPath = Join-Path $DataDir $LabConfigName
    $config = [ordered]@{
        schemaVersion = 1
        ownerSid = $ownerSid
    }
    Write-ProtectedUtf8File $configPath ($config | ConvertTo-Json) ([Security.AccessControl.FileSystemRights]0)
    return $ownerSid
}

function Set-BubblePlayAcls([string]$LabOwnerSid) {
    Set-ExactDirectoryAcl $InstallDir ([Security.AccessControl.FileSystemRights]::ReadAndExecute)
    Set-ExactDirectoryAcl $DataDir ([Security.AccessControl.FileSystemRights]::ReadAndExecute) $LabOwnerSid ([Security.AccessControl.FileSystemRights]::ReadAndExecute)
    $logsDir = Join-Path $DataDir 'logs'
    Set-ExactDirectoryAcl $logsDir ([Security.AccessControl.FileSystemRights]::ReadAndExecute) $LabOwnerSid ([Security.AccessControl.FileSystemRights]::ReadAndExecute)
    $systemLogs = Join-Path $logsDir 'system'
    $transportLogs = Join-Path $logsDir 'transport'
    Set-ExactDirectoryAcl $systemLogs ([Security.AccessControl.FileSystemRights]0) $LabOwnerSid ([Security.AccessControl.FileSystemRights]::ReadAndExecute)
    Set-ExactDirectoryAcl $transportLogs ([Security.AccessControl.FileSystemRights]::Modify) $LabOwnerSid ([Security.AccessControl.FileSystemRights]::ReadAndExecute)
    Get-ChildItem -Force -File -LiteralPath $systemLogs | ForEach-Object {
        Set-ExactFileAcl $_.FullName ([Security.AccessControl.FileSystemRights]0) $LabOwnerSid ([Security.AccessControl.FileSystemRights]::Read)
    }
    Get-ChildItem -Force -File -LiteralPath $transportLogs | ForEach-Object {
        Set-ExactFileAcl $_.FullName ([Security.AccessControl.FileSystemRights]::Modify) $LabOwnerSid ([Security.AccessControl.FileSystemRights]::Read)
    }
}

function Copy-BubblePlayBinaries {
    foreach ($name in $RequiredBinaries) {
        $source = Join-Path $SourceDir $name
        Assert-NoReparsePath $source
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "Required release binary is missing: $source"
        }
    }

    $rollbackName = (Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + [Guid]::NewGuid().ToString('N')
    $rollbackDir = Join-Path (Join-Path $DataDir 'rollback') $rollbackName
    $hasExistingBinary = $false
    foreach ($name in $RequiredBinaries) {
        if (Test-Path -LiteralPath (Join-Path $InstallDir $name) -PathType Leaf) {
            $hasExistingBinary = $true
            break
        }
    }
    if ($hasExistingBinary) {
        Assert-NoReparsePath $rollbackDir
        New-Item -ItemType Directory -Path $rollbackDir -Force | Out-Null
        Set-ExactDirectoryAcl $rollbackDir ([Security.AccessControl.FileSystemRights]0)
    }

    $backedUp = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    if ($hasExistingBinary) {
        foreach ($name in $RequiredBinaries) {
            $target = Join-Path $InstallDir $name
            if (Test-Path -LiteralPath $target -PathType Leaf) {
                $backup = Join-Path $rollbackDir $name
                Copy-Item -LiteralPath $target -Destination $backup
                Set-ExactFileAcl $backup ([Security.AccessControl.FileSystemRights]0)
                [void]$backedUp.Add($name)
            }
        }
    }

    try {
        foreach ($name in $RequiredBinaries) {
            $target = Join-Path $InstallDir $name
            Assert-NoReparsePath $target
            $transportRights = if ($name -eq 'bubbleplay_lab_helper.exe') {
                [Security.AccessControl.FileSystemRights]0
            }
            else {
                [Security.AccessControl.FileSystemRights]::ReadAndExecute
            }
            Copy-ProtectedBinary (Join-Path $SourceDir $name) $target $transportRights
        }

        $hashes = foreach ($name in $RequiredBinaries) {
            $target = Join-Path $InstallDir $name
            [ordered]@{
                file = $name
                sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $target).Hash
            }
        }
        $hashJson = $hashes | ConvertTo-Json
        $hashPath = Join-Path $DataDir 'installed-hashes.json'
        Assert-NoReparsePath $hashPath
        Write-ProtectedUtf8File $hashPath $hashJson ([Security.AccessControl.FileSystemRights]::Read)
    }
    catch {
        $updateError = $_
        $rollbackErrors = [Collections.Generic.List[string]]::new()
        foreach ($name in $RequiredBinaries) {
            $target = Join-Path $InstallDir $name
            try {
                if ($backedUp.Contains($name)) {
                    $transportRights = if ($name -eq 'bubbleplay_lab_helper.exe') {
                        [Security.AccessControl.FileSystemRights]0
                    }
                    else {
                        [Security.AccessControl.FileSystemRights]::ReadAndExecute
                    }
                    Copy-ProtectedBinary (Join-Path $rollbackDir $name) $target $transportRights
                }
                elseif (Test-Path -LiteralPath $target -PathType Leaf) {
                    Assert-NoReparsePath $target
                    Remove-Item -Force -LiteralPath $target
                }
            }
            catch {
                $rollbackErrors.Add("$name`: $($_.Exception.Message)")
            }
        }
        if ($rollbackErrors.Count -gt 0) {
            throw "BubblePlay update failed: $($updateError.Exception.Message). Rollback also failed: $($rollbackErrors -join '; ')"
        }
        throw "BubblePlay update failed and installed binaries were restored: $($updateError.Exception.Message)"
    }
}

function Register-BubblePlayService(
    [string]$Name,
    [string]$DisplayName,
    [string]$Executable,
    [string]$Account,
    [string]$Dependency,
    [string]$StartMode
) {
    $quotedExecutable = '"' + $Executable + '"'
    if (Test-ServiceExists $Name) {
        $arguments = @('config', $Name, 'binPath=', $quotedExecutable, 'start=', $StartMode, 'obj=', $Account)
        if ($Dependency) { $arguments += @('depend=', $Dependency) }
        Invoke-ServiceControl $arguments
    }
    else {
        $arguments = @('create', $Name, 'binPath=', $quotedExecutable, 'start=', $StartMode, 'obj=', $Account, 'DisplayName=', $DisplayName)
        if ($Dependency) { $arguments += @('depend=', $Dependency) }
        Invoke-ServiceControl $arguments
    }
    Invoke-ServiceControl @('sidtype', $Name, 'unrestricted')
    Invoke-ServiceControl @('failure', $Name, 'reset=', '86400', 'actions=', 'restart/1000/restart/5000/restart/30000')
}

function Register-BubblePlayLabService {
    $executable = Join-Path $InstallDir 'bubbleplay_lab_helper.exe'
    $quotedExecutable = '"' + $executable + '"'
    if (Test-ServiceExists $LabName) {
        Invoke-ServiceControl @('config', $LabName, 'binPath=', $quotedExecutable, 'start=', 'demand', 'obj=', 'LocalSystem', 'depend=', '/')
    }
    else {
        Invoke-ServiceControl @('create', $LabName, 'binPath=', $quotedExecutable, 'start=', 'demand', 'obj=', 'LocalSystem', 'DisplayName=', 'BubblePlay Stream Lab Helper')
    }
    Invoke-ServiceControl @('sidtype', $LabName, 'none')
}

function Set-LabServiceDacl([string]$OwnerSid) {
    # Service object rights: LC = SERVICE_QUERY_STATUS, RP = SERVICE_START.
    # The owner receives no stop, change-config, delete, or WRITE_DAC rights.
    $full = 'CCDCLCSWRPWPDTLOCRSDRCWDWO'
    $expected = "D:P(A;;$full;;;SY)(A;;$full;;;BA)(A;;LCRP;;;$OwnerSid)"
    Invoke-ServiceControl @('sdset', $LabName, $expected)
    $sc = Join-Path $env:SystemRoot 'System32\sc.exe'
    $output = @(& $sc sdshow $LabName 2>&1)
    $scExitCode = $LASTEXITCODE
    $actual = [string]($output | Where-Object { $_ -match '^D:' } | Select-Object -Last 1)
    $actual = $actual.Trim() -replace '\s', ''
    if ($scExitCode -ne 0) {
        throw "BubblePlayLabHelper service DACL query failed with exit code $scExitCode."
    }
    try {
        Assert-LabServiceDaclValue $actual $OwnerSid
    }
    catch {
        throw "BubblePlayLabHelper service DACL verification failed: $($_.Exception.Message)"
    }
}

function Assert-LabServiceDaclValue([string]$Sddl, [string]$OwnerSid) {
    $descriptor = [Security.AccessControl.RawSecurityDescriptor]::new($Sddl)
    if (-not $descriptor.ControlFlags.HasFlag([Security.AccessControl.ControlFlags]::DiscretionaryAclProtected)) {
        throw 'service DACL is not protected'
    }
    $expected = @{
        'S-1-5-18' = 0xF01FF # SERVICE_ALL_ACCESS
        'S-1-5-32-544' = 0xF01FF # SERVICE_ALL_ACCESS
        $OwnerSid = 0x14 # SERVICE_QUERY_STATUS | SERVICE_START
    }
    $aces = @($descriptor.DiscretionaryAcl)
    if ($aces.Count -ne $expected.Count) {
        throw "expected exactly $($expected.Count) ACEs, got $($aces.Count)"
    }
    foreach ($sid in $expected.Keys) {
        $matching = @($aces | Where-Object { $_.SecurityIdentifier.Value -eq $sid })
        if ($matching.Count -ne 1 -or
            $matching[0].AceType -ne [Security.AccessControl.AceType]::AccessAllowed -or
            $matching[0].IsInherited -or
            [uint32]$matching[0].AccessMask -ne [uint32]$expected[$sid]) {
            throw "service DACL does not contain the exact expected ACE for $sid"
        }
    }
}

function Show-BubblePlayStatus {
    foreach ($name in @($BrokerName, $TransportName, $LabName)) {
        $service = Get-CimInstance Win32_Service -Filter "Name='$name'" -ErrorAction SilentlyContinue
        if ($null -eq $service) {
            [pscustomobject]@{ Name = $name; State = 'Not installed'; StartMode = $null; Account = $null; Path = $null }
        }
        else {
            [pscustomobject]@{ Name = $name; State = $service.State; StartMode = $service.StartMode; Account = $service.StartName; Path = $service.PathName }
        }
    }
}

function Show-LabOwnerStatus {
    $configPath = Join-Path $DataDir $LabConfigName
    try {
        if (-not (Test-Path -LiteralPath $configPath -PathType Leaf -ErrorAction Stop)) {
            return [pscustomobject]@{ LabOwnerSid = 'Missing'; ServiceDacl = 'Unknown'; ConfigPath = $configPath }
        }
        Assert-NoReparsePath $configPath
        $config = Get-Content -Raw -LiteralPath $configPath | ConvertFrom-Json
        $ownerSid = [Security.Principal.SecurityIdentifier]::new([string]$config.ownerSid).Value
        $daclState = Get-LabServiceDaclState $ownerSid
        return [pscustomobject]@{ LabOwnerSid = $ownerSid; ServiceDacl = $daclState; ConfigPath = $configPath }
    }
    catch [UnauthorizedAccessException] {
        return [pscustomobject]@{ LabOwnerSid = 'Elevation required to inspect'; ServiceDacl = 'Elevation required to inspect'; ConfigPath = $configPath }
    }
    catch {
        return [pscustomobject]@{ LabOwnerSid = 'Drift: ' + $_.Exception.Message; ServiceDacl = 'Unknown'; ConfigPath = $configPath }
    }
}

function Get-LabServiceDaclState([string]$OwnerSid) {
    if (-not (Test-ServiceExists $LabName)) { return 'Service missing' }
    try {
        $sc = Join-Path $env:SystemRoot 'System32\sc.exe'
        $output = @(& $sc sdshow $LabName 2>&1)
        $scExitCode = $LASTEXITCODE
        $actual = [string]($output | Where-Object { $_ -match '^D:' } | Select-Object -Last 1)
        $actual = $actual.Trim() -replace '\s', ''
        if ($scExitCode -ne 0) { return 'Elevation required to inspect' }
        Assert-LabServiceDaclValue $actual $OwnerSid
        return 'Protected'
    }
    catch {
        return 'Drift: ' + $_.Exception.Message
    }
}

function Get-AclState(
    [string]$Path,
    [Security.AccessControl.FileSystemRights]$TransportRights,
    [string]$ReaderSid = '',
    [Security.AccessControl.FileSystemRights]$ReaderRights = [Security.AccessControl.FileSystemRights]0
) {
    $expected = @{
        'S-1-5-18' = [Security.AccessControl.FileSystemRights]::FullControl
        'S-1-5-32-544' = [Security.AccessControl.FileSystemRights]::FullControl
    }
    if ($TransportRights -ne [Security.AccessControl.FileSystemRights]0) {
        $expected[$TransportServiceSid] = $TransportRights
    }
    if ($ReaderSid -and $ReaderRights -ne [Security.AccessControl.FileSystemRights]0) {
        $expected[$ReaderSid] = $ReaderRights
    }
    try {
        if (-not (Test-Path -LiteralPath $Path -ErrorAction Stop)) { return 'Missing' }
        Assert-NoReparsePath $Path
        Assert-ExactAcl $Path $expected
        return 'Protected'
    }
    catch [UnauthorizedAccessException] {
        return 'Elevation required to inspect'
    }
    catch {
        return 'Drift: ' + $_.Exception.Message
    }
}

function Show-BubblePlayAclStatus {
    $labOwnerSid = Resolve-LabOwnerSid
    $checks = [Collections.Generic.List[object]]@(
        @($InstallDir, [Security.AccessControl.FileSystemRights]::ReadAndExecute, '', [Security.AccessControl.FileSystemRights]0),
        @($DataDir, [Security.AccessControl.FileSystemRights]::ReadAndExecute, $labOwnerSid, [Security.AccessControl.FileSystemRights]::ReadAndExecute),
        @((Join-Path $DataDir 'logs'), [Security.AccessControl.FileSystemRights]::ReadAndExecute, $labOwnerSid, [Security.AccessControl.FileSystemRights]::ReadAndExecute),
        @((Join-Path $DataDir 'logs\system'), [Security.AccessControl.FileSystemRights]0, $labOwnerSid, [Security.AccessControl.FileSystemRights]::ReadAndExecute),
        @((Join-Path $DataDir 'logs\transport'), [Security.AccessControl.FileSystemRights]::Modify, $labOwnerSid, [Security.AccessControl.FileSystemRights]::ReadAndExecute),
        @((Join-Path $DataDir 'transport.json'), [Security.AccessControl.FileSystemRights]::Read, '', [Security.AccessControl.FileSystemRights]0),
        @((Join-Path $DataDir $LabConfigName), [Security.AccessControl.FileSystemRights]0, '', [Security.AccessControl.FileSystemRights]0),
        @((Join-Path $DataDir 'installed-hashes.json'), [Security.AccessControl.FileSystemRights]::Read, '', [Security.AccessControl.FileSystemRights]0)
    )
    foreach ($name in $RequiredBinaries) {
        $transportRights = if ($name -eq 'bubbleplay_lab_helper.exe') {
            [Security.AccessControl.FileSystemRights]0
        }
        else {
            [Security.AccessControl.FileSystemRights]::ReadAndExecute
        }
        $checks.Add(@((Join-Path $InstallDir $name), $transportRights, '', [Security.AccessControl.FileSystemRights]0))
    }
    foreach ($check in $checks) {
        [pscustomobject]@{
            Path = [string]$check[0]
            AclState = Get-AclState ([string]$check[0]) ([Security.AccessControl.FileSystemRights]$check[1]) ([string]$check[2]) ([Security.AccessControl.FileSystemRights]$check[3])
        }
    }
}

if ($Action -eq 'Status') {
    $InstallDir = [IO.Path]::GetFullPath($InstallDir)
    $DataDir = [IO.Path]::GetFullPath($DataDir)
    Show-BubblePlayStatus | Format-Table -AutoSize
    Show-LabOwnerStatus | Format-Table -AutoSize -Wrap
    Show-BubblePlayAclStatus | Format-Table -AutoSize -Wrap
    return
}

Assert-Administrator

if ($Action -eq 'Uninstall') {
    if (-not $ConfirmRemoval) {
        throw 'Uninstall requires -ConfirmRemoval. Installed binaries and configuration are retained for recovery.'
    }
    if ($PSCmdlet.ShouldProcess('BubblePlayBroker, BubblePlayTransport, and BubblePlayLabHelper', 'Stop and delete service registrations')) {
        Stop-BubblePlayService $LabName
        Stop-BubblePlayService $TransportName
        Stop-BubblePlayService $BrokerName
        if (Test-ServiceExists $LabName) { Invoke-ServiceControl @('delete', $LabName) }
        if (Test-ServiceExists $TransportName) { Invoke-ServiceControl @('delete', $TransportName) }
        if (Test-ServiceExists $BrokerName) { Invoke-ServiceControl @('delete', $BrokerName) }
        Write-Host "Service registrations removed. Recovery files remain in '$InstallDir' and '$DataDir'."
    }
    return
}

$SourceDir = [IO.Path]::GetFullPath($SourceDir)
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
$DataDir = [IO.Path]::GetFullPath($DataDir)
Assert-SafeLayout

if ($PSCmdlet.ShouldProcess($InstallDir, "$Action BubblePlay privileged services")) {
    Stop-BubblePlayService $LabName
    Stop-BubblePlayService $TransportName
    Stop-BubblePlayService $BrokerName

    # Register first so Windows materializes the service SID before it is used
    # in an ACL. Services remain stopped until every protected write completes.
    Register-BubblePlayService $BrokerName 'BubblePlay Privileged Broker' (Join-Path $InstallDir 'bubbleplay_broker.exe') 'LocalSystem' '' 'auto'
    Register-BubblePlayService $TransportName 'BubblePlay Transport' (Join-Path $InstallDir 'bubbleplay_transport.exe') 'NT AUTHORITY\LocalService' $BrokerName 'auto'
    Register-BubblePlayLabService
    Assert-TransportServiceSid

    Initialize-ProtectedDirectories
    Update-ProtectedTransportConfig
    $labOwnerSid = Update-ProtectedLabConfig
    Set-BubblePlayAcls $labOwnerSid
    Set-LabServiceDacl $labOwnerSid
    Copy-BubblePlayBinaries

    if ($StartServices) {
        Start-Service -Name $BrokerName
        Start-Service -Name $TransportName
    }
    else {
        Write-Host 'Core services installed for automatic startup and left stopped for validation. The lab helper remains demand-start and stopped.'
    }
}

Show-BubblePlayStatus | Format-Table -AutoSize
Show-LabOwnerStatus | Format-Table -AutoSize -Wrap
