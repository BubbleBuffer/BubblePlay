# Encoders

Current state, 2026-10-14 (lane/encoders, not yet on master).

The host sends captured frames to one of three video encoders. This page covers how the host picks one, what every backend must produce, and the CPU mode path.

## Backends

- **NVENC** is the NVIDIA SDK encoder. It is unchanged in this lane. It prefers HEVC and falls back to H.264.
- **Media Foundation hardware H.264** is the Windows hardware MFT. It is **experimental, untested on AMD/Intel hardware**. Only an NVIDIA MFT has been exercised.
- **Microsoft software H.264** is the inbox software MFT. It is the CPU mode encoder. It is also the auto-mode fallback when no hardware encoder starts.

Media Foundation backends offer H.264 only.

## Selection

Auto mode tries the backends in this order: NVENC, then MF hardware, then MF software. The first backend that starts wins.

- `--encoder nvenc`, `mf-hw` or `mf-sw` restricts selection to one backend. This is a diagnostic override.
- CPU mode restricts selection to MF software and skips all hardware. It does this even when an override is set.
- A backend that fails to start hands over to the next one in the order.

Discovery is separate from starting an encoder:

- `nvEncodeAPI64.dll` is delay-loaded ([`build.rs`](../../host/build.rs)). The host starts on PCs without NVIDIA drivers.
- A missing NVENC DLL is an ordinary selection failure. The host reports it and continues with the next backend.
- NVENC discovery checks the capture adapter's vendor. A non-NVIDIA adapter skips NVENC.
- Discovery runs once per capture adapter and is cached ([`video_encoder.rs`](../../host/src/video_encoder.rs)). Warm reconnects reuse the cached result.
- Discovery finishes before the first capture frame. The first offer does not wait for capture.

### Log lines

Selection writes one `info` line when an encoder starts. These are the formats:

| Situation | Line |
| --- | --- |
| NVENC on NVIDIA | `encoder: NVENC (<adapter>)` |
| MF hardware, NVIDIA | `encoder: Media Foundation hardware H.264, NVIDIA` |
| MF hardware, AMD or Intel | `encoder: Media Foundation hardware H.264, AMD (experimental, untested)` (or `Intel`) |
| Auto fallback (the VM) | `encoder: Microsoft software H.264; no hardware encoder: NVENC unavailable: LoadLibrary(nvEncodeAPI64.dll)…` |
| CPU mode | `encoder: Microsoft software H.264 (CPU mode forced by tray setting)` |
| Diagnostic override | `encoder: <backend> (diagnostic override)` |

On a non-NVIDIA adapter, the MF hardware line carries the NVENC reason after a semicolon, for example `…; NVENC unavailable: capture adapter is not NVIDIA`.

## Controls

- **Tray CPU mode toggle.** The tray menu item is "CPU mode". The setting is saved as `cpu_mode` in `%LOCALAPPDATA%\BubblePlay\tray.json`. A missing field means off. Toggling restarts the supervised host, and any connected viewer session ends. The supervisor passes `--cpu-mode` only when the setting is on ([`tray/logic.rs`](../../host/src/tray/logic.rs), [`tray/supervisor.rs`](../../host/src/tray/supervisor.rs)).
- **`--cpu-mode`.** The host flag for CPU mode. It forces MF software.
- **`--encoder auto|nvenc|mf-hw|mf-sw`.** The host flag for the diagnostic override. Other values are rejected.

## Bitstream contract

Every backend produces the same stream. Runtime validation checks each MF frame against these rules.

- H.264 High profile (`profile_idc` 100), Annex B byte stream.
- One access unit per frame.
- No B slices and no reordering.
- SPS and PPS before every IDR.
- IDR only when forced. The host uses an infinite GOP, so the encoder inserts no periodic IDR. An unsolicited IDR is an error.
- Limited (studio) range. Matrix is BT.601 when the current encoded size is 768 wide or less and 576 high or less. Larger sizes use BT.709. The native maximum size does not matter.
- The VUI is present, but `video_signal_type_present` is false. The stream carries no colour description. MF output matches NVENC's SPS, and the host does not rewrite the SPS.

Receivers follow the codec of each keyframe. The browser reads the codec from the keyframe NAL headers and reconfigures on a change (commit `5e52e0b`). See the [transport contracts](README.md#transports-and-contracts).

## CPU mode pipeline

On a WARP adapter, the frame path is:

1. DXGI Desktop Duplication delivers the BGRA frame. This is unchanged.
2. The host maps the BGRA frame from a staging texture. This is the CPU path.
3. One fused pass downscales (nearest neighbour, only when pressure or a cap applies) and converts BGRA to NV12. It uses AVX2 when the CPU has it, and scalar code otherwise. It uses two row scratch buffers and never builds a full scaled BGRA image.
4. The software MFT encodes the NV12 frame.

The NV12 buffer persists between frames. Only damaged rectangles are converted again. A union of damage covers dropped and rejected captures. Unknown metadata or a geometry change refreshes the whole frame.

### Kept and dropped choices

| Choice | Status | Reason |
| --- | --- | --- |
| Bypass the VideoProcessor on WARP | Kept, on | Setup is unsupported on WARP. The fallback is the same CPU path, so the timings show no separate gain. |
| AVX2 conversion | Kept, on | Exact integer arithmetic. Full conversion fell from 4.2 to 0.9 ms mean on VM 105. |
| Damage-aware conversion | Kept, on | Small dirty conversion fell from 1.0 to 0.04 ms mean on VM 105. Readback still copies the full BGRA frame. |
| Automatic MFT worker threads | Kept, software only | Total fell from 13.9 ms (1 thread) to 8.3 ms mean on VM 105. |
| Quality/speed 0 | Kept | Higher values cost more time. Quality/speed 100 fails the colour check. |
| CABAC on | Kept | CABAC off was 2.6% faster on mean (9.39 vs 9.64 ms) but slower at p95 (13.19 vs 12.89 ms). |
| Low latency on | Kept | Off fails the synchronous one-picture output contract. |
| Adaptive resolution, 720 then 540 | Kept | See the ladder below. |
| 30 fps fallback | Kept | Used after the lowest resolution rung. |
| Two row workers | Diagnostic only (`BP_CPU_CONVERT_THREADS=2`) | Thread start-up and contention cost more than they save on VM 105. |
| Pipeline (conversion and encode overlapped) | Dropped | Small dirty gain, worse tails. The shared device gate makes readback wait. |

### Adaptive ladder

- The ceiling is the requested height, capped by the source height and by `BP_CPU_MAX_HEIGHT`.
- The rungs are the native ceiling, then 720, then 540. A rung is used only when it is lower than the previous one.
- Heights are even. Aspect ratio is kept. The encoder never upscales.
- **Step down:** the average work per frame (EWMA, alpha 0.1) stays above 90% of the frame budget for 24 consecutive encoded frames. The last change must be at least 3 s old.
- **After the lowest rung:** the frame rate drops to 30 fps. This needs a requested rate above 30 and the fps fallback on.
- **Step up:** 180 consecutive frames stay below 60% of the budget for the next rung. The last change must be at least 10 s old. The fps cap lifts first, then the rung.
- Each change uses the existing IDR and transport admission path.
- Fused scaling and adaptation apply only to uncropped local software CPU conversion. Cropped and privileged paths keep the old preparation.

## Measurements

VM 105 is Windows 11 Pro 10.0.26300 on an i5-1135G7 with four logical cores. The probe runs release builds on WARP, with the software MFT at 6 Mbps. Each case uses 180 measured frames after 30 warmup frames, at a requested 60 fps.

Total is the sum of the per-frame stages, in milliseconds, as mean / p95:

| Case (1280x800 unless noted) | Before (scalar, 2 MFT threads) | After (final defaults) |
| --- | ---: | ---: |
| Full changes | 14.018 / 17.379 | 6.510 / 9.308 |
| Small dirty update (96x96 window) | 13.162 / 17.623 | 3.864 / 4.490 |
| Full changes at 720 | not measured | 6.988 / 9.580 |
| Full changes at 540 | not measured | 3.956 / 5.093 |

The "before" column uses the first matrix's scalar configuration. That run used the historical two MFT threads. The final defaults use automatic threads. The before-and-after difference therefore mixes conversion and thread-count changes.

The old release measured 12.581 / 17.963 ms on different content. Do not compare it with the rows above.

**Includes:** staging readback and map, CPU conversion (with scaling and damage), packing, and MFT input and output work.

**Excludes:** DDA acquisition, network, viewer decode and presentation, queue wait, and fixture generation.

The completion interval, which is not in the table, also includes texture upload and encoder recreation. The final default completion interval is 6.963 / 9.840 ms for full changes and 4.275 / 4.912 ms for dirty updates.

The paced run has a mean completion interval of 33.3 ms, with a p95 of 40.4 ms. The pacing is not flat.

Raw data is in `F:\build\encoder-task\cpu\`, outside the repository.

### End-to-end on VM 105

Measured 2026-10-14 on the release host. VM 105 has no GPU: its display adapter is Basic Display Adapter (WARP). The VM runs at 1280x800 on an i5-1135G7 with four cores. A headless Chrome viewer runs on another PC through an SSH tunnel to the VM's relay. The content is a timestamp test page, full screen in Edge on the VM. Each run lasts 60 s. Results are WSS unless noted.

| Config | Viewer fps mean (p5) | Latency mean / p95 (ms) | Host CPU mean (% of 4 cores) |
| --- | ---: | ---: | ---: |
| Auto selection (software MFT; adaptive lowered to 1152x720, then 864x540 at 30 fps) | 21.4 (19) | 24.7 / 53.4 | 4.0 |
| `--cpu-mode`, defaults | 21.2 (19) | 25.8 / 55.4 | 5.9 |
| `--cpu-mode`, old unoptimised path (`BP_CPU_SIMD=0 BP_CPU_DAMAGE=0 BP_CPU_ADAPTIVE=0 BP_CPU_MFT_THREADS=half`) | 21.7 (19) | 26.4 / 55.4 | 7.3 |
| `--cpu-mode`, fixed 540p30 (`BP_CPU_ADAPTIVE=0 BP_CPU_MAX_HEIGHT=540 BP_CPU_MAX_FPS=30`) | 21.5 (20) | 24.4 / 50.9 | 3.5 |
| `--cpu-mode`, WebRTC (WebCodecs) | 22.0 (receiver counter) | 38.7 / 86.5 | 3.6 |

The old unoptimised path reaches a VM total CPU p95 of 62%. The other configs stay at 28-33%.

On this VM, every config runs at about 22 fps. The source caps the rate. DXGI Desktop Duplication reports only 19-22 new presents per second, while the page renders at 64 rAF/s. Host admission, encoder, transport and viewer keep up with that capture rate with zero skips.

The cap comes from the VM's display stack, likely DWM on the Basic Display Adapter, not from the host. A GPU-less physical PC is unverified.

So on this VM the optimisations show up as lower CPU, not higher fps.

The WebRTC page metric reads about 13 fps. The measuring script's canvas sampling misses frames drawn by the WebCodecs worker. The receiver's own counter is the reliable number.

Rig note: ICE through the SSH tunnel can stay in "checking". This is a test-rig limitation, not a product one.

## Diagnostic toggles

The `BP_CPU_*` variables are read when the converter and encoder are built. The pacing variables are read when the host starts encoding. Restart the host after changing any of them. An unrecognised value logs a warning and uses the default.

| Variable | Default | Other value |
| --- | --- | --- |
| `BP_CPU_WARP_BYPASS` | `1` | `0`: try the VideoProcessor on WARP first |
| `BP_CPU_FORCE_CONVERSION` | `0` | `1`: force CPU conversion on any software-backend adapter |
| `BP_CPU_SIMD` | `1` | `0`: scalar conversion |
| `BP_CPU_DAMAGE` | `1` | `0`: convert the full frame every time |
| `BP_CPU_CONVERT_THREADS` | `1` | `2`: two row workers |
| `BP_CPU_MFT_THREADS` | unset (MFT default) | `1`, `2`, `4` or `half` (half the cores, clamped 1–8) |
| `BP_CPU_MFT_QUALITY_SPEED` | `0` | `0`–`100`. Values 33, 50 and 100 were measured. 100 fails the colour check. |
| `BP_CPU_MFT_CABAC` | `1` | `0`: CAVLC |
| `BP_CPU_MFT_LOW_LATENCY` | `1` | `0`: fails the output contract. Diagnostic only. |
| `BP_CPU_ADAPTIVE` | `1` | `0`: no pressure adaptation. The requested ceiling and fixed caps still apply. |
| `BP_CPU_ADAPTIVE_FPS` | `1` | `0`: resolution adapts, but the frame rate never drops |
| `BP_CPU_MAX_HEIGHT` | unset | Height cap, at least 2. Rounded down to even. |
| `BP_CPU_MAX_FPS` | unset | Frame-rate cap, at least 1. Never raises the requested rate. |

## Known limits

- **AMD and Intel MFTs are untested.** No physical AMD or Intel machine has run the hardware path. Windows N and a machine without MF are not exercised.
- **NVIDIA MFT is tested on one GPU.** The test GPU is a GTX 1660 SUPER.
- **Gradual intra refresh is not used.** The inbox software MFT accepts `CODECAPI_AVEncVideoGradualIntraRefresh` (SetValue returns `S_OK` and the value reads back), but it ignores the value. With values 0, 30 and 60 the streams are byte-identical, and there are no intra slices outside IDRs (`probe_mf --intra-refresh`, PC and VM 105). The host does not use it.
- **Privileged and cropped paths keep the old preparation.** They get no fused scaling and no adaptation.
- **The old SYSTEM-agent path (`privileged_backend.rs`) is NVENC-only.** It is a development path. Its IPC carries no encoder preference, so it does not follow CPU mode. The shipped secure helper hands BGRA frames to the normal host, which uses the selected encoder.
- **Quality/speed above 0 is not qualified.** At 100, the HD matrix error is about 87 code values. The cause is unresolved.
- **NVENC performance is not benchmarked in this lane.** NVENC's encode and config code is unchanged.
- **CPU mode is tested on a VM only.** The live host runs above were on VM 105, started with `--cpu-mode`. The tray toggle, a GPU-less physical PC and the protected desktop have not been tested live.

See [the architecture index](README.md) for the other pages.
