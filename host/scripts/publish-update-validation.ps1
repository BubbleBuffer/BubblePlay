[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$ManifestPath,
    [Parameter(Mandatory = $true)]
    [ValidateSet('pass', 'fail')]
    [string]$Result,
    [string]$EvidencePath = '',
    [string]$Detail = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function ConvertTo-BubblePlayDateTimeOffset($Value) {
    if ($Value -is [DateTimeOffset]) { return [DateTimeOffset]$Value }
    if ($Value -is [DateTime]) { return [DateTimeOffset]::new([DateTime]$Value) }
    return [DateTimeOffset]::Parse(
        [string]$Value,
        [Globalization.CultureInfo]::InvariantCulture,
        [Globalization.DateTimeStyles]::RoundtripKind
    )
}

$hostDir = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$preflightRoot = [IO.Path]::GetFullPath((Join-Path $hostDir 'target-privileged\update-preflight'))
$manifest = [IO.Path]::GetFullPath($ManifestPath)
$prefix = $preflightRoot.TrimEnd('\') + '\'
if (-not $manifest.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Manifest must remain below $preflightRoot"
}
if (-not (Test-Path -LiteralPath $manifest -PathType Leaf)) {
    throw "Manifest does not exist: $manifest"
}

$document = Get-Content -Raw -LiteralPath $manifest | ConvertFrom-Json
$requestPath = [IO.Path]::GetFullPath([string]$document.validationRequestPath)
$resultPath = [IO.Path]::GetFullPath([string]$document.validationResultPath)
$expectedResultPath = [IO.Path]::GetFullPath((Join-Path (Split-Path -Parent $manifest) 'validation-result.json'))
if (-not $resultPath.Equals($expectedResultPath, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Manifest validation-result.json path is invalid.'
}
foreach ($path in @($requestPath, $resultPath)) {
    if (-not $path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Validation path must remain below $preflightRoot"
    }
}
if (-not (Test-Path -LiteralPath $requestPath -PathType Leaf)) {
    throw "The privileged updater has not requested external validation: $requestPath"
}

$request = Get-Content -Raw -LiteralPath $requestPath | ConvertFrom-Json
$manifestSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $manifest).Hash.ToUpperInvariant()
if ([string]$request.receiptId -ne [string]$document.receiptId -or
    [string]$request.validationNonce -ne [string]$document.validationNonce -or
    [string]$request.manifestSha256 -ne $manifestSha256) {
    throw 'Validation request is not bound to this manifest, receiptId, and validationNonce.'
}
if ([DateTimeOffset]::UtcNow -gt (ConvertTo-BubblePlayDateTimeOffset $request.deadlineAt)) {
    throw 'External validation request has expired.'
}

$payload = [ordered]@{
    schemaVersion = 1
    receiptId = [string]$document.receiptId
    validationNonce = [string]$document.validationNonce
    manifestSha256 = $manifestSha256
    result = $Result
    evidencePath = $EvidencePath
    detail = $Detail
    validatedAt = [DateTimeOffset]::UtcNow.ToString('O')
}
$temporary = "$resultPath.$([Guid]::NewGuid().ToString('N')).tmp"
[IO.File]::WriteAllText($temporary, ($payload | ConvertTo-Json -Depth 6), [Text.UTF8Encoding]::new($false))
Move-Item -Force -LiteralPath $temporary -Destination $resultPath
[pscustomobject]@{ Result = $Result; Path = $resultPath; ReceiptId = $payload.receiptId }
