[CmdletBinding()]
param([string]$Prefix = (Get-Date -Format 'yyyyMMdd-HHmmss'),[int]$Runs=3,[string[]]$Backends=@('mf','ffmpeg'),[switch]$Build,[datetime]$DeadlineUtc=[datetime]::MaxValue)
$ErrorActionPreference='Stop'
$spike=$PSScriptRoot
$client=Split-Path (Split-Path $spike -Parent) -Parent
$target=Join-Path $client 'target'
$junction=Get-Item -LiteralPath $target
if ($junction.LinkType -ne 'Junction' -or $junction.Target -notlike 'F:\*') { throw "Target must remain an F: junction: $target" }
$env:CARGO_TARGET_DIR=$target
$env:CARGO_BUILD_JOBS='4'
$ffroot='F:\build\deps\ffmpeg-8.1-lgpl-shared\ffmpeg-n8.1-latest-win64-lgpl-shared-8.1'
$env:FFMPEG_DIR=$ffroot
if($Runs -lt 1 -or $Prefix -notmatch '^[a-zA-Z0-9_-]+$'){throw 'invalid suite arguments'}
foreach($backend in $Backends){if($backend -notin @('mf','ffmpeg')){throw "Invalid backend $backend"}}
if ($Build) {
    & cargo build --release --manifest-path (Join-Path $spike 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw 'Cargo build failed' }
}
$exe=Join-Path $target 'release\decode-latency.exe'
$arrivalExe=Join-Path $target 'release\arrival-producer.exe'
if(-not(Test-Path -LiteralPath $arrivalExe)){throw 'Build this package with cargo build --release to include arrival-producer.exe'}
$results='F:\build\spike-results'
New-Item -ItemType Directory -Force $results | Out-Null
& (Join-Path $spike 'invoke-spike.ps1') -SpikeArgs @('--list-decoders') -Log (Join-Path $results "$Prefix-decoders.txt")
@{
    utc=(Get-Date).ToUniversalTime().ToString('o')
    os=(Get-CimInstance Win32_OperatingSystem | Select-Object Caption,Version,BuildNumber,FreePhysicalMemory,TotalVisibleMemorySize)
    gpu=(& nvidia-smi --query-gpu=name,driver_version --format=csv,noheader)
    hevc_extension=(Get-AppxPackage '*HEVC*' | Select-Object Name,Version)
    target=$target
    junction_target=$junction.Target
    executable_sha256=(Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash
    arrival_executable_sha256=(Get-FileHash -LiteralPath $arrivalExe -Algorithm SHA256).Hash
    ffmpeg_dir=$ffroot
    ffmpeg_dll_hashes=(Get-FileHash -LiteralPath (Join-Path $ffroot 'bin\avcodec-62.dll'),(Join-Path $ffroot 'bin\avutil-60.dll') | Select-Object Path,Hash)
    stream_hashes=(Get-FileHash -LiteralPath 'F:\build\spike-streams\h264.h264','F:\build\spike-streams\hevc.hevc' -Algorithm SHA256 | Select-Object Path,Hash)
} | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $results "$Prefix-environment.json")
Copy-Item -LiteralPath 'F:\build\spike-streams\evidence.json' -Destination (Join-Path $results "$Prefix-stream-evidence.json")
for ($run=1; $run -le $Runs; $run++) {
    foreach ($codec in 'h264','hevc') {
        foreach ($mode in 'immediate','vsync') {
            $ext=if ($codec -eq 'h264') {'h264'}else{'hevc'}
            foreach($backend in $Backends) {
                if([datetime]::UtcNow.AddSeconds(120) -gt $DeadlineUtc.ToUniversalTime()){throw 'Suite budget deadline reached; no next child launched'}
                $name="$Prefix-$backend-$codec-$mode-$run"
                $memory=Get-CimInstance Win32_OperatingSystem
                [pscustomobject]@{run=$name;utc=(Get-Date).ToUniversalTime().ToString('o');available_physical_kib=$memory.FreePhysicalMemory;total_physical_kib=$memory.TotalVisibleMemorySize} |
                    Export-Csv -LiteralPath (Join-Path $results "$Prefix-memory.csv") -Append -NoTypeInformation
                Add-Content -LiteralPath (Join-Path $results "$Prefix-order.txt") -Value $name
                & (Join-Path $spike 'invoke-spike.ps1') -SpikeArgs @('--backend',$backend,'--codec',$codec,'--input',"F:\build\spike-streams\$codec.$ext",'--present',$mode,'--run',$name) -Log (Join-Path $results "$name-console.txt")
            }
        }
    }
}
Import-Csv (Join-Path $results 'summary-v2.csv') | Where-Object run -like "$Prefix-*" |
    Format-Table run,presented,sd_median,sd_p99,dp_median,sp_median,sp_p99,interval_stddev,interval_p99,status
