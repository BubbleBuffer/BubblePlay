# Synthetic configuration tests. Import only function ASTs; never execute setup.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$path = Join-Path $PSScriptRoot 'setup-tray.ps1'
$tokens = $null; $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($path, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw 'setup-tray.ps1 has syntax errors.' }
foreach ($name in @('ConvertFrom-Secure', 'Assert-HostCredentials', 'Update-HostConfig')) {
    $node = $ast.Find({ param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq $name }, $true)
    if (-not $node) { throw "Missing tested function $name." }
    Invoke-Expression $node.Extent.Text
}
function Assert-Rejected([scriptblock]$Action, [string]$Message) {
    $caught = $null
    try { & $Action | Out-Null } catch { $caught = $_.Exception.Message }
    if (-not $caught -or $caught -notlike "*$Message*") { throw "Expected rejection: $Message." }
}
function Secure([string]$Value) {
    $result = [Security.SecureString]::new()
    foreach ($character in $Value.ToCharArray()) { $result.AppendChar($character) }
    return $result
}
function Get-ExistingHostConfig { $script:fixtureExisting }
function Get-ServiceConfig { $script:fixtureService }
function Add-Step { param($Name, $Status, $Detail) $script:steps.Add([string]$Status) }
function Protect-UserHex { throw 'Test must never write configuration.' }
function Invoke-ConfigPreview {
    [CmdletBinding(SupportsShouldProcess = $true)]
    param()
    Update-HostConfig
}
$steps = [Collections.Generic.List[string]]::new()
$fixtureExisting = $null; $fixtureService = $null
$Token = $null; $WorkerUrl = $null; $Monitor = $null
$HostConfigPath = 'synthetic-host.json'; $IsWhatIf = $true
$goodToken = 'T' * 32
foreach ($bad in @($null, '', (' ' * 32), ('T' * 31))) {
    Assert-Rejected { Assert-HostCredentials $bad } 'registration token'
}
Assert-HostCredentials $goodToken
Assert-Rejected { Invoke-ConfigPreview -WhatIf } 'No existing configuration'
$WorkerUrl = 'wss://synthetic.invalid/host-ws'; $Token = Secure ('T' * 31)
Assert-Rejected { Invoke-ConfigPreview -WhatIf } 'registration token'
$Token = Secure $goodToken
if ((Invoke-ConfigPreview -WhatIf) -ne 'done' -or $steps[-1] -ne 'planned') { throw 'Valid fresh install should plan config.' }
$fixtureExisting = [pscustomobject]@{ Worker = $WorkerUrl; Token = $goodToken; Monitor = $null; Extra = $false }
$WorkerUrl = $null; $Token = $null
if ((Invoke-ConfigPreview -WhatIf) -ne 'done' -or $steps[-1] -ne 'unchanged') { throw 'Valid saved configuration should remain unchanged.' }
# A saved host.json that still carries a removed field (the old host-gate token) is rewritten without it.
$fixtureExisting.Extra = $true
if ((Invoke-ConfigPreview -WhatIf) -ne 'done' -or $steps[-1] -ne 'planned') { throw 'Saved configuration with removed fields should be rewritten.' }
$fixtureExisting.Extra = $false
$Token = Secure ('U' * 32)
if ((Invoke-ConfigPreview -WhatIf) -ne 'done' -or $steps[-1] -ne 'planned') { throw 'Token-only update must be honored.' }
$Token = Secure ''
Assert-Rejected { Invoke-ConfigPreview -WhatIf } 'registration token'
$Token = $null; $fixtureExisting = $null
$fixtureService = [pscustomobject]@{ Readable = $true; Path = 'synthetic-service.json'; Worker = 'wss://synthetic.invalid/host-ws'; Token = $null }
Assert-Rejected { Invoke-ConfigPreview -WhatIf } 'registration token'
$fixtureService.Token = $goodToken
if ((Invoke-ConfigPreview -WhatIf) -ne 'done') { throw 'Valid synthetic migration should plan config.' }
Write-Output 'setup credential tests passed (boundaries, fresh/saved/migrated config, removed-field rewrite, explicit empty/token-only updates; no writes).'
