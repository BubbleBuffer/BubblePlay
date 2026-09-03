[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[a-f0-9]{32}$')]
    [string]$ReceiptId,

    [Parameter(Mandatory = $true)]
    [string]$ReceiptPath,

    [Parameter(Mandatory = $true)]
    [string]$SourceDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'The BubblePlay update helper must be run as administrator.'
}

$hostDir = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$expectedSource = [IO.Path]::GetFullPath((Join-Path $hostDir 'target-privileged\release'))
$allowedReceiptRoot = [IO.Path]::GetFullPath((Join-Path $hostDir 'target-privileged\update-receipts'))
$SourceDir = [IO.Path]::GetFullPath($SourceDir)
$ReceiptPath = [IO.Path]::GetFullPath($ReceiptPath)
$receiptPrefix = $allowedReceiptRoot.TrimEnd('\') + '\'

if (-not $SourceDir.Equals($expectedSource, [StringComparison]::OrdinalIgnoreCase)) {
    throw "SourceDir must be the verified privileged release directory: $expectedSource"
}
if (-not $ReceiptPath.StartsWith($receiptPrefix, [StringComparison]::OrdinalIgnoreCase) -or
    [IO.Path]::GetFileName($ReceiptPath) -ne "$ReceiptId.json") {
    throw "ReceiptPath must be the nonce-bound receipt beneath $allowedReceiptRoot"
}

$installer = Join-Path $PSScriptRoot 'install-service.ps1'
if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
    throw "BubblePlay installer is missing: $installer"
}

function Write-UpdateReceipt([string]$Stage, [string]$Detail) {
    $payload = [ordered]@{
        receiptId = $ReceiptId
        stage = $Stage
        detail = $Detail
        updatedAt = [DateTimeOffset]::UtcNow.ToString('O')
    }
    [IO.File]::WriteAllText(
        $ReceiptPath,
        ($payload | ConvertTo-Json -Compress),
        [Text.UTF8Encoding]::new($false)
    )
}

Write-UpdateReceipt 'started' ''
try {
    & $installer -Action Upgrade -SourceDir $SourceDir -StartServices -Confirm:$false
    Write-UpdateReceipt 'success' ''
}
catch {
    Write-UpdateReceipt 'failed' $_.Exception.Message
    throw
}
