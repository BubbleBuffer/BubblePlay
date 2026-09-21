# Native monitor recovery — 2026-09-21

Stabilization checkpoints: `df40ec0` (runtime refactor) and `1adcd6e`
(native monitor ownership/recovery). The final live run passed ten complete
transport/monitor round trips and a fresh ten-minute native HEVC soak.
The separate elevated QoS cleanup receipt also passed, completing this gate.

## Implemented and reviewed

- Local monitor selection and inline VDD keepalive belong to the host serve
  loop and survive signaling sessions. Missing runtime-selected displays retry
  three times and then fail explicitly, without substituting another screen.
- Native monitor/source transitions recycle signaling before restarting the
  peer. Pending requests expire with their epoch; stale RTC stats are discarded.
  Input admission still requires fresh frames.
- Native readiness requires matching peer generations, presented/decoded HEVC,
  dimensions, current RTC statistics and fresh presentation. Native playback
  cannot enter source-barcode benchmarks or claim measured latency.
- Lab WSS navigation explicitly retains the v3 trace/topology contract. Slow
  reconnect capability discovery previously fell back to v2, whose packets
  lacked the trace trailers required by the lab. WSS always uses WebCodecs;
  the session retains its native preference for subsequent RTC navigation.
- RTC route telemetry follows the video transport's selected candidate-pair
  reference. The old nominated/succeeded filter could report no route while
  media continued. Legacy fallback remains for reports without that reference.
- Acceptance counts a cycle only after native monitor switching away/back and
  fresh, advancing WSS recovery. It records the candidate binary hash and full
  failure diagnostics. The pipe test tolerates PowerShell startup under load
  while directly asserting that the client remains alive when its line arrives.

## Evidence

Final run: `artifacts/native-hevc-114758ef-3a92-4a17-a083-3ed3f2b7adc1.json`,
completed `2026-09-21T20:41:54.227Z`.

- Ten complete WSS/native/monitor/WSS round trips passed.
- Fresh native soak: 600.45 seconds, 588 samples, 31.99 presented fps,
  maximum recorded frame gap 283 ms; no continuity failure or peer epoch reset.
- Owned receiver, source and candidate-host processes exited gracefully;
  helper stopped and the persistent VDD remained present.
- Sixty-second idle sample: zero mean encoder utilization and zero host memory
  delta. The original report remains `unsupported` because unelevated QoS
  inspection was unavailable; other cleanup evidence was available.
- Full checks passed: 550 Node tests and 389 Rust tests, plus the check command's
  syntax/all-target/whitespace validation. See
  `artifacts/stabilization-native-route-final-check.txt`.
- Release host SHA256:
  `1945082236ba39b36af8764d78307b6c3ae409ccf8a0ea41b895cda2568671bd`.
- Elevated read-only ActiveStore query at `2026-09-21T20:42:36.7103431Z`
  found no lab QoS policies and confirmed the helper stopped. Receipt:
  `artifacts/native-qos-114758ef-3a92-4a17-a083-3ed3f2b7adc1.json`.
  The combined verdict is complete; the original unelevated report is preserved.

Capture initially acquired zero frames on both physical and virtual outputs.
After authorized closure of the TeamViewer desktop process, direct capture
probes and live playback recovered. This temporal association does not prove
an underlying driver cause. The TeamViewer service remains running; its UAC
stop attempt was cancelled. Successful short reproduction:
`artifacts/native-hevc-3b3f5693-a456-4dda-a93d-ebb10358f116.json`.

Earlier run `26517d62-3c8b-4cf7-b335-1aedf992c40c` passed ten cycles but stopped
at 140 seconds when route telemetry disappeared despite advancing fresh frames.
It is not soak acceptance evidence. The selected-candidate-pair fix preceded
and passed the final fresh run above.

This validates the native recovery gate on this machine, not the broader
interruption matrix or an optimization/latency improvement. No push or
deployment was performed. Wiki capture tools were unavailable; this sourced
capsule remains local for later ingestion.
