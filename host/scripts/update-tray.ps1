#Requires -Version 5.1
<#
.SYNOPSIS
    Non-elevated update of the BubblePlay per-user install (no UAC prompt
    unless -IncludeAgent).

.DESCRIPTION
    Copies bubbleplay-host.exe and bubbleplay_tray.exe from the build output
    into %LOCALAPPDATA%\Programs\BubblePlay, restarting the tray and its host.
    The replaced binaries are moved to <install>\previous\ (exactly one
    previous of each). Also refreshes the staged admin agent
    (bubbleplay_admin_agent.exe and bubbleplay_secure_helper.exe) and the setup/uninstall scripts beside them.
    Run setup-tray.ps1 once before the first update.

    The Flutter Devices app (bubbleplay_app.exe plus its dlls and data folder)
    is optional. When -AppDir (default app\build\windows\x64\runner\Release
    in the repo) holds bubbleplay_app.exe, its files are mirrored into
    <install> itself, because the tray looks for the app beside its own exe.
    <install>\app-files.json lists the mirrored files with sha256: changed
    files are replaced, files the previous manifest listed that the new build
    no longer has are removed, and nothing else in <install> is ever deleted.
    A running bubbleplay_app.exe from this install is closed first when its
    files change. -AppOnly does just this step.

    When the host changes, update-state.json in the tray data dir records it
    as "pending": the tray rolls back to previous\bubbleplay-host.exe if the
    new host does not register with signaling within 120 s or crash-loops, and
    marks it "good" after 10 healthy minutes. Honors BUBBLEPLAY_TRAY_DATA_DIR.

    The admin input agent in %ProgramFiles%\BubblePlay\agent is a separate,
    rarely changing component with a versioned pipe protocol: host updates
    never require it to change. When the staged agent differs from the
    installed one this script only reports it; -IncludeAgent installs it with
    one UAC prompt (setup-tray.ps1 -AgentOnly). If the host ever needs a newer
    agent protocol, the tray itself offers "Admin agent needs update (one
    admin prompt)" and injects at user level until then.

.EXAMPLE
    cargo build --offline --locked --release --bins --manifest-path host/Cargo.toml
    powershell -File host/scripts/update-tray.ps1
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string]$SourceDir,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\BubblePlay'),
    [string]$AgentDir,
    [string]$AppDir,
    [switch]$AppOnly,
    [switch]$Build,
    [Alias('IncludeElevated')]
    [switch]$IncludeAgent,
    [switch]$NoStart
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Rollback-tracked binaries (kept in previous\) and plain staged files.
$Binaries = @('bubbleplay-host.exe', 'bubbleplay_tray.exe')
$AgentExeName = 'bubbleplay_admin_agent.exe'
$HelperExeName = 'bubbleplay_secure_helper.exe'
$PrivilegedBinaries = @($AgentExeName, $HelperExeName)
$Scripts = @('setup-tray.ps1', 'uninstall-tray.ps1')
if (-not $SourceDir) { $SourceDir = Join-Path $PSScriptRoot '..\target\release' }
$SourceDir = [IO.Path]::GetFullPath($SourceDir)
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
if (-not $AgentDir) {
    $programFiles = if ($env:ProgramW6432) { $env:ProgramW6432 } else { $env:ProgramFiles }
    $AgentDir = Join-Path $programFiles 'BubblePlay\agent'
}
if (-not $AppDir) { $AppDir = Join-Path $PSScriptRoot '..\..\app\build\windows\x64\runner\Release' }
$AppDir = [IO.Path]::GetFullPath($AppDir)
$AppExeName = 'bubbleplay_app.exe'
$AppManifestName = 'app-files.json'
$TrayExe = Join-Path $InstallDir 'bubbleplay_tray.exe'

$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator) -and -not $NoStart -and -not $WhatIfPreference -and -not $AppOnly) {
    throw 'Run update-tray.ps1 from a normal (non-elevated) PowerShell so the tray does not start elevated, or pass -NoStart.'
}

function Get-Sha256([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    try {
        $sha = [Security.Cryptography.SHA256]::Create()
        try { return (($sha.ComputeHash($stream) | ForEach-Object { $_.ToString('X2') }) -join '') } finally { $sha.Dispose() }
    }
    finally { $stream.Dispose() }
}

function Get-Sha256OrNull([string]$Path) {
    try { if (Test-Path -LiteralPath $Path -PathType Leaf) { return Get-Sha256 $Path } } catch { }
    return $null
}

# Names in the install folder that belong to the host/tray install, never to the app.
$ReservedInstallNames = @($Binaries + $PrivilegedBinaries + $Scripts + @($AppManifestName, 'update-state.json'))

function Test-AppRelativePath([string]$Relative) {
    # A manifest or source path is only usable when it stays inside the install folder
    # and does not touch the host/tray files, update state or the previous\ folder.
    if ([string]::IsNullOrWhiteSpace($Relative) -or [IO.Path]::IsPathRooted($Relative)) { return $false }
    $parts = $Relative -split '[\\/]'
    if ($parts -contains '..' -or $parts -contains '.' -or $parts -contains '') { return $false }
    if ($parts[0] -ieq 'previous') { return $false }
    if ($parts.Count -eq 1 -and ($ReservedInstallNames -contains $parts[0])) { return $false }
    return $true
}

function Read-AppManifest([string]$ManifestPath) {
    # Missing or unreadable manifest means "nothing owned": no file is ever removed on a guess.
    $entries = @{}
    if (-not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) { return $entries }
    try {
        $doc = Get-Content -LiteralPath $ManifestPath -Raw | ConvertFrom-Json
        foreach ($file in @($doc.files)) {
            if ($file -and $file.path -and (Test-AppRelativePath ([string]$file.path))) { $entries[([string]$file.path).Replace('/', '\')] = [string]$file.sha256 }
        }
    }
    catch { Write-Warning "Ignoring unreadable $ManifestPath." }
    return $entries
}

function Stop-InstalledApp([string]$InstallRoot) {
    # Only the app running from this exact install folder; never another copy.
    $appPath = Join-Path $InstallRoot $AppExeName
    $running = @(Get-Process -Name ([IO.Path]::GetFileNameWithoutExtension($AppExeName)) -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -and ($_.Path -ieq $appPath) })
    foreach ($process in $running) {
        [void]$process.CloseMainWindow()
        if (-not $process.WaitForExit(10000)) {
            Write-Warning "bubbleplay_app.exe did not close in 10 s; stopping it."
            Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
            [void]$process.WaitForExit(5000)
        }
    }
}

# Mirrors the Flutter Release folder into the install folder, tracked by app-files.json.
# Returns the number of files added, replaced or removed.
function Sync-AppFiles {
    [CmdletBinding(SupportsShouldProcess = $true)]
    param([string]$Source, [string]$Target)

    $sourceRoot = [IO.Path]::GetFullPath($Source).TrimEnd('\')
    $targetRoot = [IO.Path]::GetFullPath($Target).TrimEnd('\')
    if (-not (Test-Path -LiteralPath (Join-Path $sourceRoot $AppExeName) -PathType Leaf)) { return 0 }

    $new = @{}
    foreach ($file in Get-ChildItem -LiteralPath $sourceRoot -Recurse -File -Force) {
        $relative = $file.FullName.Substring($sourceRoot.Length + 1)
        if (-not (Test-AppRelativePath $relative)) {
            Write-Warning "Not mirroring $relative (reserved name in the install folder)."
            continue
        }
        $new[$relative] = Get-Sha256 $file.FullName
    }
    $manifestPath = Join-Path $targetRoot $AppManifestName
    $old = Read-AppManifest $manifestPath

    $toCopy = @($new.Keys | Where-Object { (Get-Sha256OrNull (Join-Path $targetRoot $_)) -ne $new[$_] } | Sort-Object)
    $toRemove = @($old.Keys | Where-Object { -not $new.ContainsKey($_) -and (Test-Path -LiteralPath (Join-Path $targetRoot $_) -PathType Leaf) } | Sort-Object)
    $manifestStale = ($old.Count -ne $new.Count) -or @($new.Keys | Where-Object { -not $old.ContainsKey($_) -or $old[$_] -ne $new[$_] }).Count -gt 0
    if ($toCopy.Count -eq 0 -and $toRemove.Count -eq 0 -and -not $manifestStale) {
        Write-Host "Flutter app in $targetRoot is already current."
        return 0
    }
    if (-not $PSCmdlet.ShouldProcess($targetRoot, "Mirror Flutter app files ($($toCopy.Count) to write, $($toRemove.Count) to remove)")) { return 0 }

    if ($toCopy.Count -gt 0 -or $toRemove.Count -gt 0) { Stop-InstalledApp $targetRoot }
    New-Item -ItemType Directory -Force -Path $targetRoot | Out-Null
    foreach ($relative in $toCopy) {
        $destination = Join-Path $targetRoot $relative
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $destination) | Out-Null
        $temp = "$destination.new"
        Copy-Item -Force -LiteralPath (Join-Path $sourceRoot $relative) -Destination $temp
        Move-Item -Force -LiteralPath $temp -Destination $destination
        if ((Get-Sha256 $destination) -ne $new[$relative]) { throw "Hash mismatch after copying $relative." }
    }
    foreach ($relative in $toRemove) {
        $path = Join-Path $targetRoot $relative
        Remove-Item -Force -LiteralPath $path
        # Drop folders the removal emptied, but never the install folder itself.
        $dir = Split-Path -Parent $path
        while ($dir.Length -gt $targetRoot.Length -and $dir.StartsWith("$targetRoot\", [StringComparison]::OrdinalIgnoreCase) -and
            -not (Get-ChildItem -LiteralPath $dir -Force | Select-Object -First 1)) {
            Remove-Item -Force -LiteralPath $dir
            $dir = Split-Path -Parent $dir
        }
    }
    $listed = @($new.Keys | Sort-Object | ForEach-Object { [ordered]@{ path = $_; sha256 = $new[$_] } })
    $json = ([ordered]@{ version = 1; files = $listed } | ConvertTo-Json -Depth 4)
    [IO.File]::WriteAllText($manifestPath, $json, (New-Object Text.UTF8Encoding($false)))
    Write-Host "Flutter app: wrote $($toCopy.Count), removed $($toRemove.Count) file(s) in $targetRoot."
    return ($toCopy.Count + $toRemove.Count)
}

if ($AppOnly) {
    if (Test-Path -LiteralPath (Join-Path $AppDir $AppExeName) -PathType Leaf) { [void](Sync-AppFiles -Source $AppDir -Target $InstallDir) }
    else { Write-Host "No $AppExeName in $AppDir; nothing to mirror." }
    return
}

if ($Build) {
    $manifest = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\Cargo.toml'))
    if (-not $env:NVIDIA_VIDEO_CODEC_SDK_PATH) { $env:NVIDIA_VIDEO_CODEC_SDK_PATH = Join-Path $PSScriptRoot '..\sdk-stub' }
    if ($PSCmdlet.ShouldProcess($manifest, 'cargo build --release (host + tray + admin agent + secure-desktop helper)')) {
        # Cargo reads .cargo/config.toml from the working directory, not the manifest's, so build from host/.
        Push-Location (Split-Path $manifest)
        try { & cargo build --offline --locked --release --manifest-path $manifest --bin bubbleplay-host --bin bubbleplay_tray --bin bubbleplay_admin_agent --bin bubbleplay_secure_helper }
        finally { Pop-Location }
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed with $LASTEXITCODE" }
    }
}

$sources = @{}
foreach ($name in @($Binaries + $PrivilegedBinaries)) { $sources[$name] = Join-Path $SourceDir $name }
foreach ($name in $Scripts) { $sources[$name] = Join-Path $PSScriptRoot $name }
$changed = @()
foreach ($name in @($Binaries + $PrivilegedBinaries + $Scripts)) {
    $source = $sources[$name]
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "Missing $source (build first or pass -Build)." }
    if ((Get-Sha256OrNull (Join-Path $InstallDir $name)) -ne (Get-Sha256 $source)) { $changed += $name }
}
$staleHelpers = @($PrivilegedBinaries | Where-Object {
    (Get-Sha256OrNull (Join-Path $AgentDir $_)) -ne (Get-Sha256 $sources[$_])
})
if ($staleHelpers.Count -gt 0 -and -not $IncludeAgent) {
    Write-Host "Note: installed privileged helpers are missing or differ from this build: $($staleHelpers -join ', '). The tray checks protocol compatibility. Pass -IncludeAgent (one UAC prompt) or use the tray menu to install both."
}
$refreshAgent = $IncludeAgent # setup also checks/repairs task principal and DACL.
$dataDir = if ($env:BUBBLEPLAY_TRAY_DATA_DIR) { $env:BUBBLEPLAY_TRAY_DATA_DIR } else { Join-Path $env:LOCALAPPDATA 'BubblePlay' }
if (-not (Test-Path -LiteralPath (Join-Path $dataDir 'host.json'))) {
    Write-Warning 'host.json is missing: run setup-tray.ps1 once to write the host configuration.'
}
$binariesChanged = @($changed | Where-Object { $Binaries -contains $_ })
if ($changed.Count -eq 0) {
    Write-Host "Install in $InstallDir is already current."
}
elseif ($PSCmdlet.ShouldProcess($InstallDir, "Stop tray and replace $($changed -join ', ')")) {
    if ($binariesChanged.Count) {
        $wasRunning = [bool](Get-Process bubbleplay_tray -ErrorAction SilentlyContinue)
        if ($wasRunning -and (Test-Path -LiteralPath $TrayExe)) {
            # Graceful: the tray stops its host and the admin agent.
            $quit = Start-Process -FilePath $TrayExe -ArgumentList '--quit' -PassThru -Wait -WindowStyle Hidden
            if ($quit.ExitCode -ne 0) { Write-Warning 'Tray did not confirm shutdown in time.' }
        }
        $deadline = [DateTime]::UtcNow.AddSeconds(15)
        do {
            $busy = @(Get-Process bubbleplay_tray, bubbleplay-host -ErrorAction SilentlyContinue |
                    Where-Object { $_.Path -and $_.Path.StartsWith($InstallDir, [StringComparison]::OrdinalIgnoreCase) })
            if ($busy.Count -eq 0) { break }
            Start-Sleep -Milliseconds 300
        } while ([DateTime]::UtcNow -lt $deadline)
        if ($busy.Count -gt 0) {
            # Only our own non-elevated processes can be stopped here.
            $busy | Stop-Process -Force -ErrorAction SilentlyContinue
            Start-Sleep -Milliseconds 500
        }
    }
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $previousDir = Join-Path $InstallDir 'previous'
    New-Item -ItemType Directory -Force -Path $previousDir | Out-Null
    $hostPreviousHash = $null
    foreach ($name in $changed) {
        $target = Join-Path $InstallDir $name
        $temp = "$target.new"
        Copy-Item -Force -LiteralPath $sources[$name] -Destination $temp
        if ((Test-Path -LiteralPath $target) -and $Binaries -contains $name) {
            # Keep exactly one previous build of each binary.
            $previous = Join-Path $previousDir $name
            Move-Item -Force -LiteralPath $target -Destination $previous
            if ($name -eq 'bubbleplay-host.exe') { $hostPreviousHash = Get-Sha256 $previous }
        }
        # Pre-rollback-scheme leftovers.
        Remove-Item -Force -LiteralPath "$target.previous" -ErrorAction SilentlyContinue
        Move-Item -Force -LiteralPath $temp -Destination $target
        if ((Get-Sha256 $target) -ne (Get-Sha256 $sources[$name])) { throw "Hash mismatch after copying $name." }
    }
    if ($changed -contains 'bubbleplay-host.exe') {
        # The tray puts a pending host on probation and rolls back on failure.
        New-Item -ItemType Directory -Force -Path $dataDir | Out-Null
        $state = [ordered]@{
            status        = 'pending'
            newHash       = Get-Sha256 (Join-Path $InstallDir 'bubbleplay-host.exe')
            previousHash  = $hostPreviousHash
            updatedUnixMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
            reason        = $null
        }
        $json = $state | ConvertTo-Json
        [IO.File]::WriteAllText((Join-Path $dataDir 'update-state.json'), $json, (New-Object Text.UTF8Encoding($false)))
        if (-not $hostPreviousHash) { Write-Warning 'No previous host to roll back to; this update cannot be rolled back automatically.' }
    }
    Write-Host "Updated: $($changed -join ', ')"
}

# Flutter Devices app (optional): mirrored beside the tray exe.
if (Test-Path -LiteralPath (Join-Path $AppDir $AppExeName) -PathType Leaf) {
    [void](Sync-AppFiles -Source $AppDir -Target $InstallDir)
}
else {
    Write-Host "Note: no $AppExeName in $AppDir; the Flutter app is not installed (optional)."
}

if ($refreshAgent) {
    $setup = Join-Path $InstallDir 'setup-tray.ps1'
    if ($PSCmdlet.ShouldProcess($AgentDir, 'Check/install both staged privileged helpers (at most one UAC prompt: setup-tray.ps1 -AgentOnly)')) {
        $shell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
        $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$setup`"", '-AgentOnly',
            '-SourceDir', "`"$($InstallDir.TrimEnd('\'))`"", '-InstallDir', "`"$($InstallDir.TrimEnd('\'))`"")
        # Setup evaluates hashes, ACLs and task configuration first, then
        # elevates once only when machine changes are actually needed.
        $setupProcess = Start-Process -FilePath $shell -ArgumentList $arguments -Wait -PassThru -WindowStyle Hidden
        if ($setupProcess.ExitCode -ne 0) { throw "Agent install failed with exit code $($setupProcess.ExitCode); see setup-tray-machine-*.json in %LOCALAPPDATA%\BubblePlay\receipts." }
        Write-Host "Installed the admin agent and secure-desktop helper in $AgentDir."
    }
}

# Send To entry (file transfer), for installs made before it existed.
$SendToShortcut = Join-Path ([Environment]::GetFolderPath('SendTo')) 'BubblePlay.lnk'
if ((Test-Path -LiteralPath $TrayExe) -and -not (Test-Path -LiteralPath $SendToShortcut)) {
    if ($PSCmdlet.ShouldProcess($SendToShortcut, "Create Send To entry for $TrayExe --send-files")) {
        New-Item -ItemType Directory -Force -Path (Split-Path $SendToShortcut) | Out-Null
        $shell = New-Object -ComObject WScript.Shell
        $link = $shell.CreateShortcut($SendToShortcut)
        $link.TargetPath = $TrayExe
        $link.Arguments = '--send-files'
        $link.WorkingDirectory = $InstallDir
        $link.Description = 'Offer files to the connected BubblePlay viewer'
        $link.IconLocation = "$TrayExe,0"
        $link.Save()
        Write-Host "Created $SendToShortcut."
    }
}

if (-not $NoStart -and -not (Get-Process bubbleplay_tray -ErrorAction SilentlyContinue)) {
    if ($PSCmdlet.ShouldProcess($TrayExe, 'Start tray')) {
        if (-not (Test-Path -LiteralPath $TrayExe)) { throw "Tray not installed at $TrayExe; run setup-tray.ps1 first." }
        Start-Process -FilePath $TrayExe -WorkingDirectory $InstallDir
        Write-Host 'Tray started.'
    }
}
