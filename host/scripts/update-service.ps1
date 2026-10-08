[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$ResetLabOwner,
    [string]$ApplyManifest = '',
    [switch]$ValidateManifestOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$hostDir = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$sourceDir = Join-Path $hostDir 'target-privileged\release'
$installer = Join-Path $PSScriptRoot 'install-service.ps1'
$receiptDir = Join-Path $hostDir 'target-privileged\update-receipts'

function Test-BubblePlayAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Assert-BubblePlayNoReparse([string]$Path) {
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            $item = Get-Item -Force -LiteralPath $cursor
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Refusing reparse-point path in update transaction: $cursor"
            }
        }
        $parent = [IO.Path]::GetDirectoryName($cursor)
        if (-not $parent -or $parent -eq $cursor) { break }
        $cursor = $parent
    }
}

function ConvertTo-BubblePlayDateTimeOffset($Value) {
    if ($Value -is [DateTimeOffset]) { return [DateTimeOffset]$Value }
    if ($Value -is [DateTime]) { return [DateTimeOffset]::new([DateTime]$Value) }
    return [DateTimeOffset]::Parse(
        [string]$Value,
        [Globalization.CultureInfo]::InvariantCulture,
        [Globalization.DateTimeStyles]::RoundtripKind
    )
}

function Test-BubblePlayRecentHostRegistration([string]$Path, [DateTimeOffset]$NotBefore) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $false }
    foreach ($line in @(Get-Content -LiteralPath $Path -Tail 3000)) {
        if ([string]$line -notmatch 'signaling registered as host') { continue }
        $timestampMatch = [regex]::Match([string]$line, '^(?<timestamp>\S+)')
        if (-not $timestampMatch.Success) { continue }
        $timestamp = [DateTimeOffset]::MinValue
        if ([DateTimeOffset]::TryParse(
            $timestampMatch.Groups['timestamp'].Value,
            [Globalization.CultureInfo]::InvariantCulture,
            [Globalization.DateTimeStyles]::RoundtripKind,
            [ref]$timestamp
        ) -and $timestamp -ge $NotBefore) {
            return $true
        }
    }
    return $false
}

function Write-BubblePlayTransactionReceipt([string]$Path, [string]$ReceiptId, [string]$Stage, [string]$Detail, [hashtable]$Extra = @{}) {
    $payload = [ordered]@{
        schemaVersion = 2
        receiptId = $ReceiptId
        stage = $Stage
        detail = $Detail
        updatedAt = [DateTimeOffset]::UtcNow.ToString('O')
    }
    foreach ($key in $Extra.Keys) { $payload[$key] = $Extra[$key] }
    $directory = Split-Path -Parent $Path
    New-Item -ItemType Directory -Path $directory -Force | Out-Null
    $temporary = "$Path.$([Guid]::NewGuid().ToString('N')).tmp"
    [IO.File]::WriteAllText($temporary, ($payload | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
    Move-Item -Force -LiteralPath $temporary -Destination $Path
}

function Invoke-BubblePlayManifestUpdate([string]$ManifestPath, [switch]$ValidationOnly) {
    $preflightRoot = [IO.Path]::GetFullPath((Join-Path $hostDir 'target-privileged\update-preflight'))
    $preflightPrefix = $preflightRoot.TrimEnd('\') + '\'
    $manifestPath = [IO.Path]::GetFullPath($ManifestPath)
    if (-not $manifestPath.StartsWith($preflightPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Manifest must remain below $preflightRoot"
    }
    Assert-BubblePlayNoReparse $manifestPath
    if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) { throw "Manifest does not exist: $manifestPath" }

    $manifest = Get-Content -Raw -LiteralPath $manifestPath | ConvertFrom-Json
    if ([int]$manifest.schemaVersion -ne 1 -or -not [bool]$manifest.ready) { throw 'Manifest is not a ready schema-v1 update.' }
    if ([DateTimeOffset]::UtcNow -gt (ConvertTo-BubblePlayDateTimeOffset $manifest.expiresAt)) { throw 'Update manifest has expired; run preflight again.' }
    $receiptId = [string]$manifest.receiptId
    if ($receiptId -notmatch '^[a-f0-9]{32}$') { throw 'Manifest receiptId is invalid.' }
    if ([string]$manifest.validationNonce -notmatch '^[a-f0-9]{64}$') { throw 'Manifest validationNonce is invalid.' }

    $receiptPath = [IO.Path]::GetFullPath([string]$manifest.receiptPath)
    $expectedReceiptPath = [IO.Path]::GetFullPath((Join-Path $receiptDir "$receiptId.json"))
    if (-not $receiptPath.Equals($expectedReceiptPath, [StringComparison]::OrdinalIgnoreCase)) { throw 'Manifest receipt path is invalid.' }
    $validationRequestPath = [IO.Path]::GetFullPath([string]$manifest.validationRequestPath)
    $validationResultPath = [IO.Path]::GetFullPath([string]$manifest.validationResultPath)
    $expectedValidationResult = [IO.Path]::GetFullPath((Join-Path (Split-Path -Parent $manifestPath) 'validation-result.json'))
    if (-not $validationResultPath.Equals($expectedValidationResult, [StringComparison]::OrdinalIgnoreCase)) { throw 'Manifest validation-result.json path is invalid.' }
    foreach ($path in @($validationRequestPath, $validationResultPath)) {
        if (-not $path.StartsWith($preflightPrefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'Manifest validation path escapes preflight root.' }
    }

    $expectedSource = [IO.Path]::GetFullPath((Join-Path $hostDir 'target-privileged\release'))
    $manifestSource = [IO.Path]::GetFullPath([string]$manifest.sourceDir)
    if (-not $manifestSource.Equals($expectedSource, [StringComparison]::OrdinalIgnoreCase)) { throw 'Manifest sourceDir is not the fixed release directory.' }
    Assert-BubblePlayNoReparse $manifestSource
    $required = @('bubbleplay_broker.exe', 'bubbleplay_transport.exe', 'bubbleplay_agent.exe', 'bubbleplay_input_proxy.exe', 'bubbleplay_lab_helper.exe', 'bubbleplay-host.exe')
    $additiveMigrationBinaries = @('bubbleplay_input_proxy.exe')
    $manifestFiles = @($manifest.files)
    if ($manifestFiles.Count -ne $required.Count) { throw 'Manifest must contain exactly the six required binaries.' }
    foreach ($name in $required) {
        $entries = @($manifestFiles | Where-Object { [string]$_.name -eq $name })
        if ($entries.Count -ne 1) { throw "Manifest has an invalid entry count for $name." }
        $entry = $entries[0]
        $path = [IO.Path]::GetFullPath([string]$entry.path)
        $expected = [IO.Path]::GetFullPath((Join-Path $manifestSource $name))
        if (-not $path.Equals($expected, [StringComparison]::OrdinalIgnoreCase)) { throw "Manifest path is invalid for $name." }
        Assert-BubblePlayNoReparse $path
        $item = Get-Item -LiteralPath $path
        if ([int64]$entry.size -ne [int64]$item.Length) { throw "Manifest size mismatch for $name." }
        $actualHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToUpperInvariant()
        if ($actualHash -ne [string]$entry.sha256) { throw "Manifest SHA256 mismatch for $name." }
    }

    if ($ValidationOnly) {
        return [pscustomobject]@{ Valid = $true; ReceiptId = $receiptId; Manifest = $manifestPath; Files = $required.Count }
    }
    if (-not (Test-BubblePlayAdministrator)) {
        throw 'The manifest transaction must be run in the already-open Administrator PowerShell. It never launches UAC itself.'
    }

    $manifestSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $manifestPath).Hash.ToUpperInvariant()
    $startedAt = [DateTimeOffset]::UtcNow
    $mutationStarted = $false
    $rollbackSource = Join-Path (Join-Path $env:ProgramData 'BubblePlay-UpdateRollback') $receiptId
    $absentBefore = [Collections.Generic.List[string]]::new()
    $diagnosticsPath = ''
    Write-BubblePlayTransactionReceipt $receiptPath $receiptId 'validated' '' @{ manifestSha256 = $manifestSha256 }

    try {
        Assert-BubblePlayNoReparse $rollbackSource
        New-Item -ItemType Directory -Path $rollbackSource -Force | Out-Null
        $icacls = Join-Path $env:SystemRoot 'System32\icacls.exe'
        & $icacls $rollbackSource '/inheritance:r' '/grant:r' '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Unable to protect the rollback snapshot directory.' }
        foreach ($name in $required) {
            $installed = Join-Path $env:ProgramFiles "BubblePlay\$name"
            if (-not (Test-Path -LiteralPath $installed -PathType Leaf)) {
                if ($name -notin $additiveMigrationBinaries) { throw "Installed rollback source is missing: $installed" }
                $absentBefore.Add($name)
                Copy-Item -Force -LiteralPath (Join-Path $manifestSource $name) -Destination (Join-Path $rollbackSource $name)
                continue
            }
            Copy-Item -Force -LiteralPath $installed -Destination (Join-Path $rollbackSource $name)
        }
        Write-BubblePlayTransactionReceipt $receiptPath $receiptId 'snapshot-complete' '' @{
            manifestSha256 = $manifestSha256
            rollbackSource = $rollbackSource
            absentBefore = @($absentBefore)
        }

        $hostLog = Join-Path $env:ProgramData 'BubblePlay\logs\transport\transport-host.stderr.log'
        $mutationStarted = $true
        & $installer -Action Upgrade -SourceDir $manifestSource -StartServices -Confirm:$false
        foreach ($entry in $manifestFiles) {
            $installed = Join-Path $env:ProgramFiles "BubblePlay\$([string]$entry.name)"
            $actualHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $installed).Hash.ToUpperInvariant()
            if ($actualHash -ne [string]$entry.sha256) { throw "Installed SHA256 mismatch for $([string]$entry.name)." }
        }
        Write-BubblePlayTransactionReceipt $receiptPath $receiptId 'installed' '' @{ manifestSha256 = $manifestSha256 }

        foreach ($name in @('BubblePlayBroker', 'BubblePlayTransport')) {
            $service = Get-Service -Name $name -ErrorAction Stop
            if ($service.Status -ne 'Running') { throw "$name is not running after update." }
        }
        $helper = Get-Service -Name 'BubblePlayLabHelper' -ErrorAction Stop
        $probe = Join-Path (Join-Path $hostDir '..\worker\tools\lab') 'stream-lab-helper-probe.mjs'
        $probeOutput = @(& node.exe $probe 2>&1)
        if ($LASTEXITCODE -ne 0) { throw "BubblePlayLabHelper no-VDD probe failed: $($probeOutput -join ' ')" }
        $helper.WaitForStatus([System.ServiceProcess.ServiceControllerStatus]::Stopped, [TimeSpan]::FromSeconds(15))

        $registrationDeadline = [DateTimeOffset]::UtcNow.AddSeconds(60)
        $registered = $false
        do {
            if (Test-Path -LiteralPath $hostLog -PathType Leaf) {
                $item = Get-Item -LiteralPath $hostLog
                if ($item.LastWriteTimeUtc -ge $startedAt.UtcDateTime.AddSeconds(-2)) {
                    $registered = Test-BubblePlayRecentHostRegistration $hostLog ($startedAt.AddSeconds(-2))
                }
            }
            if (-not $registered) { Start-Sleep -Milliseconds 500 }
        } while (-not $registered -and [DateTimeOffset]::UtcNow -lt $registrationDeadline)
        if (-not $registered) { throw 'Updated host did not register with signaling within 60 seconds.' }

        $collector = & (Join-Path $PSScriptRoot 'collect-diagnostics.ps1') -TailLines 500
        $diagnosticsPath = [string]$collector.Summary
        if ($collector.AccessDenied -ne 0 -or $collector.CapturedLogs -ne 7) {
            throw "Post-update diagnostics captured $($collector.CapturedLogs)/7 logs with $($collector.AccessDenied) access denied."
        }
        Write-BubblePlayTransactionReceipt $receiptPath $receiptId 'internal-validation-passed' '' @{
            manifestSha256 = $manifestSha256
            helperProbe = ($probeOutput -join ' ')
            diagnosticsPath = $diagnosticsPath
        }

        $deadline = [DateTimeOffset]::UtcNow.AddSeconds([int]$manifest.rollbackTimeoutSeconds)
        $request = [ordered]@{
            schemaVersion = 1
            receiptId = $receiptId
            validationNonce = [string]$manifest.validationNonce
            manifestSha256 = $manifestSha256
            requestedAt = [DateTimeOffset]::UtcNow.ToString('O')
            deadlineAt = $deadline.ToString('O')
        }
        [IO.File]::WriteAllText($validationRequestPath, ($request | ConvertTo-Json -Depth 6), [Text.UTF8Encoding]::new($false))
        Write-BubblePlayTransactionReceipt $receiptPath $receiptId 'awaiting-external-validation' '' @{ manifestSha256 = $manifestSha256; deadlineAt = $deadline.ToString('O'); diagnosticsPath = $diagnosticsPath }
        Write-Host "Internal validation passed. Waiting for nonce-bound external validation until $($deadline.ToString('O'))..."

        $validated = $false
        do {
            if (Test-Path -LiteralPath $validationResultPath -PathType Leaf) {
                try {
                    $result = Get-Content -Raw -LiteralPath $validationResultPath | ConvertFrom-Json
                    if ([string]$result.receiptId -eq $receiptId -and
                        [string]$result.validationNonce -eq [string]$manifest.validationNonce -and
                        [string]$result.manifestSha256 -eq $manifestSha256) {
                        if ([string]$result.result -eq 'fail') { throw "External validation failed: $([string]$result.detail)" }
                        if ([string]$result.result -eq 'pass') { $validated = $true; break }
                    }
                }
                catch {
                    if ($_.Exception.Message -like 'External validation failed:*') { throw }
                }
            }
            Start-Sleep -Milliseconds 500
        } while ([DateTimeOffset]::UtcNow -lt $deadline)
        if (-not $validated) { throw 'External validation timed out; automatic rollback is required.' }

        $labQosPolicies = @(Get-NetQosPolicy -PolicyStore ActiveStore -ErrorAction Stop |
            Where-Object { $_.Name -like 'BubblePlay-StreamLab-*' -or $_.Name -like 'BubblePlayLab-*' } |
            Select-Object -ExpandProperty Name)
        if ($labQosPolicies.Count -ne 0) { throw "External validation left Stream Lab QoS policies active: $($labQosPolicies -join ', ')" }
        $helperAfterValidation = Get-Service -Name 'BubblePlayLabHelper' -ErrorAction Stop
        if ($helperAfterValidation.Status -ne 'Stopped') { throw 'BubblePlayLabHelper remained active after external validation.' }

        Write-BubblePlayTransactionReceipt $receiptPath $receiptId 'success' '' @{ manifestSha256 = $manifestSha256; diagnosticsPath = $diagnosticsPath }
        Remove-Item -Recurse -Force -LiteralPath $rollbackSource
        Write-Host "BubblePlay update committed after internal and external validation; receipt: $receiptPath"
    }
    catch {
        $failure = $_.Exception.Message
        try {
            $collector = & (Join-Path $PSScriptRoot 'collect-diagnostics.ps1') -TailLines 1000
            $diagnosticsPath = [string]$collector.Summary
        } catch { }
        if ($mutationStarted -and (Test-Path -LiteralPath $rollbackSource -PathType Container)) {
            try {
                & $installer -Action Upgrade -SourceDir $rollbackSource -StartServices -Confirm:$false
                foreach ($name in $absentBefore) {
                    $installed = Join-Path $env:ProgramFiles "BubblePlay\$name"
                    if (Test-Path -LiteralPath $installed -PathType Leaf) {
                        Remove-Item -Force -LiteralPath $installed
                    }
                }
                Write-BubblePlayTransactionReceipt $receiptPath $receiptId 'rolled-back' $failure @{ manifestSha256 = $manifestSha256; diagnosticsPath = $diagnosticsPath; rollbackSource = $rollbackSource }
                throw "BubblePlay update failed validation and was rolled back: $failure (receipt $receiptPath)"
            }
            catch {
                if ($_.Exception.Message -like 'BubblePlay update failed validation and was rolled back:*') { throw }
                Write-BubblePlayTransactionReceipt $receiptPath $receiptId 'rollback-failed' "$failure Rollback error: $($_.Exception.Message)" @{ manifestSha256 = $manifestSha256; diagnosticsPath = $diagnosticsPath; rollbackSource = $rollbackSource }
                throw "BubblePlay update failed and rollback also failed. Receipt: $receiptPath"
            }
        }
        Write-BubblePlayTransactionReceipt $receiptPath $receiptId 'failed-before-change' $failure @{ manifestSha256 = $manifestSha256; diagnosticsPath = $diagnosticsPath }
        throw
    }
}

function Read-BubblePlayUpdateReceipt([string]$Path, [string]$ReceiptId) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $null
    }
    try {
        $receipt = Get-Content -Raw -LiteralPath $Path | ConvertFrom-Json
        if ([string]$receipt.receiptId -ne $ReceiptId) {
            return $null
        }
        return $receipt
    }
    catch {
        return $null
    }
}

function Wait-BubblePlayUpdateReceipt(
    [string]$Path,
    [string]$ReceiptId,
    [int]$TimeoutSeconds,
    [switch]$AcceptStarted
) {
    $deadline = [DateTimeOffset]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $receipt = Read-BubblePlayUpdateReceipt $Path $ReceiptId
        if ($null -ne $receipt) {
            $stage = [string]$receipt.stage
            if ($stage -in @('success', 'failed') -or ($AcceptStarted -and $stage -eq 'started')) {
                return $receipt
            }
        }
        Start-Sleep -Milliseconds 200
    } while ([DateTimeOffset]::UtcNow -lt $deadline)
    return $null
}

if ($ApplyManifest) {
    Invoke-BubblePlayManifestUpdate $ApplyManifest -ValidationOnly:$ValidateManifestOnly
    return
}

if (-not $SkipBuild) {
    Push-Location $hostDir
    try {
        $bundledSdkStub = Join-Path $hostDir 'sdk-stub'
        if (-not $env:NVIDIA_VIDEO_CODEC_SDK_PATH -and
            (Test-Path -LiteralPath (Join-Path $bundledSdkStub 'nvEncodeAPI.lib') -PathType Leaf) -and
            (Test-Path -LiteralPath (Join-Path $bundledSdkStub 'nvcuvid.lib') -PathType Leaf)) {
            $env:NVIDIA_VIDEO_CODEC_SDK_PATH = $bundledSdkStub
        }
        & cargo build --release --bins --target-dir target-privileged
        if ($LASTEXITCODE -ne 0) {
            throw "BubblePlay release build failed with exit code $LASTEXITCODE."
        }
    }
    finally {
        Pop-Location
    }
}

foreach ($name in @(
    'bubbleplay_broker.exe',
    'bubbleplay_transport.exe',
    'bubbleplay_agent.exe',
    'bubbleplay_input_proxy.exe',
    'bubbleplay_lab_helper.exe',
    'bubbleplay-host.exe'
)) {
    $path = Join-Path $sourceDir $name
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Required release binary is missing: $path"
    }
}

$escapedInstaller = $installer.Replace("'", "''")
$escapedSourceDir = $sourceDir.Replace("'", "''")
$null = New-Item -ItemType Directory -Path $receiptDir -Force
$receiptId = [Guid]::NewGuid().ToString('N')
$receiptPath = Join-Path $receiptDir "$receiptId.json"
$escapedReceiptPath = $receiptPath.Replace("'", "''")
$resetArgument = if ($ResetLabOwner) { ' -ResetLabOwner' } else { '' }
$elevatedCommand = @"
`$ErrorActionPreference = 'Stop'
`$receiptId = '$receiptId'
`$receiptPath = '$escapedReceiptPath'
function Write-BubblePlayUpdateReceipt([string]`$stage, [string]`$detail) {
    `$payload = [ordered]@{
        receiptId = `$receiptId
        stage = `$stage
        detail = `$detail
        updatedAt = [DateTimeOffset]::UtcNow.ToString('O')
    }
    [IO.File]::WriteAllText(`$receiptPath, (`$payload | ConvertTo-Json -Compress), [Text.Encoding]::UTF8)
}
Write-BubblePlayUpdateReceipt 'started' ''
try {
    & '$escapedInstaller' -Action Upgrade -SourceDir '$escapedSourceDir' -StartServices -Confirm:`$false$resetArgument
    Write-BubblePlayUpdateReceipt 'success' ''
}
catch {
    Write-BubblePlayUpdateReceipt 'failed' `$_.Exception.Message
    throw
}
"@
$encodedCommand = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($elevatedCommand))
$arguments = @('-NoProfile', '-EncodedCommand', $encodedCommand)
$systemPowerShell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'

Write-Host 'Requesting one elevation to atomically stop, update, and restart BubblePlay core services...'
$process = $null
$launchError = $null
try {
    $process = Start-Process -FilePath $systemPowerShell -ArgumentList $arguments -Verb RunAs -Wait -PassThru
}
catch {
    $launchError = $_
}

$receipt = Read-BubblePlayUpdateReceipt $receiptPath $receiptId
if ($null -eq $receipt -and $null -ne $launchError) {
    # Some Windows shells have reported ERROR_CANCELLED after the elevated
    # child was already visible. Give that child a bounded chance to publish
    # its nonce before treating the launcher result as authoritative.
    $receipt = Wait-BubblePlayUpdateReceipt $receiptPath $receiptId 5 -AcceptStarted
}
if ($null -ne $receipt -and [string]$receipt.stage -eq 'started') {
    $receipt = Wait-BubblePlayUpdateReceipt $receiptPath $receiptId 180
}

if ($null -eq $receipt) {
    if ($null -ne $launchError) {
        throw $launchError
    }
    if ($null -ne $process -and $process.ExitCode -ne 0) {
        throw "Elevated BubblePlay update failed with exit code $($process.ExitCode) and published no receipt."
    }
    throw "Elevated BubblePlay update published no receipt: $receiptPath"
}
if ([string]$receipt.stage -eq 'failed') {
    throw "Elevated BubblePlay update failed: $([string]$receipt.detail) (receipt $receiptPath)"
}
if ([string]$receipt.stage -ne 'success') {
    throw "Elevated BubblePlay update did not finish before the receipt deadline: $receiptPath"
}

Write-Host "BubblePlay core services updated and restarted successfully; receipt: $receiptPath"
