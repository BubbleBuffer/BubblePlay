# Native monitor recovery — 2026-09-21

The prior stabilization work is checkpointed at `df40ec0`. Optimization remains
on hold: the new ten-cycle native acceptance and fresh ten-minute soak have not
passed.

## Implemented and reviewed

- Local monitor selection and inline VDD keepalive now belong to the host serve
  loop, surviving signaling sessions. A missing runtime-selected display is
  retried three times and then reported; capture never substitutes a screen.
  Privileged Agent ownership is unchanged.
- Native monitor/source transitions recycle signaling before restarting the
  peer. Pending monitor requests expire with their epoch. Monitor commands use
  signaling readiness; fresh-frame input admission is unchanged.
- The lab preserves native decoder selection across reconnects. Native evidence
  requires matching peer generations, presented/decoded HEVC frames, dimensions,
  current RTC statistics and fresh presentation. A stats poll finishing after a
  peer change is discarded. Nullable timestamps fail closed.
- WSS still uses its WebCodecs readiness contract when returning from native.
  Native playback cannot enter source-barcode benchmarks or claim latency.
- `worker/tools/native-hevc-acceptance.mjs` exercises monitor/transport changes,
  a native continuity soak and cleanup. Static alternate desktops require a new
  peer's presented frame; the synthetic-source monitor requires advancing frames.

## Evidence and remaining gate

Full checks passed (547 Node tests, 389 Rust tests, syntax/all-target/whitespace
checks), log `artifacts/stabilization-native-final-check.txt`. The final native
benchmark guard and WSS regression passed the focused 151-test suite in
`artifacts/stabilization-native-focused-check.txt`. Release host built successfully.

Post-fix short runs retain DISPLAY5 across WSS-to-native transitions, without the
previous repeated peer-left loop. They fail fresh-frame acceptance: the source
advances at roughly 56 fps, but capture supplies no new desktop frames and the
host replays cached IDRs. Source and receiver windows were verified on separate
displays. WSS capture counters likewise show zero acquisitions over hundreds of
producer iterations. Artifacts include runs `44b9b515-6a6d-444b-a545-836ea05386d5`,
`498ddf47-ee6f-4725-88aa-355c093e324e`, and
`cefae60c-6e00-44cf-a356-a0f184ef3466` (with diagnostic screenshots).

Direct three-second capture/NVENC probes, bypassing transport, exited 0 but
reported `frames_encoded=0 acquired=0 new_presents=0` on both DISPLAY5 and
DISPLAY1. See `artifacts/capture-probe-6e2c047c-9f2c-42b3-b777-b21188fe936f.txt`
and the corresponding `-physical.txt`. This establishes a capture-path failure
on both outputs; it does not prove its environmental cause. A TeamViewer session
popup is visible in the received image. Interference from another capture client
is a hypothesis, not a confirmed diagnosis. TeamViewer was not stopped.

All test-owned host/browser processes exited, helper stopped, and persistent VDD
was retained. Short idle samples are not the required 60-second acceptance run.
ActiveStore QoS inspection remains unverified: unelevated access was denied and
the Windows elevation request for `tools/verify-lab-qos.ps1` was cancelled.

Next: establish whether TeamViewer can be closed safely, repeat direct capture,
then run the full native acceptance command and obtain read-only elevated QoS
evidence. Do not treat connected sockets or cached-IDR playback as stability.

Wiki capture write tools were unavailable; this sourced task capsule is retained
locally for later memory ingestion. No deployment or push was performed.
