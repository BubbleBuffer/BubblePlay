[CmdletBinding()]
param([Parameter(Mandatory)][string[]]$SpikeArgs,[string]$Log)
$ErrorActionPreference='Stop'
if(Test-Path -LiteralPath 'F:\build\spike-results\comparison-stop.request'){throw 'Scoped comparison stop requested; no next child launched'}
$target=Join-Path (Split-Path (Split-Path $PSScriptRoot -Parent) -Parent) 'target'
$exe=Join-Path $target 'release\decode-latency.exe'
$ffbin='F:\build\deps\ffmpeg-8.1-lgpl-shared\ffmpeg-n8.1-latest-win64-lgpl-shared-8.1\bin'
$start=[System.Diagnostics.ProcessStartInfo]::new($exe)
$start.UseShellExecute=$false
$start.CreateNoWindow=$true
$start.RedirectStandardOutput=$true
$start.RedirectStandardError=$true
# Only the child measurement process sees this DLL search path.
$start.Environment['PATH']="$ffbin;"+[Environment]::GetEnvironmentVariable('PATH','Process')
foreach($arg in $SpikeArgs){$start.ArgumentList.Add($arg)}
$process=[System.Diagnostics.Process]::Start($start)
if(-not $Log){$Log="F:\build\spike-results\launch-$($process.Id)-$([datetime]::UtcNow.Ticks).txt"}
$outFile=[System.IO.FileStream]::new("$Log.stdout.txt",[System.IO.FileMode]::Create,[System.IO.FileAccess]::Write,[System.IO.FileShare]::Read,4096,$true)
$errFile=[System.IO.FileStream]::new("$Log.stderr.txt",[System.IO.FileMode]::Create,[System.IO.FileAccess]::Write,[System.IO.FileShare]::Read,4096,$true)
$stdout=$process.StandardOutput.BaseStream.CopyToAsync($outFile)
$stderr=$process.StandardError.BaseStream.CopyToAsync($errFile)
try {
    $timedOut=-not $process.WaitForExit(120000)
    if($timedOut) {
        # This is the exact child started here, never an image-name cleanup.
        $process.Kill();if(-not $process.WaitForExit(5000)){throw "Own measurement child $($process.Id) failed to terminate after timeout"}
    }
    $null=$stdout.GetAwaiter().GetResult();$null=$stderr.GetAwaiter().GetResult()
    $outFile.Dispose();$errFile.Dispose()
    $output=[System.IO.File]::ReadAllText("$Log.stdout.txt")+[System.IO.File]::ReadAllText("$Log.stderr.txt")
    $output | Set-Content -LiteralPath $Log -Encoding utf8
    Write-Host $output
    if($timedOut){throw "Own measurement child $($process.Id) exceeded 120s; captured startup/progress in $Log"}
    if($process.ExitCode -ne 0){throw "Measurement child exited $($process.ExitCode)"}
} finally {$outFile.Dispose();$errFile.Dispose();$process.Dispose()}
