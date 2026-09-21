# Runtime stabilization follow-up — 2026-09-05

Local changes on `codex/stabilize-runtime`; no deployment.

## Confirmed defects and changes

- Diagnostic WebRTC used an H.264-only decoder while offering HEVC. Codec
  selection now accounts for the transport and decoder. Native WebRTC still
  prefers HEVC, and WSS keeps its HEVC-capable decoder.
- HEVC-only benchmark overrides cannot be measured with the diagnostic RTC
  decoder. The lab rejects this combination before changing session resources.
  A separate H.264 compatibility check releases the override and explicitly
  labels its result as compatibility evidence. It does not replace HEVC gates.
- Legacy `RTCEncodedVideoFrame.timestamp` uses RTP clock units, not microseconds.
  The fallback now converts 90 kHz to microseconds. Current Chromium supplied
  `metadata.rtpTimestamp`, so this latent bug was not the cause of the measured
  backlog. See the [W3C timestamp definition](https://www.w3.org/TR/2023/WD-webrtc-encoded-transform-20230428/).
- Windows timed channel waits rounded a requested 1 ms wait to about 15 ms.
  A local 100-wait probe measured 15.28 ms normally and 1.57 ms with a 1 ms timer
  period. Negotiated RTC epochs now acquire that period and release it through
  RAII on teardown; idle epochs do not acquire it. Timing is verified by direct
  measurements rather than a flaky wall-clock unit assertion. See
  [Microsoft's paired timer-period contract](https://learn.microsoft.com/en-us/windows/win32/api/timeapi/nf-timeapi-timebeginperiod).
- The host postponed already-due active RTP deadlines by another 4 ms. It now
  services them immediately while preserving the one-second idle backoff.
  The pinned str0m 0.21.0 pacer deliberately requests immediate/near-immediate
  timeout service between packet dequeues. A focused regression failed with the
  original delay and passed after the change.
- The H.264 fallback check could issue its control before a newly navigated WSS
  route started. It now waits for that route. Compatibility readiness rechecks
  the selected test monitor after signaling changes.
- The new diagnostic compatibility check requires an AVC decoder, an RTC route,
  advancing frame counters/timestamps, and a fresh final presentation. A single
  frame followed by a stall cannot produce a successful compatibility result.

## Live evidence and limits

The initial corrected-codec runs decoded H.264 but accumulated delivery delay.
Host telemetry admitted several Mbps while transmitting about 0.3 Mbps; deadlines
were often serviced about 15 ms late. Timer resolution alone increased delivery
to about 1.6 Mbps, which was still insufficient. Removing the extra active
deadline delay then produced a completed diagnostic run at 60.14 fps, software
latency p95 144 ms, and zero decoder resets, followed by WSS H.264 fallback at
29.99 fps. That run is retained under
`artifacts/stream-lab/7efbe2e2-45e1-48a6-b2aa-6cf252492ea4`.

Further transition runs exposed intermittent monitor selection/reconnection
failures. An attempted shortcut to confirm a switch from a refreshed monitor list
did not resolve them and was removed. Native HEVC presented some frames, but its
monitor-selection check failed; this is not a native-path acceptance pass.

The final bounded run is recorded in `artifacts/stabilization-codec-live.json`.
It completed with diagnostic H.264 WebRTC at 55.20 fps, latency p95 300 ms,
zero decoder resets, and subsequent WSS H.264 fallback at 29.96 fps and latency
p95 225 ms. This variation from the earlier sample is not evidence of a fixed
performance improvement. The helper stopped and all owned browser/host children
exited gracefully; the persistent display was preserved. QoS evidence remained
unavailable. The final run's detailed artifacts are under
`artifacts/stream-lab/5ad454c6-5b78-4317-93aa-3a2ca3ae2428`.
Earlier experimental reports that said `complete` before continuity validation
was added must not be treated as acceptance evidence; their worker traces show
stalls. The new check rejects that condition explicitly.

Optimization remains paused for native HEVC measurement support, repeatable
monitor recovery, QoS idle evidence, and the remaining interruption matrix.
The earlier ten-minute WSS soak predates this follow-up and is not a new soak of
the changed host. Independent review covered decoder selection, timestamp units,
timer ownership, active pacer scheduling, and stalled-sample rejection.

Final validation passed: `node tools/check.mjs full` (533 Node tests, 385 Rust
tests, JavaScript syntax checks, all-target Rust checks, and Git whitespace
checks). The release host build passed. Logs are retained as
`artifacts/stabilization-followup-check.txt` and
`artifacts/stabilization-codec-build.txt`. Three existing Rust library warnings
remain. Wiki write tools were unavailable; this repository note retains the
sourced outcome locally.
