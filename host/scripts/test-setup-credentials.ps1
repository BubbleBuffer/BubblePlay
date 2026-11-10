# Synthetic credential and filesystem boundary checks; never install or elevate.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$path = Join-Path $PSScriptRoot 'setup-tray.ps1'
$tokens = $null; $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($path, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw 'setup-tray.ps1 has syntax errors.' }
foreach ($name in @('ConvertFrom-Secure', 'Assert-HostCredentials', 'Update-HostConfig', 'Get-RemovableServiceNames', 'Remove-BubblePlayServices')) {
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
function Add-Step { param($Name, $Status, $Detail) $script:steps.Add([string]$Status); $script:stepDetails.Add("$Name $Status $Detail") }
function Protect-UserHex { throw 'Test must never write configuration.' }
function Invoke-ConfigPreview {
    [CmdletBinding(SupportsShouldProcess = $true)]
    param()
    Update-HostConfig
}
$steps = [Collections.Generic.List[string]]::new()
$stepDetails = [Collections.Generic.List[string]]::new()
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
# Decision 25: an older install's Transport service is always removed, even with -KeepServices.
$ServiceNames = @('BubblePlayLabHelper', 'BubblePlayTransport', 'BubblePlayBroker')
$TransportServiceName = 'BubblePlayTransport'
$fixtureServices = @('BubblePlayTransport', 'BubblePlayBroker')
function Get-Service { param($Name, $ErrorAction) if ($script:fixtureServices -contains $Name) { [pscustomobject]@{ Name = $Name } } }
function Invoke-ServicesPreview {
    [CmdletBinding(SupportsShouldProcess = $true)]
    param()
    Remove-BubblePlayServices
}
$KeepServices = $false
if ((@(Get-RemovableServiceNames) -join ',') -ne ($ServiceNames -join ',')) { throw 'Without -KeepServices every BubblePlay service is removable.' }
$stepDetails.Clear()
$KeepServices = $true
if ((@(Get-RemovableServiceNames) -join ',') -ne 'BubblePlayTransport') { throw '-KeepServices must still remove Transport.' }
Invoke-ServicesPreview -WhatIf
if ($stepDetails[0] -ne 'services planned Stop and delete: BubblePlayTransport (ProgramData is kept).') { throw "Transport removal must be planned and recorded: $($stepDetails[0])" }
if ($stepDetails[1] -notlike 'services-kept skipped *') { throw 'The kept services must be recorded.' }
Write-Output 'setup credential tests passed (boundaries, fresh/saved/migrated config, removed-field rewrite, explicit empty/token-only updates, Transport removal; no writes).'

# Import the real protection and tree-deletion functions without executing setup.
# Only synthetic credentials and a uniquely named temporary fixture are used.
foreach ($name in @('ConvertTo-Hex', 'ConvertFrom-Hex', 'Protect-UserHex', 'Unprotect-Hex', 'Assert-NoReparse', 'Get-SafeTreeFiles', 'Remove-SafeTree')) {
    $node = $ast.Find({ param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq $name }, $true)
    if (-not $node) { throw "Missing tested function $name." }
    Invoke-Expression $node.Extent.Text
}
Add-Type -AssemblyName System.Security
$encoded = Protect-UserHex $goodToken
if ((Unprotect-Hex $encoded) -ne $goodToken -or $script:LegacyUserProtection) { throw 'App-entropy token must decrypt without legacy fallback.' }
$bytes = [Text.Encoding]::UTF8.GetBytes($goodToken)
try {
    $legacy = ConvertTo-Hex ([Security.Cryptography.ProtectedData]::Protect($bytes, $null, [Security.Cryptography.DataProtectionScope]::CurrentUser))
}
finally { [Array]::Clear($bytes, 0, $bytes.Length) }
if ((Unprotect-Hex $legacy) -ne $goodToken -or -not $script:LegacyUserProtection) { throw 'Legacy null-entropy token must remain readable and be marked for rewrite.' }
$wrongEntropy = [Text.Encoding]::UTF8.GetBytes('synthetic.other.application')
$rejected = $false
try { $null = [Security.Cryptography.ProtectedData]::Unprotect((ConvertFrom-Hex $encoded), $wrongEntropy, [Security.Cryptography.DataProtectionScope]::CurrentUser) } catch { $rejected = $true }
if (-not $rejected) { throw 'An app-entropy blob must reject different entropy.' }

$tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
$fixture = Join-Path $tempRoot ('bubbleplay-install-test-' + [Guid]::NewGuid().ToString('N'))
$tree = Join-Path $fixture 'install'
$outside = Join-Path $fixture 'outside'
$link = Join-Path $tree 'data\assets\linked'
try {
    [void][IO.Directory]::CreateDirectory((Split-Path -Parent $link))
    [void][IO.Directory]::CreateDirectory($outside)
    $sentinel = Join-Path $outside 'keep.txt'
    [IO.File]::WriteAllText($sentinel, 'outside sentinel')
    $null = New-Item -ItemType Junction -Path $link -Target $outside
    Assert-Rejected { Remove-SafeTree $tree } 'Reparse'
    if ([IO.File]::ReadAllText($sentinel) -ne 'outside sentinel') { throw 'Deletion followed a nested junction.' }
    if (-not (Test-Path -LiteralPath $tree)) { throw 'Unsafe tree must remain after rejection.' }
    [IO.Directory]::Delete($link)
    Remove-SafeTree $tree
    if (Test-Path -LiteralPath $tree) { throw 'Ordinary tree removal must complete.' }
    if (-not (Test-Path -LiteralPath $sentinel)) { throw 'Ordinary removal changed the outside sentinel.' }
}
finally {
    $absolute = [IO.Path]::GetFullPath($fixture)
    if (-not $absolute.StartsWith($tempRoot + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Fixture cleanup escaped the temporary directory.' }
    if (Test-Path -LiteralPath $link) { [IO.Directory]::Delete($link) }
    if (Test-Path -LiteralPath $fixture) { Remove-SafeTree $fixture }
}
# Public entry points must reject arbitrary machine install roots before UAC.
Assert-Rejected { & $path -InstallDir (Join-Path $tempRoot 'not-program-files') -WhatIf } 'InstallDir must'
$uninstaller = Join-Path $PSScriptRoot 'uninstall-tray.ps1'
Assert-Rejected { & $uninstaller -InstallDir (Join-Path $tempRoot 'not-program-files') -WhatIf } 'InstallDir must'
Write-Output 'Installer boundary tests passed (synthetic DPAPI entropy, nested-junction refusal, safe deletion and fixed machine paths).'
