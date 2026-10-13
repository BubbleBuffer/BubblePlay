<#
.SYNOPSIS
Generates synthetic raw Annex-B H.264 and HEVC test streams for the decode-latency spike, then verifies them.

.DESCRIPTION
Uses only the installed ffmpeg and ffprobe (no downloads). The source is synthetic only, never a desktop or
camera capture. The exact lavfi graph is written to evidence.json (field "source_graph"):
  - testsrc2 at 1920x1080 and 60 fps (ffmpeg's built-in test pattern)
  - three STATIC drawbox panels (taskbar, app window, dialog). They do not animate.
  - one MOVING drawtext line, x = mod(n*4, width), where n is the frame number.
drawbox's t option is border thickness (t=fill here), not time, so drawbox never moves.

Encoders: h264_nvenc and hevc_nvenc with preset p1, tune ull, bf 0, GOP 120, CBR at 20 Mbps,
VBV of VbvMs (default 100 ms: bufsize = bitrate x VbvMs / 1000), no-scenecut 1, forced-idr 1, and AUDs on.
Average bitrate must fall within MinMbps..MaxMbps (default 18..22 Mbps), checked per stream.
Output: <OutDir>\h264.h264 and <OutDir>\hevc.hevc, exactly -Frames frames at -Fps.

Verification per stream:
  - ffprobe: codec, width, height, pixel format, decoded frame count (-count_frames).
  - Annex-B scan: AUD count (must equal the frame count), IDR count and the access-unit index of each IDR
    (must be every GOP, so 0, 120, ... 1080 for 1200 frames at g120).
  - Frame rate, from two independent sources, both reported separately in evidence:
      vui_frame_rate: H.264 time_scale / (2 * num_units_in_tick), HEVC time_scale / num_units_in_tick,
                      read from the SPS/VPS VUI with trace_headers.
      encoder_reported_fps: the output stream's fps from the ffmpeg encoder log.
    ffprobe's r_frame_rate is recorded but not used for pass/fail. A raw Annex-B file has no container
    timing, so ffprobe's value there is a heuristic.
Evidence is written to <OutDir>\evidence.json. The ffmpeg logs go to <ResultsDir>\make-streams-*.log.

Threads are capped at half the logical cores (default). Runs in the foreground and leaves no background processes.

.EXAMPLE
pwsh -File make-streams.ps1
#>
[CmdletBinding()]
param(
    [string]$OutDir = 'F:\build\spike-streams',
    [string]$ResultsDir = 'F:\build\spike-results',
    [int]$Width = 1920,
    [int]$Height = 1080,
    [int]$Fps = 60,
    [int]$Frames = 1200,
    [long]$BitrateBps = 20000000,
    [int]$VbvMs = 100,
    [double]$MinMbps = 18,
    [double]$MaxMbps = 22,
    [string[]]$Codecs = @('h264', 'hevc'),
    [int]$GopSize = 120,
    [int]$Threads = [math]::Max(1, [math]::Floor([Environment]::ProcessorCount / 2)),
    [string]$FontFile = 'C:\Windows\Fonts\arial.ttf'
)

$ErrorActionPreference = 'Stop'

foreach ($tool in 'ffmpeg', 'ffprobe') {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { throw "$tool not found on PATH" }
}
if (-not (Test-Path -LiteralPath $FontFile)) { throw "Font file not found: $FontFile" }

New-Item -ItemType Directory -Force -Path $OutDir, $ResultsDir | Out-Null

# VBV buffer = VbvMs of bitrate, in bits (100 ms at 20 Mbps = 2,000,000 bits).
$vbvBits = [long][math]::Round($BitrateBps * $VbvMs / 1000)
$expectedIdr = [int][math]::Ceiling($Frames / $GopSize)

# Source graph (no quotes, so no shell or argument-quoting surprises).
# Static panels: no expressions, so no escaping. The only expression is the moving drawtext x.
# Inside the moving expression the comma is escaped as \, and the font path's colon as \:.
$fontEsc = ($FontFile -replace '\\', '/') -replace ':', '\:'
$graph = "testsrc2=size=${Width}x${Height}:rate=$Fps," +
    "drawbox=x=0:y=$($Height - 40):w=${Width}:h=40:color=0x404040@0.9:t=fill," +
    "drawbox=x=120:y=120:w=640:h=400:color=0x2b579a@0.9:t=fill," +
    "drawbox=x=1000:y=300:w=500:h=300:color=white@0.9:t=fill," +
    "drawtext=fontfile=${fontEsc}:text=BubblePlay synthetic frame %{frame_num}:x=mod(n*4\,$Width):y=760:fontsize=56:fontcolor=white:box=1:boxcolor=black@0.5"

$specs = @(
    @{ Name = 'h264'; Encoder = 'h264_nvenc'; Muxer = 'h264'; File = 'h264.h264'; Codec = 'h264' },
    @{ Name = 'hevc'; Encoder = 'hevc_nvenc'; Muxer = 'hevc'; File = 'hevc.hevc'; Codec = 'hevc' }
)
$specs = @($specs | Where-Object { $Codecs -contains $_.Name })

function Get-AnnexBInfo {
    param([string]$Path, [string]$Codec)
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    # Emulation prevention guarantees 00 00 01 only occurs as a start code, so a Latin-1 regex scan is exact.
    $text = [System.Text.Encoding]::Latin1.GetString($bytes)
    $aud = 0; $nal = 0; $au = -1
    $idrAu = New-Object System.Collections.Generic.List[int]
    foreach ($m in [regex]::Matches($text, '\x00\x00\x01')) {
        $nal++
        $h = $bytes[$m.Index + 3]
        if ($Codec -eq 'h264') {
            $type = $h -band 0x1F
            $isAud = ($type -eq 9); $isIdr = ($type -eq 5)
        }
        else {
            $type = ($h -shr 1) -band 0x3F
            $isAud = ($type -eq 35); $isIdr = ($type -eq 19 -or $type -eq 20)
        }
        if ($isAud) { $aud++; $au++ }
        if ($isIdr) { $idrAu.Add($au) }
    }
    [pscustomobject]@{ Nal = $nal; Aud = $aud; IdrAu = @($idrAu); Bytes = $bytes.Length }
}

function Get-VuiTiming {
    param([string]$Path, [string]$Codec)
    $trace = & ffmpeg -hide_banner -loglevel verbose -i $Path -c copy -bsf:v trace_headers -frames:v 1 -f null - 2>&1 | Out-String
    $units = [regex]::Match($trace, '(?:vui_)?num_units_in_tick\s+\S+\s+=\s+(\d+)')
    $scale = [regex]::Match($trace, '(?:vui_)?time_scale\s+\S+\s+=\s+(\d+)')
    if (-not ($units.Success -and $scale.Success)) {
        return [pscustomobject]@{ TimeScale = $null; NumUnits = $null; Rate = $null; Formula = 'VUI timing not found' }
    }
    $n = [double]$units.Groups[1].Value; $t = [double]$scale.Groups[1].Value
    if ($Codec -eq 'h264') {
        [pscustomobject]@{ TimeScale = [long]$t; NumUnits = [long]$n; Rate = [math]::Round($t / (2 * $n), 3); Formula = 'time_scale / (2 * num_units_in_tick)' }
    }
    else {
        [pscustomobject]@{ TimeScale = [long]$t; NumUnits = [long]$n; Rate = [math]::Round($t / $n, 3); Formula = 'time_scale / num_units_in_tick' }
    }
}

$ffmpegVersion = (ffmpeg -version | Select-Object -First 1)
$evidence = [ordered]@{
    generated_utc = (Get-Date).ToUniversalTime().ToString('o')
    synthetic_only = $true
    source_graph = $graph
    source_notes = 'testsrc2 base; 3 static drawbox panels (t=fill is border thickness, not time); moving drawtext only (x=mod(n*4,W))'
    tool = $ffmpegVersion
    threads = $Threads
    expected = [ordered]@{
        width = $Width; height = $Height; fps = $Fps; frames = $Frames; gop = $GopSize
        idr_count = $expectedIdr
        idr_au_indices = @(0..($expectedIdr - 1) | ForEach-Object { $_ * $GopSize })
        bitrate_bps = $BitrateBps; vbv_ms = $VbvMs; vbv_bufsize_bits = $vbvBits
        bitrate_range_mbps = @($MinMbps, $MaxMbps); aud = $true
        no_scenecut = $true; forced_idr = $true
    }
    streams = @()
}
$allPass = $true

foreach ($s in $specs) {
    $out = Join-Path $OutDir $s.File
    $log = Join-Path $ResultsDir "make-streams-$($s.Name).log"
    if (Test-Path -LiteralPath $out) { Remove-Item -LiteralPath $out -Force }

    $ffArgs = @(
        '-hide_banner', '-nostdin', '-y', '-loglevel', 'info',
        '-threads', "$Threads", '-filter_threads', "$Threads",
        '-f', 'lavfi', '-i', $graph,
        '-frames:v', "$Frames", '-r', "$Fps",
        '-c:v', $s.Encoder, '-preset', 'p1', '-tune', 'ull', '-bf', '0',
        '-g', "$GopSize", '-no-scenecut', '1', '-forced-idr', '1',
        '-rc', 'cbr', '-b:v', "$BitrateBps", '-maxrate', "$BitrateBps", '-bufsize', "$vbvBits",
        '-pix_fmt', 'yuv420p', '-aud', '1',
        '-f', $s.Muxer, $out
    )
    Write-Host "Encoding $($s.File) with $($s.Encoder) ..."
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    & ffmpeg @ffArgs *> $log
    $encExit = $LASTEXITCODE
    $sw.Stop()

    # Encoder-reported rate: the output stream line in the ffmpeg log (input is wrapped_avframe).
    $logText = Get-Content -LiteralPath $log -Raw -ErrorAction SilentlyContinue
    $encRateMatch = [regex]::Matches($logText, "Video: $($s.Codec)\b[^\r\n]*?\b([\d.]+) fps") | Select-Object -Last 1
    $encoderFps = if ($encRateMatch) { [double]$encRateMatch.Groups[1].Value } else { $null }

    if (-not (Test-Path -LiteralPath $out)) {
        $allPass = $false
        $evidence.streams += [ordered]@{ file = $s.File; encoder = $s.Encoder; encoder_exit = $encExit; pass = $false; log = $log }
        Write-Host "  $($s.File): no output produced (encoder exit $encExit), see $log" -ForegroundColor Red
        continue
    }

    $probeJson = & ffprobe -hide_banner -v error -threads $Threads -count_frames -select_streams v:0 `
        -show_entries 'stream=codec_name,profile,width,height,pix_fmt,r_frame_rate,avg_frame_rate,nb_read_frames' `
        -of json $out
    $probeExit = $LASTEXITCODE
    $probed = if ($probeExit -eq 0 -and $probeJson) { ($probeJson -join "`n") | ConvertFrom-Json } else { $null }
    $st = if ($probed -and $probed.streams) { $probed.streams[0] } else { [pscustomobject]@{} }

    $info = Get-AnnexBInfo -Path $out -Codec $s.Codec
    $vui = Get-VuiTiming -Path $out -Codec $s.Codec
    $decoded = [int]$st.nb_read_frames
    $avgMbps = $info.Bytes * 8 / ($Frames / $Fps) / 1e6
    $expectedIdrAu = @(0..($expectedIdr - 1) | ForEach-Object { $_ * $GopSize })
    $checks = [ordered]@{
        encoder_exit_0      = ($encExit -eq 0)
        ffprobe_exit_0      = ($probeExit -eq 0)
        codec_name          = ($st.codec_name -eq $s.Codec)
        dimensions          = ($st.width -eq $Width -and $st.height -eq $Height)
        vui_frame_rate      = ($vui.Rate -eq $Fps)
        encoder_reported_fps = ($encoderFps -eq $Fps)
        decoded_frames      = ($decoded -eq $Frames)
        aud_per_frame       = ($info.Aud -eq $Frames)
        idr_count           = (@($info.IdrAu).Count -eq $expectedIdr)
        idr_cadence         = ((@($info.IdrAu) -join ',') -eq ($expectedIdrAu -join ','))
        bitrate_in_range    = ($avgMbps -ge $MinMbps -and $avgMbps -le $MaxMbps)
    }
    $failed = @($checks.GetEnumerator() | Where-Object { -not $_.Value } | ForEach-Object { $_.Key })
    $pass = ($failed.Count -eq 0)
    if (-not $pass) { $allPass = $false }

    $evidence.streams += [ordered]@{
        file = $s.File
        encoder = $s.Encoder
        ffmpeg_args = ($ffArgs -join ' ')
        bytes = $info.Bytes
        encode_seconds = [math]::Round($sw.Elapsed.TotalSeconds, 2)
        codec_name = $st.codec_name
        profile = $st.profile
        width = $st.width
        height = $st.height
        pix_fmt = $st.pix_fmt
        decoded_frames = $decoded
        nal_units = $info.Nal
        aud_count = $info.Aud
        idr_count = @($info.IdrAu).Count
        idr_au_indices = @($info.IdrAu)
        avg_mbps = [math]::Round($avgMbps, 2)
        vui_time_scale = $vui.TimeScale
        vui_num_units_in_tick = $vui.NumUnits
        vui_frame_rate = $vui.Rate
        vui_formula = $vui.Formula
        encoder_reported_fps = $encoderFps
        encoder_input_rate = $Fps
        ffprobe_r_frame_rate = $st.r_frame_rate
        ffprobe_r_frame_rate_note = 'raw Annex-B has no container timing; informational only, not used for pass/fail'
        failed_checks = $failed
        sha256 = (Get-FileHash -LiteralPath $out -Algorithm SHA256).Hash
        checks = $checks
        pass = $pass
    }
    Write-Host ("  {0}: decoded={1} aud={2} idr={3} {4}x{5} vui_fps={6} enc_fps={7} -> {8} {9}" -f $s.File, $decoded, $info.Aud, @($info.IdrAu).Count, $st.width, $st.height, $vui.Rate, $encoderFps, $(if ($pass) { 'PASS' } else { 'FAIL' }), ($failed -join ','))
}

$evidence.all_pass = $allPass
$evidencePath = Join-Path $OutDir 'evidence.json'
$evidence | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $evidencePath -Encoding utf8
Write-Host "Evidence: $evidencePath  all_pass=$allPass"
if (-not $allPass) { exit 1 }
