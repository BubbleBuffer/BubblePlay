#Requires -RunAsAdministrator
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[1-5][0-9a-fA-F]{3}-[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}$')]
    [string]$RunId
)

# Read-only system inspection. Writes only a task-local evidence report.
$ErrorActionPreference = 'Stop'
$artifactDirectory = Join-Path (Split-Path -Parent $PSScriptRoot) 'artifacts'
$artifactItem = Get-Item -LiteralPath $artifactDirectory
if ($artifactItem.Attributes -band [IO.FileAttributes]::ReparsePoint) {
    throw 'The evidence directory must not be a reparse point.'
}
$policies = @(Get-NetQosPolicy -PolicyStore ActiveStore -ErrorAction Stop |
    Where-Object { $_.Name -like 'BubblePlay-StreamLab-*' -or $_.Name -like 'BubblePlayLab-*' } |
    Select-Object -ExpandProperty Name)
$report = [ordered]@{
    runId = $RunId.ToLowerInvariant()
    observedAt = [DateTimeOffset]::UtcNow.ToString('o')
    policyStore = 'ActiveStore'
    querySucceeded = $true
    activeLabPolicies = $policies
    labPoliciesAbsent = ($policies.Count -eq 0)
    helperState = [string](Get-Service -Name 'BubblePlayLabHelper' -ErrorAction Stop).Status
}
$outputPath = Join-Path $artifactDirectory ('native-qos-' + $RunId.ToLowerInvariant() + '.json')
if (Test-Path -LiteralPath $outputPath) { throw "Evidence already exists: $outputPath" }
$report | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $outputPath -Encoding UTF8
Write-Output $outputPath
if ($policies.Count -gt 0) { exit 1 }
