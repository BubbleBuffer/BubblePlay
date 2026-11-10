#Requires -Version 5.1
<#
.SYNOPSIS
    Inspect optional app payload selection and fixed machine update boundaries.
.DESCRIPTION
    Imports only source function ASTs. Uses synthetic temporary files, never
    launches an app, elevates, or writes into the real Program Files install.
#>
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$setup = Join-Path $PSScriptRoot 'setup-tray.ps1'
$tokens = $null; $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($setup, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw 'Setup has syntax errors.' }
foreach ($name in @('Assert-NoReparse', 'Get-SafeTreeFiles', 'Remove-SafeTree', 'Test-AppPayloadPath', 'Get-PackageSources')) {
    $node = $ast.Find({ param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq $name }, $true)
    if (-not $node) { throw "Missing tested function $name." }
    Invoke-Expression $node.Extent.Text
}
function Assert([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
$tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
$fixture = Join-Path $tempRoot ('bubbleplay-app-test-' + [Guid]::NewGuid().ToString('N'))
$AppDir = Join-Path $fixture 'Release'
$AppOnly = $true
$SourceDir = $AppDir
# Get-PackageSources skips the base package for AppOnly; it must not require
# host binaries or source scripts to accompany an independently selected app.
$UserBinaries = @(); $UserScripts = @()
try {
    [void][IO.Directory]::CreateDirectory((Join-Path $AppDir 'data\flutter_assets'))
    $expected = @('bubbleplay_app.exe', 'flutter_windows.dll', 'bubbleplay_app_core.dll', 'data\app.so', 'data\flutter_assets\AssetManifest.bin')
    foreach ($name in $expected) { [IO.File]::WriteAllText((Join-Path $AppDir $name), 'synthetic runtime bytes; never execute') }
    foreach ($name in @('host.json', '.env', 'private.txt', 'symbols.pdb', 'probe.exe', 'setup-tray.ps1', 'bubbleplay_tray.exe')) { [IO.File]::WriteAllText((Join-Path $AppDir $name), 'not app payload') }
    $selected = Get-PackageSources
    Assert ($selected.Count -eq $expected.Count) 'App selection includes only runtime files.'
    foreach ($name in $expected) { Assert ($selected.ContainsKey($name)) "App runtime file is selected: $name" }
    foreach ($name in @('..\other.dll', 'C:\other.dll', 'data\..\host.json', 'data\file:stream', 'data\\bad', 'agent\helper.exe', 'previous\host.exe', 'host.json', 'setup-tray.ps1')) {
        Assert (-not (Test-AppPayloadPath $name)) "Unsafe/reserved app path is refused: $name"
    }
    foreach ($scriptName in @('update-tray.ps1', 'setup-tray.ps1')) {
        $rejected = $false
        try { & (Join-Path $PSScriptRoot $scriptName) -AppOnly -AppDir $AppDir -InstallDir (Join-Path $fixture 'Install') -WhatIf }
        catch { if ($_.Exception.Message -notlike '*InstallDir must*') { throw }; $rejected = $true }
        Assert $rejected 'Public updater/setup must refuse a writable install root before UAC.'
        Assert (-not (Test-Path -LiteralPath (Join-Path $fixture 'Install'))) 'A rejected install root was not created.'
    }
}
finally {
    $absolute = [IO.Path]::GetFullPath($fixture)
    if (-not $absolute.StartsWith($tempRoot + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Fixture cleanup escaped the temporary directory.' }
    if (Test-Path -LiteralPath $fixture) { Remove-SafeTree $fixture }
}
Write-Output 'App payload and protected update boundary checks passed; no apps, installer elevation or machine writes.'
