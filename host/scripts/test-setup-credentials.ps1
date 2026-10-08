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
$Token = $null; $HostGateToken = $null; $WorkerUrl = $null; $Monitor = $null
$HostConfigPath = 'synthetic-host.json'; $IsWhatIf = $true
$goodToken = 'T' * 32; $goodGate = 'G' * 32
foreach ($bad in @($null, '', (' ' * 32), ('T' * 31))) {
    Assert-Rejected { Assert-HostCredentials $bad $goodGate } 'registration token'
    Assert-Rejected { Assert-HostCredentials $goodToken $bad } 'endpoint token'
}
Assert-HostCredentials $goodToken $goodGate
Assert-Rejected { Invoke-ConfigPreview -WhatIf } 'No existing configuration'
$WorkerUrl = 'wss://synthetic.invalid/host-ws'; $Token = Secure $goodToken
Assert-Rejected { Invoke-ConfigPreview -WhatIf } 'endpoint token'
$HostGateToken = Secure ('G' * 31)
Assert-Rejected { Invoke-ConfigPreview -WhatIf } 'endpoint token'
$HostGateToken = Secure $goodGate
if ((Invoke-ConfigPreview -WhatIf) -ne 'done' -or $steps[-1] -ne 'planned') { throw 'Valid fresh install should plan config.' }
$fixtureExisting = [pscustomobject]@{ Worker = $WorkerUrl; Token = $goodToken; HostGateToken = $goodGate; Monitor = $null }
$WorkerUrl = $null; $Token = $null; $HostGateToken = $null
if ((Invoke-ConfigPreview -WhatIf) -ne 'done' -or $steps[-1] -ne 'unchanged') { throw 'Valid saved configuration should remain unchanged.' }
$HostGateToken = Secure ('H' * 32)
if ((Invoke-ConfigPreview -WhatIf) -ne 'done' -or $steps[-1] -ne 'planned') { throw 'Gate-only update must be honored.' }
$HostGateToken = Secure ''
Assert-Rejected { Invoke-ConfigPreview -WhatIf } 'endpoint token'
$HostGateToken = $null; $Token = Secure ''
Assert-Rejected { Invoke-ConfigPreview -WhatIf } 'registration token'
$Token = $null; $fixtureExisting = $null
$fixtureService = [pscustomobject]@{ Readable = $true; Path = 'synthetic-service.json'; Worker = 'wss://synthetic.invalid/host-ws'; Token = $goodToken; HostGateToken = $null }
Assert-Rejected { Invoke-ConfigPreview -WhatIf } 'endpoint token'
$fixtureService.HostGateToken = $goodGate
if ((Invoke-ConfigPreview -WhatIf) -ne 'done') { throw 'Valid synthetic migration should plan config.' }
Write-Output 'setup credential tests passed (boundaries, fresh/saved/migrated config, explicit empty/gate-only updates; no writes).'
