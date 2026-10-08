# Exercise the real helper with synthetic prompts and a mocked external command.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$helper = Join-Path $PSScriptRoot 'set-app-password.ps1'
$global:BubblePlayPasswordTest = [pscustomobject]@{
    Prompts = 0; Calls = 0; Answers = [Collections.Generic.Queue[string]]::new()
}
function Read-Host {
    param([string]$Prompt, [switch]$AsSecureString)
    $global:BubblePlayPasswordTest.Prompts++
    $secret = [Security.SecureString]::new()
    foreach ($character in $global:BubblePlayPasswordTest.Answers.Dequeue().ToCharArray()) { $secret.AppendChar($character) }
    return $secret
}
function npx {
    $global:BubblePlayPasswordTest.Calls++
    if ($args[0] -ne 'wrangler' -or $args[-1] -ne 'synthetic-worker') { throw 'Unexpected helper destination/command.' }
    $global:LASTEXITCODE = 0
}
function Invoke-Case([string]$Password, [string]$Repeat, [bool]$Accept) {
    $global:BubblePlayPasswordTest.Answers.Enqueue($Password); $global:BubblePlayPasswordTest.Answers.Enqueue($Repeat)
    $before = $global:BubblePlayPasswordTest.Calls; $caught = $false; $failure = $null
    try { & $script:helper -Worker 'synthetic-worker' | Out-Null } catch { $caught = $true; $failure = $_.Exception.Message }
    if ($caught -eq $Accept -or ($global:BubblePlayPasswordTest.Calls - $before) -ne [int]$Accept) {
        throw "Synthetic password length $($Password.Length): acceptance/command mismatch ($failure)."
    }
}
$caught = $false
try { & $helper | Out-Null } catch { $caught = $true }
if (-not $caught -or $global:BubblePlayPasswordTest.Prompts -ne 0 -or $global:BubblePlayPasswordTest.Calls -ne 0) { throw 'Missing Worker must fail before prompts or commands.' }
foreach ($worker in @(' ', '--unexpected', 'name with spaces')) {
    $caught = $false
    try { & $helper -Worker $worker | Out-Null } catch { $caught = $true }
    if (-not $caught -or $global:BubblePlayPasswordTest.Prompts -ne 0 -or $global:BubblePlayPasswordTest.Calls -ne 0) { throw 'Invalid Worker must fail before prompts or commands.' }
}
Invoke-Case '' '' $false
Invoke-Case ('x' * 11) ('x' * 11) $false
Invoke-Case (' ' * 12) (' ' * 12) $false
Invoke-Case ('x' * 257) ('x' * 257) $false
Invoke-Case 'Synthetic123' 'synthetic123' $false
Invoke-Case ('x' * 12) ('x' * 12) $true
Invoke-Case ('x' * 256) ('x' * 256) $true
# Pin against the relay's current minimum so future changes cannot silently drift.
$relay = Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot '../src/index.js')
if ($relay -notmatch 'APP_PASSWORD:\s*12\b') { throw 'Update helper/test to match relay password minimum.' }
Write-Output 'password helper tests passed (explicit destination, 12/256 boundaries, mismatch; external commands mocked).'
Remove-Variable BubblePlayPasswordTest -Scope Global
