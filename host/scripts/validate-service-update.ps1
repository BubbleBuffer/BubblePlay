[CmdletBinding()]
param(
    [string]$ManifestPath = '',
    [string]$Url = 'https://playfull-cat.your-subdomain.workers.dev/',
    [ValidatePattern('^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$')]
    [string]$WorkerVersionId = '',
    [ValidateRange(1, 10)]
    [int]$SmokeRuns = 3,
    [ValidateRange(3, 15)]
    [int]$MaxSmokeAttempts = 9,
    [ValidateRange(60, 1800)]
    [int]$SignalingSeconds = 600,
    [ValidateRange(10, 300)]
    [int]$CleanupSeconds = 60,
    [switch]$EnvironmentOnly,
    [string]$ParseSmokeLog = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$hostDir = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$projectRoot = [IO.Path]::GetFullPath((Join-Path $hostDir '..'))
$workerDir = Join-Path $projectRoot 'worker'
$preflightRoot = [IO.Path]::GetFullPath((Join-Path $hostDir 'target-privileged\update-preflight'))
$timeline = [Collections.Generic.List[object]]::new()
$MaxHostRecoveries = 3
$HostRecoveryTimeoutSeconds = 15

function Add-Evidence([string]$Phase, [string]$Status, $Detail) {
    $timeline.Add([ordered]@{ at = [DateTimeOffset]::UtcNow.ToString('O'); phase = $Phase; status = $Status; detail = $Detail })
    $path = Join-Path $evidenceDir 'timeline.json'
    [IO.File]::WriteAllText($path, (@($timeline) | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
}

function Get-LabChromeProcesses {
    return @(Get-CimInstance Win32_Process -Filter "Name='chrome.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -match '--remote-debugging-port=(19341|19342)' } |
        Select-Object ProcessId, CommandLine)
}

function Get-DisplayNames {
    Add-Type -AssemblyName System.Windows.Forms
    return @([System.Windows.Forms.Screen]::AllScreens | ForEach-Object DeviceName | Sort-Object -Unique)
}

function Get-CleanupState([string[]]$ExpectedDisplays) {
    $helper = Get-CimInstance Win32_Service -Filter "Name='BubblePlayLabHelper'"
    $qos = @()
    $qosEvidenceAvailable = $false
    $qosError = $null
    try {
        $qos = @(Get-NetQosPolicy -PolicyStore ActiveStore -ErrorAction Stop |
            Where-Object { $_.Name -like 'BubblePlay-StreamLab-*' -or $_.Name -like 'BubblePlayLab-*' } |
            Select-Object -ExpandProperty Name)
        $qosEvidenceAvailable = $true
    }
    catch { $qosError = $_.Exception.Message }
    $displays = Get-DisplayNames
    $displayDiff = @(Compare-Object $ExpectedDisplays $displays)
    $chrome = @(Get-LabChromeProcesses)
    return [ordered]@{
        clean = ($helper.State -eq 'Stopped' -and
            (-not $qosEvidenceAvailable -or $qos.Count -eq 0) -and
            $displayDiff.Count -eq 0 -and $chrome.Count -eq 0)
        helperState = $helper.State
        qosEvidenceAvailable = $qosEvidenceAvailable
        qosPolicies = $qos
        qosError = $qosError
        displays = $displays
        displayDiff = $displayDiff
        labChrome = @($chrome | Select-Object ProcessId)
    }
}

function Read-BubblePlayCliJson([string]$Path) {
    $lines = @(Get-Content -LiteralPath $Path -ErrorAction Stop)
    $start = -1
    for ($index = 0; $index -lt $lines.Count; $index++) {
        if ($lines[$index].TrimStart().StartsWith('{')) { $start = $index; break }
    }
    if ($start -lt 0) { throw "CLI output contains no trailing JSON object: $Path" }
    return (($lines[$start..($lines.Count - 1)] -join "`n") | ConvertFrom-Json)
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

function Assert-StableHost(
    [int]$TransportPid,
    [Collections.Generic.List[int]]$HostPids,
    [Collections.Generic.List[object]]$HostRecoveries,
    [string]$HostLog
) {
    $transport = Get-CimInstance Win32_Service -Filter "Name='BubblePlayTransport'"
    if ($transport.State -ne 'Running' -or [int]$transport.ProcessId -ne $TransportPid) {
        throw 'BubblePlayTransport stopped or restarted during external validation.'
    }
    $currentHosts = @(Get-CimInstance Win32_Process -Filter "Name='bubbleplay-host.exe'" |
        Select-Object ProcessId, CreationDate)
    $currentHostPids = @($currentHosts | ForEach-Object { [int]$_.ProcessId } | Sort-Object)
    if ($HostPids.Count -eq $currentHostPids.Count -and
        $HostPids.Count -gt 0 -and
        @(Compare-Object -ReferenceObject @($HostPids) -DifferenceObject $currentHostPids).Count -eq 0) {
        return $null
    }
    if ($HostRecoveries.Count -ge $MaxHostRecoveries) {
        throw "bubbleplay-host recovery budget exhausted after $($HostRecoveries.Count) recoveries."
    }

    $previousHostPids = @($HostPids)
    $observedAt = [DateTimeOffset]::UtcNow
    $deadline = $observedAt.AddSeconds($HostRecoveryTimeoutSeconds)
    $recoveredHost = $null
    do {
        $transport = Get-CimInstance Win32_Service -Filter "Name='BubblePlayTransport'"
        if ($transport.State -ne 'Running' -or [int]$transport.ProcessId -ne $TransportPid) {
            throw 'BubblePlayTransport stopped or restarted during external validation.'
        }
        $currentHosts = @(Get-CimInstance Win32_Process -Filter "Name='bubbleplay-host.exe'" |
            Select-Object ProcessId, CreationDate)
        if ($currentHosts.Count -eq 1 -and $null -ne $currentHosts[0].CreationDate) {
            $candidatePid = [int]$currentHosts[0].ProcessId
            if ($candidatePid -in $previousHostPids) { return $null }
            $candidateStartedAt = [DateTimeOffset]::new([DateTime]$currentHosts[0].CreationDate)
            if (Test-BubblePlayRecentHostRegistration $HostLog ($candidateStartedAt.AddSeconds(-2))) {
                $recoveredHost = $currentHosts[0]
                break
            }
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTimeOffset]::UtcNow -lt $deadline)

    if ($null -eq $recoveredHost) {
        $observedHostPids = @($currentHosts | ForEach-Object { [int]$_.ProcessId } | Sort-Object)
        throw "bubbleplay-host did not re-register within $HostRecoveryTimeoutSeconds seconds: $($previousHostPids -join ',') -> $($observedHostPids -join ',')"
    }

    $nextPid = [int]$recoveredHost.ProcessId
    $HostPids.Clear()
    $HostPids.Add($nextPid)
    $recovery = [ordered]@{
        previousHostPids = $previousHostPids
        hostPid = $nextPid
        observedAt = $observedAt.ToString('O')
        recoveryObservedMs = [math]::Round(([DateTimeOffset]::UtcNow - $observedAt).TotalMilliseconds, 1)
    }
    $HostRecoveries.Add($recovery)
    return $recovery
}

if ($ParseSmokeLog) {
    return Read-BubblePlayCliJson ([IO.Path]::GetFullPath($ParseSmokeLog))
}

if ($EnvironmentOnly) {
    $environmentDisplays = @(Get-DisplayNames)
    $environmentChrome = @(Get-LabChromeProcesses)
    $environmentCleanup = Get-CleanupState $environmentDisplays
    return [pscustomobject]@{
        ShapeSafe = $true
        DisplayCount = $environmentDisplays.Count
        LabChromeCount = $environmentChrome.Count
        CleanupState = $environmentCleanup
    }
}

if (-not $ManifestPath) { throw 'ManifestPath is required unless -EnvironmentOnly is used.' }
if (-not $WorkerVersionId) { throw 'WorkerVersionId is required for live external validation.' }
$manifestPath = [IO.Path]::GetFullPath($ManifestPath)
$manifestDir = Split-Path -Parent $manifestPath
if (-not $manifestPath.StartsWith(($preflightRoot.TrimEnd('\') + '\'), [StringComparison]::OrdinalIgnoreCase)) {
    throw "Manifest must remain below $preflightRoot"
}
$manifest = Get-Content -Raw -LiteralPath $manifestPath | ConvertFrom-Json
$requestPath = [IO.Path]::GetFullPath([string]$manifest.validationRequestPath)
$evidenceDir = Join-Path $manifestDir 'external-validation'
New-Item -ItemType Directory -Path $evidenceDir -Force | Out-Null

$publisher = Join-Path $PSScriptRoot 'publish-update-validation.ps1'
$collector = Join-Path $PSScriptRoot 'collect-diagnostics.ps1'
$helperProbe = Join-Path $workerDir 'tools\stream-lab-helper-probe.mjs'
$labCli = Join-Path $workerDir 'tools\stream-lab.mjs'
$summaryPath = Join-Path $evidenceDir 'summary.json'
$hostLog = Join-Path $env:ProgramData 'BubblePlay\logs\transport\transport-host.stderr.log'

function Assert-StableHostAndRecord(
    [int]$TransportPid,
    [Collections.Generic.List[int]]$HostPids,
    [Collections.Generic.List[object]]$HostRecoveries
) {
    $recovery = Assert-StableHost $TransportPid $HostPids $HostRecoveries $hostLog
    if ($null -ne $recovery) { Add-Evidence 'host-recovery' 'pass' $recovery }
}

try {
    $requestDeadline = [DateTimeOffset]::UtcNow.AddSeconds(180)
    while (-not (Test-Path -LiteralPath $requestPath -PathType Leaf) -and [DateTimeOffset]::UtcNow -lt $requestDeadline) {
        Start-Sleep -Milliseconds 500
    }
    if (-not (Test-Path -LiteralPath $requestPath -PathType Leaf)) { throw 'The updater did not request external validation within 180 seconds.' }
    Add-Evidence 'request' 'pass' $requestPath

    $diagnostics = & $collector -TailLines 1000
    if ($diagnostics.AccessDenied -ne 0 -or $diagnostics.CapturedLogs -ne 7) {
        throw "Diagnostics captured $($diagnostics.CapturedLogs)/7 logs with $($diagnostics.AccessDenied) access denied."
    }
    Add-Evidence 'diagnostics' 'pass' ([string]$diagnostics.Summary)

    $probeLog = Join-Path $evidenceDir 'helper-probe.log'
    $probeOutput = @(& node.exe $helperProbe 2>&1)
    [IO.File]::WriteAllLines($probeLog, [string[]]$probeOutput, [Text.UTF8Encoding]::new($false))
    if ($LASTEXITCODE -ne 0) { throw "No-VDD Helper probe failed. See $probeLog" }
    Add-Evidence 'helper-probe' 'pass' $probeLog

    $preDisplays = Get-DisplayNames
    if (@(Get-LabChromeProcesses).Count -ne 0) { throw 'A stale Stream Lab Chrome process exists before smoke validation.' }
    $transport = Get-CimInstance Win32_Service -Filter "Name='BubblePlayTransport'"
    if ($transport.State -ne 'Running' -or [int]$transport.ProcessId -le 0) { throw 'BubblePlayTransport is not running.' }
    $transportPid = [int]$transport.ProcessId
    $hostPids = [Collections.Generic.List[int]]::new()
    foreach ($hostPid in @(Get-CimInstance Win32_Process -Filter "Name='bubbleplay-host.exe'" |
        ForEach-Object { [int]$_.ProcessId } | Sort-Object)) {
        $hostPids.Add($hostPid)
    }
    if ($hostPids.Count -ne 1) { throw "Expected exactly one bubbleplay-host process; found $($hostPids.Count)." }
    $hostRecoveries = [Collections.Generic.List[object]]::new()
    $signalingStarted = [DateTimeOffset]::UtcNow
    Add-Evidence 'signaling-stability' 'started' @{ transportPid = $transportPid; hostPids = @($hostPids); seconds = $SignalingSeconds }

    $smokePasses = 0
    $smokeAttempt = 0
    while ($smokePasses -lt $SmokeRuns -and $smokeAttempt -lt $MaxSmokeAttempts) {
        $smokeAttempt++
        $smokeLog = Join-Path $evidenceDir "smoke-attempt-$smokeAttempt.json"
        Push-Location $workerDir
        try {
            $smokeErrorActionPreference = $ErrorActionPreference
            $ErrorActionPreference = 'Continue'
            try {
                $smokeOutput = @(& node.exe $labCli 'smoke' '--url' $Url '--worker-version-id' $WorkerVersionId '--seconds' '5' '--repetitions' '1' '--no-action-probe' 2>&1)
                $smokeExit = $LASTEXITCODE
            }
            finally { $ErrorActionPreference = $smokeErrorActionPreference }
        }
        finally { Pop-Location }
        [IO.File]::WriteAllLines($smokeLog, [string[]]$smokeOutput, [Text.UTF8Encoding]::new($false))
        $smoke = Read-BubblePlayCliJson $smokeLog
        if ($smokeExit -eq 2 -and [string]$smoke.result -eq 'unavailable') {
            Assert-StableHostAndRecord $transportPid $hostPids $hostRecoveries
            Add-Evidence "smoke-attempt-$smokeAttempt" 'retry' @{ log = $smokeLog; code = $smoke.error.code; message = $smoke.error.message }
            Start-Sleep -Seconds 5
            continue
        }
        if ($smokeExit -ne 0 -or [string]$smoke.result -in @('fail', 'unavailable', 'rejected')) {
            throw "Stream Lab smoke attempt $smokeAttempt failed with exit code $smokeExit and result $([string]$smoke.result). See $smokeLog"
        }
        $smokePasses++
        Assert-StableHostAndRecord $transportPid $hostPids $hostRecoveries
        Add-Evidence "smoke-$smokePasses" 'pass' @{ attempt = $smokeAttempt; log = $smokeLog; runId = $smoke.runId }
    }
    if ($smokePasses -ne $SmokeRuns) { throw "Only $smokePasses/$SmokeRuns smoke passes completed in $MaxSmokeAttempts attempts." }

    while (([DateTimeOffset]::UtcNow - $signalingStarted).TotalSeconds -lt $SignalingSeconds) {
        Assert-StableHostAndRecord $transportPid $hostPids $hostRecoveries
        Start-Sleep -Seconds 5
    }
    Assert-StableHostAndRecord $transportPid $hostPids $hostRecoveries
    Add-Evidence 'signaling-stability' 'pass' @{
        seconds = [math]::Round(([DateTimeOffset]::UtcNow - $signalingStarted).TotalSeconds, 1)
        transportPid = $transportPid
        hostPids = @($hostPids)
        hostRecoveries = @($hostRecoveries)
    }

    $cleanupDeadline = [DateTimeOffset]::UtcNow.AddSeconds(30)
    do {
        $cleanup = Get-CleanupState $preDisplays
        if (-not $cleanup.clean) { Start-Sleep -Seconds 1 }
    } while (-not $cleanup.clean -and [DateTimeOffset]::UtcNow -lt $cleanupDeadline)
    if (-not $cleanup.clean) { throw "Lab resources did not clean up: $($cleanup | ConvertTo-Json -Compress -Depth 6)" }
    $cleanStarted = [DateTimeOffset]::UtcNow
    do {
        Start-Sleep -Seconds 1
        $cleanup = Get-CleanupState $preDisplays
        if (-not $cleanup.clean) { throw "Lab cleanup did not remain stable: $($cleanup | ConvertTo-Json -Compress -Depth 6)" }
    } while (([DateTimeOffset]::UtcNow - $cleanStarted).TotalSeconds -lt $CleanupSeconds)
    Add-Evidence 'cleanup-idle' 'pass' @{ seconds = $CleanupSeconds; state = $cleanup }

    $summary = [ordered]@{
        result = 'pass'
        manifest = $manifestPath
        completedAt = [DateTimeOffset]::UtcNow.ToString('O')
        smokeRuns = $SmokeRuns
        smokeAttempts = $smokeAttempt
        signalingSeconds = $SignalingSeconds
        cleanupSeconds = $CleanupSeconds
        timeline = @($timeline)
    }
    [IO.File]::WriteAllText($summaryPath, ($summary | ConvertTo-Json -Depth 10), [Text.UTF8Encoding]::new($false))
    & $publisher -ManifestPath $manifestPath -Result pass -EvidencePath $summaryPath -Detail 'All non-privileged external validation gates passed.' | Out-Null
    $summary | ConvertTo-Json -Depth 10
}
catch {
    $failure = $_.Exception.Message
    try {
        $diagnostics = & $collector -TailLines 2000
        Add-Evidence 'failure-diagnostics' 'captured' ([string]$diagnostics.Summary)
    } catch { }
    Add-Evidence 'external-validation' 'fail' $failure
    $summary = [ordered]@{ result = 'fail'; manifest = $manifestPath; failedAt = [DateTimeOffset]::UtcNow.ToString('O'); error = $failure; timeline = @($timeline) }
    [IO.File]::WriteAllText($summaryPath, ($summary | ConvertTo-Json -Depth 10), [Text.UTF8Encoding]::new($false))
    try { & $publisher -ManifestPath $manifestPath -Result fail -EvidencePath $summaryPath -Detail $failure | Out-Null } catch { }
    throw "External validation failed and rollback was requested: $failure Evidence: $summaryPath"
}
