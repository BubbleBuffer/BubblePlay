[CmdletBinding()]
param(
    [string]$SourceBinary = (Join-Path $PSScriptRoot '..\target-candidate\release\bubbleplay_lab_helper.exe'),
    [string]$InstallDir = (Join-Path $env:ProgramFiles 'BubblePlay\LabHelper'),
    [string]$DataDir = (Join-Path $env:ProgramData 'BubblePlay'),
    [string]$ProbeScript = (Join-Path $PSScriptRoot '..\..\worker\tools\lab\stream-lab-helper-probe.mjs'),
    [string]$ReceiptPath = (Join-Path $PSScriptRoot '..\target-candidate\lab-helper-repair-receipt.json'),
    [string]$OwnerSid = ([Security.Principal.WindowsIdentity]::GetCurrent().User.Value)
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$serviceName = 'BubblePlayLabHelper'
$executableName = 'bubbleplay_lab_helper.exe'
$configPath = Join-Path $DataDir 'lab-service.json'

function Assert-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Lab Helper repair requires one elevated launch.'
    }
}

function Assert-NoReparsePath([string]$Path) {
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            $item = Get-Item -Force -LiteralPath $cursor
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Refusing reparse-point path: $cursor"
            }
        }
        $parent = [IO.Path]::GetDirectoryName($cursor)
        if (-not $parent -or $parent -eq $cursor) { break }
        $cursor = $parent
    }
}

function Invoke-Checked([string]$File, [string[]]$Arguments) {
    & $File @Arguments | Out-Host
    if ($LASTEXITCODE -ne 0) {
        throw "$File failed with exit code $LASTEXITCODE"
    }
}

Assert-Administrator
$SourceBinary = [IO.Path]::GetFullPath($SourceBinary)
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
$DataDir = [IO.Path]::GetFullPath($DataDir)
$ProbeScript = [IO.Path]::GetFullPath($ProbeScript)
$ReceiptPath = [IO.Path]::GetFullPath($ReceiptPath)

$programFilesPrefix = [IO.Path]::GetFullPath($env:ProgramFiles).TrimEnd('\') + '\'
$programDataPrefix = [IO.Path]::GetFullPath($env:ProgramData).TrimEnd('\') + '\'
if (-not $InstallDir.StartsWith($programFilesPrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'InstallDir must remain below Program Files.'
}
if (-not $DataDir.StartsWith($programDataPrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'DataDir must remain below ProgramData.'
}
foreach ($path in @($SourceBinary, $InstallDir, $DataDir, $ProbeScript, $ReceiptPath)) {
    Assert-NoReparsePath $path
}
if (-not (Test-Path -LiteralPath $SourceBinary -PathType Leaf)) { throw "Missing helper binary: $SourceBinary" }
if (-not (Test-Path -LiteralPath $ProbeScript -PathType Leaf)) { throw "Missing helper probe: $ProbeScript" }
if ([IO.Path]::GetFileName($SourceBinary) -ne $executableName) { throw "Source binary must be $executableName" }
[void][Security.Principal.SecurityIdentifier]::new($OwnerSid)

$sourceHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $SourceBinary).Hash.ToLowerInvariant()
New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
New-Item -ItemType Directory -Path $DataDir -Force | Out-Null
$destination = Join-Path $InstallDir $executableName
$temporary = Join-Path $InstallDir ('.helper-' + [Guid]::NewGuid().ToString('N') + '.tmp')
$backup = Join-Path $InstallDir ('.helper-' + [Guid]::NewGuid().ToString('N') + '.bak')
try {
    [IO.File]::Copy($SourceBinary, $temporary, $false)
    $copiedHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $temporary).Hash.ToLowerInvariant()
    if ($copiedHash -ne $sourceHash) { throw 'Helper binary hash changed during staging.' }
    if (Test-Path -LiteralPath $destination) {
        [IO.File]::Replace($temporary, $destination, $backup, $true)
    }
    else {
        [IO.File]::Move($temporary, $destination)
    }
}
finally {
    foreach ($path in @($temporary, $backup)) {
        if (Test-Path -LiteralPath $path) { Remove-Item -Force -LiteralPath $path }
    }
}

$icacls = Join-Path $env:SystemRoot 'System32\icacls.exe'
Invoke-Checked $icacls @($InstallDir, '/inheritance:r', '/grant:r', '*S-1-5-18:(OI)(CI)F', '*S-1-5-32-544:(OI)(CI)F')
Invoke-Checked $icacls @($destination, '/inheritance:r', '/grant:r', '*S-1-5-18:F', '*S-1-5-32-544:F')

if (-not (Test-Path -LiteralPath $configPath -PathType Leaf)) {
    $config = [ordered]@{ schemaVersion = 1; ownerSid = $OwnerSid } | ConvertTo-Json
    [IO.File]::WriteAllText($configPath, $config, [Text.UTF8Encoding]::new($false))
    Invoke-Checked $icacls @($configPath, '/inheritance:r', '/grant:r', '*S-1-5-18:F', '*S-1-5-32-544:F')
}

$sc = Join-Path $env:SystemRoot 'System32\sc.exe'
$quotedDestination = '"' + $destination + '"'
$query = & $sc query $serviceName 2>&1
if ($LASTEXITCODE -eq 0) {
    if (($query -join "`n") -notmatch 'STATE\s*:\s*1\s+STOPPED') {
        Invoke-Checked $sc @('stop', $serviceName)
        $stopDeadline = [DateTimeOffset]::UtcNow.AddSeconds(10)
        do {
            Start-Sleep -Milliseconds 200
            $stopState = @(& $sc query $serviceName 2>&1)
            $isStopped = $LASTEXITCODE -eq 0 -and ($stopState -join "`n") -match 'STATE\s*:\s*1\s+STOPPED'
        } while (-not $isStopped -and [DateTimeOffset]::UtcNow -lt $stopDeadline)
        if (-not $isStopped) { throw 'Existing helper did not stop before repair.' }
    }
    Invoke-Checked $sc @('config', $serviceName, 'binPath=', $quotedDestination, 'start=', 'demand', 'obj=', 'LocalSystem', 'depend=', '/')
}
else {
    Invoke-Checked $sc @('create', $serviceName, 'binPath=', $quotedDestination, 'start=', 'demand', 'obj=', 'LocalSystem', 'DisplayName=', 'BubblePlay Stream Lab Helper')
}
Invoke-Checked $sc @('sidtype', $serviceName, 'none')
$full = 'CCDCLCSWRPWPDTLOCRSDRCWDWO'
$serviceDacl = "D:P(A;;$full;;;SY)(A;;$full;;;BA)(A;;LCRP;;;$OwnerSid)"
Invoke-Checked $sc @('sdset', $serviceName, $serviceDacl)

Invoke-Checked $sc @('start', $serviceName)
$node = (Get-Command node.exe -ErrorAction Stop).Source
& $node $ProbeScript
if ($LASTEXITCODE -ne 0) { throw "stream-lab-helper-probe.mjs failed with exit code $LASTEXITCODE" }
$probePassed = $true

$deadline = [DateTimeOffset]::UtcNow.AddSeconds(10)
do {
    $stateOutput = @(& $sc query $serviceName 2>&1)
    if ($LASTEXITCODE -ne 0) { throw 'Unable to query repaired helper service.' }
    $stopped = ($stateOutput -join "`n") -match 'STATE\s*:\s*1\s+STOPPED'
    if ($stopped) { break }
    Start-Sleep -Milliseconds 200
} while ([DateTimeOffset]::UtcNow -lt $deadline)
if (-not $stopped) { throw 'Repaired helper did not stop after its bounded probe.' }

$receiptDir = [IO.Path]::GetDirectoryName($ReceiptPath)
New-Item -ItemType Directory -Path $receiptDir -Force | Out-Null
$receipt = [ordered]@{
    schemaVersion = 1
    repairedAt = [DateTimeOffset]::UtcNow.ToString('O')
    service = $serviceName
    binary = $destination
    sha256 = $sourceHash
    ownerSid = $OwnerSid
    probePassed = $probePassed
    serviceStopped = $stopped
} | ConvertTo-Json
[IO.File]::WriteAllText($ReceiptPath, $receipt, [Text.UTF8Encoding]::new($false))
Write-Host "BubblePlay Lab Helper repaired and self-tested; receipt: $ReceiptPath"
