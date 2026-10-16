# Windows hardware decode latency spike

Standalone Rust/windows-crate tool: synthetic Annex-B AUs arrive at 60 Hz, decode through Media Foundation or FFmpeg d3d11va to same-device NV12 textures, and use the same D3D11 video-processor presenter. `DecodeBackend` owns submit/poll/drain and returns a texture with its sample/AVFrame lease. Presentation has no decoded-surface copy or CPU download.

The windowed swap chain uses FLIP_DISCARD, two buffers, ALLOW_TEARING, and a frame-latency waitable object at latency 1. Immediate is sync 0 plus ALLOW_TEARING; vsync is sync 1. Video processing/backing buffer are 1920x1080, scaled into a 1280x720 HWND. Each successful run exits its window and joins its arrival child. Keep the lean `arrival-producer.exe` companion beside the main executable; the default Cargo build produces both.

## Run

From this directory in PowerShell:

```powershell
$env:FFMPEG_DIR='F:\build\deps\ffmpeg-8.1-lgpl-shared\ffmpeg-n8.1-latest-win64-lgpl-shared-8.1'
./run-suite.ps1 -Build -Prefix unique-comparison -Runs 3
python ./analyze-results.py --summary summary-v2.csv --prefix unique-comparison --compare
python ./analyze-phases.py --summary summary-v2.csv --prefix unique-comparison
```

The suite verifies `client-win/target` remains an F: junction and builds only this package. It sets process-local Cargo/FFmpeg variables; `invoke-spike.ps1` puts FFmpeg's bin on PATH only for its bounded child. The C ABI shim compiles against installed headers and links avcodec/avutil: no handwritten FFmpeg struct layouts or global PATH changes.

```powershell
./invoke-spike.ps1 -SpikeArgs @('--list-decoders')
./invoke-spike.ps1 -SpikeArgs @('--timer-check','--max-frames','1200','--run','unique-timer')
./invoke-spike.ps1 -SpikeArgs @('--backend','ffmpeg','--codec','hevc','--input','F:\build\spike-streams\hevc.hevc','--present','immediate','--run','unique-single')
```

Use `--backend mf`, `--codec h264`, `--present vsync`, or `--max-frames 120` for other runs. `make-streams.ps1` generates synthetic content only; accepted fixtures were reused. Streams/evidence are in `F:\build\spike-streams`; CSVs, logs, raw DXGI stats, display snapshots, hashes, RGB checks, analyses and `summary-v2.csv` are in `F:\build\spike-results`. Existing run CSV names are refused. The suite alternates MF/FFmpeg for each codec/mode and repeats three times.

## What it measures

- A short-lived child paces arrivals with CreateWaitableTimerExW(HIGH_RESOLUTION), absolute 60 Hz QPC deadlines, process-local HighQoS/timer-resolution policy and MMCSS Pro Audio CRITICAL and a balanced process-local timeBeginPeriod(1) request. A ready/start handshake excludes initialization. Fixed shared-memory records publish actual wake timestamps through release/acquire atomics; no channel allocation, hot spin or artificial load. FIFO backlog is measured separately; no intentional jitter buffer or frame skipping.
- `t_submit` precedes compressed-packet allocation on the accepted submission attempt; arrival→submit backlog is separate from the latency tables. `t_output` follows output availability. `t_decoded` follows Context4.Signal, Flush and Fence.SetEventOnCompletion event wait on the shared immediate context. No timed probe copy or sleeping query poll. This observes all earlier shared-context work plus CPU/event wakeup, including possible prior presentation work; it is not an isolated decoder-engine hardware timestamp.
- `t_presented` is CPU return after Present, including video processing and the frame-latency wait. Jitter uses this endpoint. Startup is included; percentiles use nearest rank and SD is population SD. Late submissions exceed scheduled arrival by 1 ms; late frames exceed it by 16.667 ms. Application drops differ from display drops.
- DXGI frame/media stats and IDs are saved separately. Attempted vblank estimates are flagged when reference matching yields invalid latencies. Optical scanout, exact flips and display-drop counts remain unmeasured; sparse/missing stats do not establish drops.
- RGB checks run after timing and sample a synthetic swap-chain buffer. They check varied RGB content and record a sampled FNV hash; they are not an all-frame or pixel-perfect software-decoder reference. Buffer retention after FLIP_DISCARD is not portable, so this is an empirical check on this PC. No desktop capture.

## Configuration and caveats

GTX 1660 SUPER, driver 610.88, Windows 11 Home 10.0.26200, HEVC extension 2.5.33.0. Installed FFmpeg DLL self-version is `n8.1.3-18-g46d899d7a0-20261013` (package directory labelled 8.1); exact versions/hashes are recorded. FFmpeg wraps the presenter's device using av_hwdevice_ctx_alloc/init, verifies canonical immediate-context identity, requires D3D11/NV12/decoder binding and same-device surfaces, sets LOW_DELAY/thread_count 1/no frame threading, and rejects software fallback.

MF hardware-registered enumeration is empty here; installed synchronous D3D11-aware hybrid MFTs produce hardware NV12 decoder surfaces. Installed-MFT/profile checks and actual output checks are separate. H264 low latency uses ICodecAPI VT_UI4 readback 1; HEVC lacks ICodecAPI and uses the documented same-GUID MF_LOW_LATENCY IMFAttributes UINT32 route, readback 1. Accepted configuration does not prove its performance effect. Missing-HEVC PCs, other profiles and device-loss recovery are untested.

DisplayConfig identifies selected primary Dell S2419HGF (`DISPLAY1`) at **119.982 Hz**, Lenovo L22e-30 (`DISPLAY2`) at **74.973 Hz**, both NVIDIA, no clones. Global DWM snapshots have reported ~120 and ~75 Hz while the HWND output remains the Dell. The previous 75/120 discrepancy is consistent with different references on this mixed-refresh desktop; the exact cause of every historical DXGI statistic is unproved. DwmGetCompositionTimingInfo requires NULL HWND on modern Windows, so its rate is not labelled as the window's physical output.

Fixtures: 1920x1080, 60 fps, 1200 frames/AUs, NVENC p1/ull, B-frames 0, IDR every 120 frames, CBR ~20 Mbit/s, 100 ms VBV; testsrc2/static panels/moving text only. Normal PC activity is uncontrolled. No desktop capture, artificial CPU load, installed BubblePlay changes or detached measurement processes.

## Results

Authoritative fresh matrix: `compare-final-20261014`, harness `fence-v7-ipc`; 24 successful fresh processes, strict MF/FFmpeg alternation, three runs of each codec/mode/backend. All **28,800 frames** decoded/presented, zero decode errors, zero application drops, 1200 fence waits per run, zero GPU query polls, no timed probe copy. Every run passed the sampled final-buffer RGB check; hashes agree by codec (H264 `82ee7cf96f573393`, HEVC `84b304e76b46a5b2`). Independent raw-QPC recount verifies all frame indices, timestamp ordering and summary arithmetic. This proves sampled final-output agreement, not all-frame pixel equivalence.

Input timer p99 lateness was **0.608600–1.954700 ms**, under 1 ms in **22/24 runs**. The loaded-feed sub-1 ms acceptance check did not pass universally. Per-run arrival/FIFO distributions are in [compare-final-20261014-summary.md](F:/build/spike-results/compare-final-20261014-summary.md).

Standalone check: `timer HIGH_RESOLUTION + process HighQoS/no timer-resolution throttling + lean IPC producer MMCSS Pro Audio CRITICAL + process timer period1ms; frames=1200; median_ms=0.319400; p99_ms=1.765900; max_ms=3.590000; p99_under_1ms=false`

All times are milliseconds. CPU-present endpoint is Present return; every frame is included.

| Run | Frames | Submit→decode med/p95/p99/max | Decode→Present med/p95/p99/max | Submit→Present med/p95/p99/max | Interval SD/p99 | App drops | Submit late >1ms / frame late >16.67ms |
|---|---:|---|---|---|---|---:|---:|
| mf-h264-immediate-1 | 1200 | 4.762/7.473/10.352/99.546 | 0.285/0.934/1.622/12.778 | 5.105/7.821/10.728/112.324 | 2.813/21.785 | 0 | 430/26 |
| ffmpeg-h264-immediate-1 | 1200 | 5.007/10.210/51.594/1356.832 | 0.254/0.974/2.696/42.923 | 5.367/10.956/54.046/1367.996 | 12.922/71.081 | 0 | 537/305 |
| mf-h264-vsync-1 | 1200 | 4.294/7.121/9.326/88.124 | 0.226/0.814/3.570/9.565 | 4.588/7.738/10.969/97.689 | 1.695/21.062 | 0 | 302/18 |
| ffmpeg-h264-vsync-1 | 1200 | 4.464/8.743/47.872/912.782 | 0.265/4.533/7.251/83.726 | 5.325/10.649/48.675/926.347 | 11.742/64.853 | 0 | 527/301 |
| mf-hevc-immediate-1 | 1200 | 3.054/6.154/30.984/103.932 | 0.189/1.003/1.828/25.596 | 3.305/7.397/31.219/105.096 | 8.592/62.327 | 0 | 330/101 |
| ffmpeg-hevc-immediate-1 | 1200 | 2.873/8.036/49.448/1000.273 | 0.153/0.902/2.182/30.454 | 3.098/8.903/49.660/1007.453 | 18.434/81.530 | 0 | 455/240 |
| mf-hevc-vsync-1 | 1200 | 3.031/9.402/97.564/772.602 | 1.016/7.352/11.733/80.021 | 7.069/15.875/97.895/852.624 | 25.585/136.999 | 0 | 773/624 |
| ffmpeg-hevc-vsync-1 | 1200 | 3.155/7.056/52.344/1275.377 | 1.559/6.800/8.510/67.964 | 7.099/10.878/71.713/1292.786 | 19.375/93.518 | 0 | 744/608 |
| mf-h264-immediate-2 | 1200 | 4.108/7.176/11.092/75.715 | 0.199/0.746/1.326/83.571 | 4.376/7.594/11.625/159.286 | 2.917/28.683 | 0 | 319/32 |
| ffmpeg-h264-immediate-2 | 1200 | 3.131/8.870/22.981/3504.832 | 0.186/1.131/2.305/48.041 | 3.576/9.660/25.076/3513.973 | 19.059/46.216 | 0 | 594/398 |
| mf-h264-vsync-2 | 1200 | 2.273/4.848/7.401/67.742 | 0.160/0.804/4.232/18.293 | 2.613/5.395/8.598/78.966 | 2.597/26.686 | 0 | 333/20 |
| ffmpeg-h264-vsync-2 | 1200 | 4.698/10.031/89.127/2009.855 | 0.466/6.232/7.801/147.086 | 7.196/11.916/90.525/2020.697 | 16.412/109.607 | 0 | 757/586 |
| mf-hevc-immediate-2 | 1200 | 2.956/9.446/107.219/222.741 | 0.211/0.966/2.013/8.569 | 3.275/9.874/107.410/223.021 | 22.692/147.293 | 0 | 537/332 |
| ffmpeg-hevc-immediate-2 | 1200 | 3.130/9.318/38.466/1868.559 | 0.211/1.109/2.532/18.628 | 3.376/9.935/38.714/1879.739 | 19.582/95.249 | 0 | 626/364 |
| mf-hevc-vsync-2 | 1200 | 2.960/8.800/83.246/300.063 | 0.247/7.830/13.827/52.864 | 3.704/14.772/86.277/300.595 | 18.118/101.810 | 0 | 581/357 |
| ffmpeg-hevc-vsync-2 | 1200 | 3.167/13.496/74.328/1126.109 | 0.537/10.442/23.023/261.512 | 6.677/19.730/85.097/1138.141 | 19.882/108.678 | 0 | 842/675 |
| mf-h264-immediate-3 | 1200 | 4.094/6.666/7.598/60.558 | 0.198/0.645/1.029/6.965 | 4.347/6.893/8.319/67.523 | 1.899/22.513 | 0 | 273/10 |
| ffmpeg-h264-immediate-3 | 1200 | 4.601/9.142/68.297/737.185 | 0.217/0.917/2.258/92.375 | 4.879/9.840/83.900/776.499 | 17.262/97.032 | 0 | 526/282 |
| mf-h264-vsync-3 | 1200 | 4.628/9.739/129.846/227.490 | 0.340/6.351/7.510/22.188 | 5.628/11.989/131.208/227.662 | 18.633/138.195 | 0 | 611/412 |
| ffmpeg-h264-vsync-3 | 1200 | 3.549/12.674/123.063/874.424 | 2.901/8.174/18.757/301.117 | 8.037/17.983/149.445/874.718 | 48.332/204.707 | 0 | 958/850 |
| mf-hevc-immediate-3 | 1200 | 3.194/10.414/145.513/460.797 | 0.235/1.230/3.673/229.880 | 3.487/11.271/157.338/461.303 | 35.106/189.172 | 0 | 754/542 |
| ffmpeg-hevc-immediate-3 | 1200 | 3.090/8.916/135.946/391.674 | 0.225/1.014/2.854/25.335 | 3.355/10.187/152.658/393.019 | 29.499/164.066 | 0 | 612/396 |
| mf-hevc-vsync-3 | 1200 | 3.520/12.116/225.107/977.136 | 4.664/8.397/20.510/412.463 | 8.374/20.735/254.087/1087.509 | 42.542/281.615 | 0 | 1195/1188 |
| ffmpeg-hevc-vsync-3 | 1200 | 3.500/14.197/138.940/1457.562 | 2.220/8.118/14.715/161.519 | 7.916/19.065/141.409/1475.101 | 24.846/156.090 | 0 | 912/789 |

Pooled comparison, all 3600 frames (three fresh-process runs) per backend/codec/present mode; no startup exclusion. Values are median / p99 milliseconds. No backend selection is implied.

| Codec | Present | MF submit→decoded | FFmpeg submit→decoded | MF submit→presented | FFmpeg submit→presented |
|---|---|---|---|---|---|
| h264 | immediate | 4.431/9.814 | 4.618/47.517 | 4.712/10.635 | 4.914/54.046 |
| h264 | vsync | 4.126/22.091 | 4.421/73.708 | 4.528/23.885 | 7.134/85.685 |
| hevc | immediate | 3.065/102.109 | 3.037/91.267 | 3.338/107.116 | 3.281/91.578 |
| hevc | vsync | 3.160/145.295 | 3.232/93.340 | 7.464/157.077 | 7.470/106.245 |

HEVC locations below are zero-based AU/frame indices; all durations are submit→decoded milliseconds. “Later” searches frames 120–1199 only as a location diagnostic; no frames are excluded from the latency tables. Full >16.667 ms stall lists, including phase durations, are saved as per-run `-decode-stalls.csv`; exact frame 1050 and the surrounding 1040–1060 window are shown to test recurrence of the accepted MF spike.

| HEVC run | Worst overall frame: ms | Worst later frame: ms | Later before output / fence ms | Frame 1050 ms | Worst 1040–1060 frame: ms |
|---|---:|---:|---:|---:|---:|
| mf-hevc-immediate-1 | 467: 103.932 | 467: 103.932 | 1.422/102.510 | 1.516 | 1060: 4.907 |
| ffmpeg-hevc-immediate-1 | 0: 1000.273 | 478: 411.487 | 256.872/154.615 | 3.168 | 1057: 3.852 |
| mf-hevc-vsync-1 | 0: 772.602 | 1078: 407.109 | 174.994/232.115 | 2.030 | 1041: 9.960 |
| ffmpeg-hevc-vsync-1 | 0: 1275.377 | 584: 347.449 | 0.460/346.989 | 1.778 | 1041: 4.513 |
| mf-hevc-immediate-2 | 352: 222.741 | 352: 222.741 | 95.874/126.867 | 1.519 | 1051: 10.685 |
| ffmpeg-hevc-immediate-2 | 0: 1868.559 | 706: 310.084 | 304.479/5.606 | 1.813 | 1046: 5.903 |
| mf-hevc-vsync-2 | 1101: 300.063 | 1101: 300.063 | 194.271/105.792 | 234.960 | 1050: 234.960 |
| ffmpeg-hevc-vsync-2 | 0: 1126.109 | 285: 302.060 | 301.101/0.960 | 4.093 | 1046: 37.410 |
| mf-hevc-immediate-3 | 667: 460.797 | 667: 460.797 | 265.687/195.110 | 5.741 | 1049: 165.654 |
| ffmpeg-hevc-immediate-3 | 595: 391.674 | 595: 391.674 | 0.784/390.890 | 2.837 | 1046: 5.093 |
| mf-hevc-vsync-3 | 0: 977.136 | 1103: 513.139 | 311.357/201.782 | 3.736 | 1053: 225.107 |
| ffmpeg-hevc-vsync-3 | 0: 1457.562 | 1171: 262.250 | 109.067/153.183 | 1.766 | 1054: 2.970 |

Near-frame-1050 stalls recurred intermittently in MF HEVC: immediate run 3 at frame 1049 (165.654 ms), vsync run 2 at 1050 (234.960 ms), and vsync run 3 at 1053 (225.107 ms). Exact frame 1050 ranged 1.516–234.960 ms for MF and 1.766–4.093 ms for FFmpeg. FFmpeg's one >16.667 ms stall in the 1040–1060 window was vsync run 2 at 1046 (37.410 ms). The earlier MF 613.245 ms stall at exactly 1050 did not repeat at that magnitude or in every run. Stalls also occurred at many other frame indices; the table and complete lists preserve those observations without attributing a cause.

Normal PC activity is uncontrolled. Pre-run available physical memory in this matrix ranged **79.4–5708.4 MiB**; earlier setup/gates observed only 1–8 MiB and unusually long process/build delays. No unrelated applications were stopped. Host-observed before-output and fence-wakeup intervals do not by themselves identify CPU, GPU or driver root cause. Immediate/vsync distributions are CPU Present-return measurements; actual flip/scanout latency and display drops remain unknown because raw DXGI reference matching is sparse or unreliable.

Retained diagnostic failures are outside this fresh matrix: v3 READY timeout; old 65-second outer timeout; v5 MF HEVC decode-fence timeout at frame 651; v6 MF HEVC immediate run 2 presented all 1200 frames but required arrival-child forced termination after its three-second cleanup limit and had no RGB result. Final cleanup allows 20 seconds outside timing, still fails on forced termination, and targets only the owned child. Older `mf-paced-20261013`, `fencev2-`, `compare-fence-20261013`, `compare-ipc-fence-20261014` and `compare-ipc-long-20261014` datasets are preserved and not pooled here.

References: [timer](https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-createwaitabletimerexw), [process timer resolution](https://learn.microsoft.com/en-us/windows/win32/api/timeapi/nf-timeapi-timebeginperiod), [Signal ordering](https://learn.microsoft.com/en-us/windows/win32/api/d3d11_3/nf-d3d11_3-id3d11devicecontext4-signal), [fence event](https://learn.microsoft.com/en-us/windows/win32/api/d3d11_3/nf-d3d11_3-id3d11fence-seteventoncompletion), [DWM scope](https://learn.microsoft.com/en-us/windows/win32/api/dwmapi/nf-dwmapi-dwmgetcompositiontiminginfo), [MF decode](https://learn.microsoft.com/en-us/windows/win32/medfound/supporting-direct3d-11-video-decoding-in-media-foundation), [MF_LOW_LATENCY](https://learn.microsoft.com/en-us/windows/win32/medfound/mf-low-latency), [FFmpeg context](https://github.com/FFmpeg/FFmpeg/blob/n8.1/libavutil/hwcontext_d3d11va.c), [flip statistics](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/dxgi-flip-model).

