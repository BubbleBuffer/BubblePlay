#Requires -Version 5.1
<#
.SYNOPSIS
    Plan-only tests (-WhatIf, fake folders under the temp dir, no UAC, no
    process or install touched) for the elevation order of
    update-tray.ps1 -IncludeAgent and the source handling of
    setup-tray.ps1 -AgentOnly.

.EXAMPLE
    pwsh -NoProfile -File host/scripts/test-update-order.ps1
#>
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Join-Path ([IO.Path]::GetTempPath()) ("bubbleplay-order-test-{0}" -f [Guid]::NewGuid().ToString('N'))
$failures = 0
function Assert-That([bool]$Condition, [string]$Message) {
    if ($Condition) { Write-Host "ok   $Message" } else { Write-Host "FAIL $Message"; $script:failures++ }
}
function New-Fakes([string]$Dir, [string[]]$Names) {
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    foreach ($n in $Names) { [IO.File]::WriteAllText((Join-Path $Dir $n), "fake $n") }
}
$all = 'bubbleplay-host.exe', 'bubbleplay_tray.exe', 'bubbleplay_admin_agent.exe', 'bubbleplay_secure_helper.exe'
$agentOnly = 'bubbleplay_admin_agent.exe', 'bubbleplay_secure_helper.exe'

try {
    # 1. update-tray -IncludeAgent: the UAC step is planned before the host is stopped.
    $release = Join-Path $root 'Release'
    New-Fakes $release $all
    $env:BUBBLEPLAY_TRAY_DATA_DIR = Join-Path $root 'data'
    $out = (& pwsh -NoProfile -File (Join-Path $PSScriptRoot 'update-tray.ps1') -WhatIf -IncludeAgent -NoStart `
            -SourceDir $release -InstallDir (Join-Path $root 'Install') -AgentDir (Join-Path $root 'Agent') `
            -AppDir (Join-Path $root 'NoApp') *>&1 | Out-String)
    $elevate = $out.IndexOf('privileged helpers from the new build')
    $stop = $out.IndexOf('Stop tray and replace')
    Assert-That ($elevate -ge 0 -and $stop -ge 0 -and $elevate -lt $stop) 'update-tray plans the elevation before stopping the host'

    # 2. setup-tray -AgentOnly without -SourceDir: default source missing -> install folder copies.
    # The script runs from a temp folder so ..\target\release cannot exist.
    $scripts = Join-Path $root 'scripts'
    New-Item -ItemType Directory -Force -Path $scripts | Out-Null
    Copy-Item (Join-Path $PSScriptRoot 'setup-tray.ps1') $scripts
    $install = Join-Path $root 'Install2'
    New-Fakes $install $agentOnly
    $out = (& pwsh -NoProfile -File (Join-Path $scripts 'setup-tray.ps1') -WhatIf -AgentOnly -InstallDir $install *>&1 | Out-String)
    Assert-That ($out -match '"sourceDir":\s*"[^"]*Install2"') 'setup-tray -AgentOnly falls back to the install folder'

    # 3. No source anywhere: it fails, it does not plan an elevation.
    $empty = Join-Path $root 'Empty'
    New-Item -ItemType Directory -Force -Path $empty | Out-Null
    $out = (& pwsh -NoProfile -File (Join-Path $scripts 'setup-tray.ps1') -WhatIf -AgentOnly -InstallDir $empty *>&1 | Out-String)
    Assert-That ($LASTEXITCODE -ne 0 -and $out -match 'Missing .*bubbleplay_admin_agent') 'setup-tray -AgentOnly with a missing source fails before elevation'
}
finally {
    Remove-Item Env:BUBBLEPLAY_TRAY_DATA_DIR -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force -LiteralPath $root -ErrorAction SilentlyContinue
}
if ($failures) { Write-Host "$failures failure(s)"; exit 1 }
Write-Host 'all ok'
