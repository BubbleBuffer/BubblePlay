#Requires -Version 5.1
<#
.SYNOPSIS
    Tests the Flutter app mirroring of update-tray.ps1 (-AppOnly) against fake
    folders under $env:TEMP. Never touches the real install or any process.

.EXAMPLE
    pwsh -NoProfile -File host/scripts/test-update-tray-app.ps1
#>
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script = Join-Path $PSScriptRoot 'update-tray.ps1'
$root = Join-Path ([IO.Path]::GetTempPath()) ("bubbleplay-app-test-{0}" -f [Guid]::NewGuid().ToString('N'))
$release = Join-Path $root 'Release'
$install = Join-Path $root 'Install'
$failures = 0

function Assert-That([bool]$Condition, [string]$Message) {
    if ($Condition) { Write-Host "ok   $Message" }
    else { Write-Host "FAIL $Message"; $script:failures++ }
}

function Write-Text([string]$Path, [string]$Text) {
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Path) | Out-Null
    [IO.File]::WriteAllText($Path, $Text)
}

function Read-Text([string]$Path) { [IO.File]::ReadAllText($Path) }

function Invoke-AppSync {
    # Own process, like a real run; -NoStart is irrelevant to -AppOnly but harmless.
    $output = & pwsh -NoProfile -File $script -AppOnly -AppDir $release -InstallDir $install 2>&1
    if ($LASTEXITCODE -ne 0) { throw "update-tray.ps1 -AppOnly failed: $output" }
    return ($output | Out-String)
}

function Get-Snapshot {
    # relative path -> last write ticks, for "changes nothing" checks.
    $map = @{}
    foreach ($f in Get-ChildItem -LiteralPath $install -Recurse -File -Force) {
        $map[$f.FullName.Substring($install.Length + 1)] = "$($f.LastWriteTimeUtc.Ticks)/$($f.Length)"
    }
    return $map
}

try {
    # Fake Release folder, including a nested data folder.
    Write-Text (Join-Path $release 'bubbleplay_app.exe') 'app-v1'
    Write-Text (Join-Path $release 'flutter_windows.dll') 'engine-v1'
    Write-Text (Join-Path $release 'data\app.so') 'aot-v1'
    Write-Text (Join-Path $release 'data\flutter_assets\AssetManifest.bin') 'assets-v1'
    Write-Text (Join-Path $release 'old_plugin.dll') 'plugin-v1'

    # Foreign files that belong to the host/tray install.
    foreach ($name in 'bubbleplay-host.exe', 'bubbleplay_tray.exe', 'setup-tray.ps1', 'update-state.json') {
        Write-Text (Join-Path $install $name) "foreign-$name"
    }
    Write-Text (Join-Path $install 'previous\bubbleplay-host.exe') 'foreign-previous'
    Write-Text (Join-Path $install 'notes.txt') 'foreign-notes'

    # 1. First install copies everything and writes the manifest.
    Invoke-AppSync | Out-Null
    foreach ($relative in 'bubbleplay_app.exe', 'flutter_windows.dll', 'data\app.so', 'data\flutter_assets\AssetManifest.bin', 'old_plugin.dll') {
        Assert-That ((Test-Path -LiteralPath (Join-Path $install $relative)) -and ((Read-Text (Join-Path $install $relative)) -eq (Read-Text (Join-Path $release $relative)))) "first install copied $relative"
    }
    $manifestPath = Join-Path $install 'app-files.json'
    Assert-That (Test-Path -LiteralPath $manifestPath) 'manifest written'
    $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    Assert-That (@($manifest.files).Count -eq 5) 'manifest lists 5 files'

    # 2. Second run changes nothing.
    $before = Get-Snapshot
    Start-Sleep -Milliseconds 1100
    $output = Invoke-AppSync
    $after = Get-Snapshot
    $same = ($before.Count -eq $after.Count) -and (@($before.Keys | Where-Object { $before[$_] -ne $after[$_] }).Count -eq 0)
    Assert-That $same 'second run changed nothing'
    Assert-That ($output -match 'already current') 'second run reports already current'

    # 3. A changed file is replaced.
    Write-Text (Join-Path $release 'flutter_windows.dll') 'engine-v2'
    Invoke-AppSync | Out-Null
    Assert-That ((Read-Text (Join-Path $install 'flutter_windows.dll')) -eq 'engine-v2') 'changed file replaced'
    Assert-That ((Read-Text (Join-Path $install 'bubbleplay_app.exe')) -eq 'app-v1') 'unchanged file kept'
    Assert-That (-not (Test-Path -LiteralPath (Join-Path $install 'flutter_windows.dll.new'))) 'no temp file left'

    # 4. A file removed from the Release folder is removed (and its emptied folder).
    Remove-Item -Force -LiteralPath (Join-Path $release 'old_plugin.dll')
    Remove-Item -Recurse -Force -LiteralPath (Join-Path $release 'data\flutter_assets')
    Invoke-AppSync | Out-Null
    Assert-That (-not (Test-Path -LiteralPath (Join-Path $install 'old_plugin.dll'))) 'removed file removed'
    Assert-That (-not (Test-Path -LiteralPath (Join-Path $install 'data\flutter_assets'))) 'emptied folder removed'
    Assert-That (Test-Path -LiteralPath (Join-Path $install 'data\app.so')) 'sibling file kept'
    Assert-That (@((Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json).files).Count -eq 3) 'manifest lists 3 files'

    # 5. Foreign files are untouched throughout.
    foreach ($name in 'bubbleplay-host.exe', 'bubbleplay_tray.exe', 'setup-tray.ps1', 'update-state.json') {
        Assert-That ((Read-Text (Join-Path $install $name)) -eq "foreign-$name") "foreign $name untouched"
    }
    Assert-That ((Read-Text (Join-Path $install 'previous\bubbleplay-host.exe')) -eq 'foreign-previous') 'previous\ untouched'
    Assert-That ((Read-Text (Join-Path $install 'notes.txt')) -eq 'foreign-notes') 'unlisted file untouched'

    # 6. A Release file named like a host file is not mirrored over it.
    Write-Text (Join-Path $release 'bubbleplay_tray.exe') 'evil'
    Invoke-AppSync 3>&1 | Out-Null
    Assert-That ((Read-Text (Join-Path $install 'bubbleplay_tray.exe')) -eq 'foreign-bubbleplay_tray.exe') 'reserved name not overwritten'
    Remove-Item -Force -LiteralPath (Join-Path $release 'bubbleplay_tray.exe')

    # 7. A corrupt manifest removes nothing.
    [IO.File]::WriteAllText($manifestPath, '{not json')
    Remove-Item -Force -LiteralPath (Join-Path $release 'flutter_windows.dll')
    Invoke-AppSync 3>&1 | Out-Null
    Assert-That (Test-Path -LiteralPath (Join-Path $install 'flutter_windows.dll')) 'corrupt manifest: nothing removed'
}
finally {
    # Only the folder this test created.
    if ($root.StartsWith([IO.Path]::GetTempPath(), [StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $root)) {
        Remove-Item -Recurse -Force -LiteralPath $root -ErrorAction SilentlyContinue
    }
}

if ($failures -gt 0) { Write-Host "$failures check(s) failed."; exit 1 }
Write-Host 'All checks passed.'
