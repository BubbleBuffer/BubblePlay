[CmdletBinding()]
param(
    [ValidateRange(1, 5000)]
    [int]$TailLines = 1000,
    [string]$OutputRoot = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$allowedRoot = [IO.Path]::GetFullPath((Join-Path $projectRoot 'artifacts\service-diagnostics'))
if (-not $OutputRoot) {
    $OutputRoot = $allowedRoot
}
$OutputRoot = [IO.Path]::GetFullPath($OutputRoot)
$allowedPrefix = $allowedRoot.TrimEnd('\') + '\'
if ($OutputRoot -ne $allowedRoot -and
    -not $OutputRoot.StartsWith($allowedPrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw "OutputRoot must remain under $allowedRoot"
}

$runId = (Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + [Guid]::NewGuid().ToString('N')
$destination = Join-Path $OutputRoot $runId
New-Item -ItemType Directory -Path $destination -Force | Out-Null

$logRoot = Join-Path $env:ProgramData 'BubblePlay\logs'
$knownLogs = [ordered]@{
    'agent.stderr.log' = Join-Path $logRoot 'system\agent.stderr.log'
    'agent.stdout.log' = Join-Path $logRoot 'system\agent.stdout.log'
    'broker-service.combined.log' = Join-Path $logRoot 'system\broker-service.combined.log'
    'lab-helper.combined.log' = Join-Path $logRoot 'system\lab-helper.combined.log'
    'transport-service.combined.log' = Join-Path $logRoot 'transport\transport-service.combined.log'
    'transport-host.stderr.log' = Join-Path $logRoot 'transport\transport-host.stderr.log'
    'transport-host.stdout.log' = Join-Path $logRoot 'transport\transport-host.stdout.log'
}

$logs = [Collections.Generic.List[object]]::new()
foreach ($entry in $knownLogs.GetEnumerator()) {
    $source = [IO.Path]::GetFullPath([string]$entry.Value)
    $logPrefix = [IO.Path]::GetFullPath($logRoot).TrimEnd('\') + '\'
    if (-not $source.StartsWith($logPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing diagnostics path outside the fixed log root: $source"
    }
    try {
        # Test-Path itself throws when an ancestor deliberately denies traversal.
        # Keep the complete probe inside the guarded block so this collector can
        # always produce a useful summary, including before the readable-log ACL
        # has been deployed.
        $item = Get-Item -LiteralPath $source -ErrorAction Stop
        if ($item.PSIsContainer) {
            $logs.Add([ordered]@{ name = $entry.Key; state = 'missing'; bytes = 0; tail = $null })
            continue
        }
        $tailPath = Join-Path $destination ([string]$entry.Key)
        $tail = @(Get-Content -LiteralPath $source -Tail $TailLines -ErrorAction Stop)
        [IO.File]::WriteAllLines($tailPath, [string[]]$tail, [Text.UTF8Encoding]::new($false))
        $logs.Add([ordered]@{
            name = $entry.Key
            state = 'captured'
            bytes = [int64]$item.Length
            modifiedAt = $item.LastWriteTimeUtc.ToString('O')
            tail = $tailPath
        })
    }
    catch [UnauthorizedAccessException] {
        $logs.Add([ordered]@{ name = $entry.Key; state = 'access-denied'; bytes = 0; tail = $null })
    }
    catch [System.Management.Automation.ItemNotFoundException] {
        $logs.Add([ordered]@{ name = $entry.Key; state = 'missing'; bytes = 0; tail = $null })
    }
}

$services = @(Get-CimInstance Win32_Service -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -like 'BubblePlay*' } |
    ForEach-Object {
        [ordered]@{
            name = $_.Name
            state = $_.State
            processId = [int]$_.ProcessId
            startMode = $_.StartMode
        }
    })

$summary = [ordered]@{
    schemaVersion = 1
    runId = $runId
    collectedAt = [DateTimeOffset]::UtcNow.ToString('O')
    tailLines = $TailLines
    services = $services
    logs = @($logs)
}
$summaryPath = Join-Path $destination 'summary.json'
[IO.File]::WriteAllText(
    $summaryPath,
    ($summary | ConvertTo-Json -Depth 6),
    [Text.UTF8Encoding]::new($false)
)

[pscustomobject]@{
    RunId = $runId
    Summary = $summaryPath
    CapturedLogs = @($logs | Where-Object { $_.state -eq 'captured' }).Count
    AccessDenied = @($logs | Where-Object { $_.state -eq 'access-denied' }).Count
}
