[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[a-f0-9]{32}$')]
    [string]$SnapshotReceiptId,

    [Parameter(Mandatory = $true)]
    [string]$ProvenManifestPath,

    [ValidateRange(300, 1800)]
    [int]$RollbackTimeoutSeconds = 900
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Restoring a protected BubblePlay snapshot requires Administrator PowerShell.'
}

$hostDir = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$preflightRoot = [IO.Path]::GetFullPath((Join-Path $hostDir 'target-privileged\update-preflight'))
$releaseDir = [IO.Path]::GetFullPath((Join-Path $hostDir 'target-privileged\release'))
$receiptRoot = [IO.Path]::GetFullPath((Join-Path $hostDir 'target-privileged\update-receipts'))
$snapshotRoot = [IO.Path]::GetFullPath((Join-Path $env:ProgramData 'BubblePlay-UpdateRollback'))
$snapshotDir = [IO.Path]::GetFullPath((Join-Path $snapshotRoot $SnapshotReceiptId))
$ProvenManifestPath = [IO.Path]::GetFullPath($ProvenManifestPath)
$required = @(
    'bubbleplay_broker.exe',
    'bubbleplay_transport.exe',
    'bubbleplay_agent.exe',
    'bubbleplay_lab_helper.exe',
    'bubbleplay-host.exe'
)

if (-not (Test-Path -LiteralPath $snapshotDir -PathType Container)) {
    throw "Protected updater snapshot is missing: $snapshotDir"
}
if (-not (Test-Path -LiteralPath $ProvenManifestPath -PathType Leaf)) {
    throw "Proven manifest is missing: $ProvenManifestPath"
}

$proven = Get-Content -Raw -LiteralPath $ProvenManifestPath | ConvertFrom-Json
$provenReceipt = [IO.Path]::GetFullPath([string]$proven.receiptPath)
if (-not $provenReceipt.StartsWith(($receiptRoot.TrimEnd('\') + '\'), [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Proven receipt path escapes the BubblePlay receipt root.'
}
$provenResult = Get-Content -Raw -LiteralPath $provenReceipt | ConvertFrom-Json
if ([string]$provenResult.stage -ne 'success') {
    throw "The supplied manifest has no successful deployment receipt: $provenReceipt"
}

$verified = [Collections.Generic.List[object]]::new()
foreach ($name in $required) {
    $entries = @($proven.files | Where-Object { [string]$_.name -eq $name })
    if ($entries.Count -ne 1) { throw "Proven manifest has an invalid entry count for $name." }
    $source = Join-Path $snapshotDir $name
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "Protected snapshot is missing $name."
    }
    $item = Get-Item -LiteralPath $source
    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $source).Hash.ToUpperInvariant()
    if ([int64]$item.Length -ne [int64]$entries[0].size -or $hash -ne [string]$entries[0].sha256) {
        throw "Protected snapshot $name does not match the proven successful manifest."
    }
    $verified.Add([ordered]@{ name = $name; source = $source; size = [int64]$item.Length; sha256 = $hash })
}

New-Item -ItemType Directory -Path $releaseDir -Force | Out-Null
foreach ($entry in $verified) {
    $destination = Join-Path $releaseDir ([string]$entry.name)
    $temporary = "$destination.$([Guid]::NewGuid().ToString('N')).restore"
    Copy-Item -LiteralPath ([string]$entry.source) -Destination $temporary
    $copiedHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $temporary).Hash.ToUpperInvariant()
    if ($copiedHash -ne [string]$entry.sha256) {
        Remove-Item -Force -LiteralPath $temporary
        throw "Staged snapshot hash mismatch for $([string]$entry.name)."
    }
    Move-Item -Force -LiteralPath $temporary -Destination $destination
}

$runId = (Get-Date -Format 'yyyyMMdd-HHmmss') + '-restore-' + [Guid]::NewGuid().ToString('N')
$runDir = Join-Path $preflightRoot $runId
New-Item -ItemType Directory -Path $runDir -Force | Out-Null
$receiptId = [Guid]::NewGuid().ToString('N')
$nonceBytes = [byte[]]::new(32)
$generator = [Security.Cryptography.RandomNumberGenerator]::Create()
try { $generator.GetBytes($nonceBytes) } finally { $generator.Dispose() }
$nonce = -join ($nonceBytes | ForEach-Object { '{0:x2}' -f $_ })
$files = @($required | ForEach-Object {
    $path = Join-Path $releaseDir $_
    $item = Get-Item -LiteralPath $path
    [ordered]@{
        name = $_
        path = $item.FullName
        size = [int64]$item.Length
        sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToUpperInvariant()
    }
})
$manifestPath = Join-Path $runDir 'manifest.json'
$manifest = [ordered]@{
    schemaVersion = 1
    runId = $runId
    receiptId = $receiptId
    createdAt = [DateTimeOffset]::UtcNow.ToString('O')
    expiresAt = [DateTimeOffset]::UtcNow.AddMinutes(15).ToString('O')
    ready = $true
    sourceDir = $releaseDir
    files = $files
    checks = @(
        [ordered]@{ name = 'proven-success-receipt'; status = 'pass'; evidence = $provenReceipt },
        [ordered]@{ name = 'protected-snapshot-hash-match'; status = 'pass'; evidence = $snapshotDir }
    )
    validationNonce = $nonce
    validationRequestPath = Join-Path $runDir 'validation-request.json'
    validationResultPath = Join-Path $runDir 'validation-result.json'
    receiptPath = Join-Path $receiptRoot "$receiptId.json"
    rollbackTimeoutSeconds = $RollbackTimeoutSeconds
}
[IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))

& (Join-Path $PSScriptRoot 'update-service.ps1') -ApplyManifest $manifestPath
