#Requires -Version 5.1
[CmdletBinding()]
param([string]$Builder)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $Builder) { $Builder = Join-Path $PSScriptRoot '..\package-release.ps1' }
Add-Type -AssemblyName System.IO.Compression.FileSystem
$artifactRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..\artifacts'))
$fixture = Join-Path $artifactRoot ('package-test-' + [Guid]::NewGuid().ToString('N'))
$binaryDir = Join-Path $fixture 'binaries'
$scriptDir = Join-Path $fixture 'scripts'
$docs = Join-Path $fixture 'docs'
$junction = Join-Path $fixture 'linked-binaries'
$utf8 = New-Object Text.UTF8Encoding($false)
$count = 0

function Assert([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "Assertion failed: $Message" }
}
function Expect-Failure([scriptblock]$Action, [string]$Pattern) {
    try { $null = & $Action } catch {
        Assert ($_.Exception.Message -match $Pattern) "Expected $Pattern, got $($_.Exception.Message)"
        return
    }
    throw "Expected failure matching $Pattern."
}
function Write-Fixture([string]$Path, [string]$Text) { [IO.File]::WriteAllText($Path, $Text, $utf8) }

try {
    foreach ($dir in @($binaryDir, $scriptDir, $docs)) { [void][IO.Directory]::CreateDirectory($dir) }
    foreach ($name in @('bubbleplay-host.exe', 'bubbleplay_tray.exe', 'bubbleplay_admin_agent.exe', 'bubbleplay_secure_helper.exe')) {
        # Structural x64 PE fixture; never launch these synthetic bytes.
        $bytes = New-Object byte[] 256
        [BitConverter]::GetBytes([uint16]0x5a4d).CopyTo($bytes, 0)
        [BitConverter]::GetBytes([uint32]128).CopyTo($bytes, 60)
        [BitConverter]::GetBytes([uint32]0x4550).CopyTo($bytes, 128)
        [BitConverter]::GetBytes([uint16]0x8664).CopyTo($bytes, 132)
        [BitConverter]::GetBytes([uint16]0x20b).CopyTo($bytes, 152)
        [IO.File]::WriteAllBytes((Join-Path $binaryDir $name), $bytes)
    }
    foreach ($name in @('setup-tray.ps1', 'update-tray.ps1', 'uninstall-tray.ps1')) { Write-Fixture (Join-Path $scriptDir $name) '# synthetic script; never run' }
    Write-Fixture (Join-Path $docs 'quickstart.md') 'Public synthetic quickstart.'
    Write-Fixture (Join-Path $docs 'notices.md') 'Public synthetic dependency notices.'
    Write-Fixture (Join-Path $docs 'license') 'Explicit synthetic license.'
    foreach ($name in @('host.json', '.env', 'identity.json', 'token.txt', 'host.log', 'bubbleplay_agent.exe', 'probe_native_file_drop.exe', 'private-companion.zip')) {
        Write-Fixture (Join-Path $binaryDir $name) 'EXCLUDED_SYNTHETIC_PRIVATE_MARKER'
    }
    [void][IO.Directory]::CreateDirectory((Join-Path $binaryDir 'drivers'))
    Write-Fixture (Join-Path $binaryDir 'drivers\private.inf') 'EXCLUDED_SYNTHETIC_PRIVATE_MARKER'
    $parameters = @{
        Version = '0.1.0-alpha.1'; SourceRevision = ('a' * 40); SourceDirty = $true
        BinaryDir = $binaryDir; ScriptDir = $scriptDir
        QuickstartPath = (Join-Path $docs 'quickstart.md'); DependencyNoticesPath = (Join-Path $docs 'notices.md')
        OutputDir = (Join-Path $fixture 'out-one')
    }
    $first = & $Builder @parameters
    $zip = [IO.Compression.ZipFile]::OpenRead($first.path)
    try {
        $expected = @('bubbleplay-host.exe', 'bubbleplay_tray.exe', 'bubbleplay_admin_agent.exe', 'bubbleplay_secure_helper.exe', 'setup-tray.ps1', 'update-tray.ps1', 'uninstall-tray.ps1', 'QUICKSTART.md', 'THIRD-PARTY-NOTICES.md', 'release-manifest.json')
        Assert ($zip.Entries.Count -eq $expected.Count) 'Only the explicit public allowlist is packaged.'
        foreach ($entry in $zip.Entries) {
            Assert ($expected -ccontains $entry.FullName) "Entry excluded from allowlist: $($entry.FullName)"
            $reader = New-Object IO.StreamReader($entry.Open())
            try { Assert (-not $reader.ReadToEnd().Contains('EXCLUDED_SYNTHETIC_PRIVATE_MARKER')) 'No synthetic private material is included.' } finally { $reader.Dispose() }
        }
        $reader = New-Object IO.StreamReader($zip.GetEntry('release-manifest.json').Open())
        try { $manifest = $reader.ReadToEnd() | ConvertFrom-Json } finally { $reader.Dispose() }
        Assert ($manifest.sourceDirty -eq $true -and $manifest.sourceState -eq 'uncommitted') 'Dirty candidate is explicitly identified.'
        Assert ($manifest.releaseIntent -eq 'candidate' -and -not $manifest.productLicenseIncluded) 'Candidate does not imply product licensing.'
        Assert ($manifest.files.Count -eq 9) 'Manifest inventories every payload file.'
        Assert ($manifest.profile -eq 'per-machine-tray') 'Release declares the protected machine install layout.'
    }
    finally { $zip.Dispose() }
    $count++

    $parameters.OutputDir = Join-Path $fixture 'out-two'
    $second = & $Builder @parameters
    Assert ($first.sha256 -ceq $second.sha256) 'Identical inputs produce a byte-identical ZIP independent of output path.'
    $count++

    Expect-Failure { & $Builder @parameters } 'Output already exists'
    Assert ((Get-FileHash -LiteralPath $second.path -Algorithm SHA256).Hash.ToLowerInvariant() -ceq $second.sha256) 'Existing ZIP remains unchanged after refusal.'
    $overwritten = & $Builder @parameters -Overwrite
    Assert ($overwritten.sha256 -ceq $second.sha256) 'Explicit overwrite remains deterministic.'
    $count++

    $parameters.OutputDir = Join-Path $fixture 'missing-output'
    $missing = Join-Path $binaryDir 'bubbleplay_tray.exe'
    Move-Item -LiteralPath $missing -Destination ($missing + '.saved')
    Expect-Failure { & $Builder @parameters } 'Required release file missing'
    Assert (-not (Test-Path -LiteralPath $parameters.OutputDir)) 'Missing binary fails before output creation.'
    Move-Item -LiteralPath ($missing + '.saved') -Destination $missing
    $count++

    $null = New-Item -ItemType Junction -Path $junction -Target $binaryDir
    $parameters.BinaryDir = $junction
    Expect-Failure { & $Builder @parameters } 'Reparse/symbolic'
    $parameters.BinaryDir = $binaryDir
    $count++

    $parameters.BinaryDir = Join-Path $fixture 'relay-staging\host\target\release'
    Expect-Failure { & $Builder @parameters } 'obsolete relay-staging'
    $parameters.BinaryDir = $binaryDir
    $count++

    Expect-Failure { & $Builder @parameters -PublicRelease } 'requires.*LicensePath'
    $parameters.LicensePath = Join-Path $docs 'license'
    $public = & $Builder @parameters -PublicRelease
    Assert ($public.entries -eq 11) 'Explicit supplied license is included for public release intent.'
    $count++

    $parameters.OutputDir = Join-Path $fixture 'hash-mismatch'
    Expect-Failure { & $Builder @parameters -ExpectedBinaryHashes @{ 'bubbleplay-host.exe' = ('0' * 64) } } 'hash does not match'
    Assert (-not (Test-Path -LiteralPath $parameters.OutputDir)) 'Wrong binary identity never reaches output.'
    $count++

    # Optional Flutter runtime belongs to the same protected program package.
    # It includes root DLLs and nested assets, never symbols/configs or tools.
    $appDir = Join-Path $fixture 'flutter-release'
    [void][IO.Directory]::CreateDirectory((Join-Path $appDir 'data\flutter_assets'))
    foreach ($name in @('bubbleplay_app.exe', 'flutter_windows.dll', 'bubbleplay_app_core.dll')) {
        Copy-Item -LiteralPath (Join-Path $binaryDir 'bubbleplay-host.exe') -Destination (Join-Path $appDir $name)
    }
    Write-Fixture (Join-Path $appDir 'data\icudtl.dat') 'Synthetic runtime data.'
    Write-Fixture (Join-Path $appDir 'data\flutter_assets\AssetManifest.bin') 'Synthetic asset manifest.'
    foreach ($name in @('host.json', '.env', 'symbols.pdb', 'probe.exe', 'host.log')) {
        Write-Fixture (Join-Path $appDir $name) 'EXCLUDED_SYNTHETIC_PRIVATE_MARKER'
    }
    $parameters.AppDir = $appDir
    $parameters.OutputDir = Join-Path $fixture 'with-app'
    $appPackage = & $Builder @parameters
    $zip = [IO.Compression.ZipFile]::OpenRead($appPackage.path)
    try {
        foreach ($name in @('bubbleplay_app.exe', 'flutter_windows.dll', 'bubbleplay_app_core.dll', 'data/icudtl.dat', 'data/flutter_assets/AssetManifest.bin')) {
            Assert ($null -ne $zip.GetEntry($name)) "App runtime file is packaged: $name"
        }
        foreach ($name in @('host.json', '.env', 'symbols.pdb', 'probe.exe', 'host.log')) {
            Assert ($null -eq $zip.GetEntry($name)) "Unrelated app output stays excluded: $name"
        }
        $reader = New-Object IO.StreamReader($zip.GetEntry('release-manifest.json').Open())
        try { $manifest = $reader.ReadToEnd() | ConvertFrom-Json } finally { $reader.Dispose() }
        Assert ($manifest.files.Count -eq 15) 'Manifest covers the licensed base payload and every Flutter runtime file.'
    }
    finally { $zip.Dispose() }
    $count++
    Write-Output "$count package checks passed; no executables or installer scripts were run."
}
finally {
    # Delete only the fixture this script created. Remove its junction itself
    # first so recursive cleanup cannot traverse a reparse point.
    $absolute = [IO.Path]::GetFullPath($fixture)
    if (-not $absolute.StartsWith($artifactRoot.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Fixture cleanup target escaped artifacts.' }
    if (Test-Path -LiteralPath $junction) { [IO.Directory]::Delete($junction) }
    if (Test-Path -LiteralPath $absolute) { Remove-Item -LiteralPath $absolute -Recurse -Force }
}
