# BubblePlay worker lab

The signaling relay is now the Rust relay in [`relay-rs/`](../relay-rs/README.md). CUT-1 (2026-10-25) deleted the Go relay and the JavaScript Worker relay that this guide used to describe, with their deploy, browser-approval and device-ID sections. Set up the relay from [relay setup](../relay-rs/README.md).

This directory still holds the browser assets in `public/` and the lab and latency tools in `tools/`. The sections below cover those tools. Some of them still name the old Worker deployment (`wrangler`, `--worker-version-id`, `*.workers.dev` examples); check those against `relay-rs/` before you run them.

## Latency tests

The deterministic receiver tests have no third-party dependencies:

```powershell
npm test
```

The live runner occupies the room's single client slot, injects a 750 ms
main-thread stall, and writes a JSON report under `../artifacts/latency/`:

```powershell
$env:BUBBLEPLAY_TOKEN = "<TOKEN>"
npm run latency:live -- --url https://bubbleplay-relay.example.workers.dev/ --seconds 30
```

The production client uses Chromium's native WebRTC video decoder. The
software glass-to-glass barcode reader still belongs to the diagnostic
WebCodecs path, so select it explicitly when running that legacy benchmark.
Diagnostic WebRTC negotiates H.264; native WebRTC retains its HEVC preference.
The lab rejects HEVC benchmark overrides on the H.264 diagnostic decoder before
changing profiles. Its separate H.264 compatibility sample releases the override
and requires advancing, fresh presentation throughout the sampled route.
Physical panel scanout is not included, and diagnostic H.264 compatibility does
not establish native HEVC benchmark performance.

```powershell
npm run latency:bench -- --url https://bubbleplay-relay.example.workers.dev/ --decoder-path webcodecs --seconds 30
```

For a real phone/cellular run, add `profile=1` to the authenticated client URL.
The browser then relays one compact WebRTC/network snapshot per second to the
Rust host. Search the host log for `client diagnostics`; the profile contains
absolute loss/retransmit/PLI/freeze/jitter-buffer counters, selected candidate
types, decoder implementation, native presentation cadence, Android network
hints, and WebCodecs Worker state only when that diagnostic path is selected.
The token and local/remote IP addresses are never included in the profile
payload.

## Closed-loop stream lab

The stream lab drives a synthetic desktop on the Parsec virtual display,
records the actual browser output through Chrome DevTools, and scores each
candidate with VMAF, SSIM, PSNR, OCR accuracy, regional sharpness, temporal
artifacts, action-to-photon delay, host encode pressure, and network overhead.
It uses the normal BubblePlay host and Worker paths; benchmark overrides are
strict, leased for at most ten minutes, and automatically revert.

Prerequisites are Chrome, FFmpeg with `libvmaf`, Tesseract with English and
German language data, the Parsec virtual display driver, and the installed
`BubblePlayLabHelper` demand-start service. The installer grants the owning
Windows account query/start rights, so a campaign starts the helper through
SCM without a UAC prompt; the LocalSystem service exits after `end` or lease
expiry. A persistent Chrome profile remembers the BubblePlay browser login.

```powershell
npm run lab:preflight
npm run lab:manual -- --worker-version-id <immutable-version-uuid>
npm run lab:smoke -- --url https://bubbleplay-relay.example.workers.dev/
npm run lab:baseline -- --url https://bubbleplay-relay.example.workers.dev/
npm run lab:sweep -- --url https://bubbleplay-relay.example.workers.dev/
npm run lab:campaign -- --url https://bubbleplay-relay.example.workers.dev/
npm run lab:campaign -- --resume
npm run lab:compare
npm run lab:cleanup
npm run lab:diagnostics
```

`STREAM_LAB_LOCAL_BYPASS=1` is lab-only and must never be set on a relay
that other machines can reach. It lets `/lab-*` sockets and static assets skip
approval, login and the host credentials when the request addresses
`127.0.0.1`, `localhost` or `[::1]` and every client-address header present
(`CF-Connecting-IP`, `True-Client-IP`, `X-Real-IP`, `X-Forwarded-For`,
`Forwarded`) names a loopback address. Outside Cloudflare the hostname is the
client's own `Host` header, and a relay reached directly carries no such
header, so there the variable alone would open the lab routes to anyone.
It is unset by default; pass it only to a loopback-bound `wrangler dev`.

`lab:manual` opens the pinned local receiver and uninstalled candidate host for
hands-on use. It defaults to `png-quiescent-detail`, keeps the browser visible,
and never starts the synthetic source or automated cursor/zoom interaction
trials. Press Ctrl+C in its terminal to close the candidate session cleanly.
Remote runs require an immutable Worker version that is present in the active
deployment (a zero-percent candidate is sufficient). Pinned remote runs may
use `--transient-vdd`; the Lab Helper display lease does not weaken Worker
version or compatibility admission.

Results, captures, telemetry, and candidate manifests are written beneath
`../artifacts/stream-lab/`. Encoder, preset, AQ, VBV, GOP, and bitrate quality
screening is unshaped so transport congestion cannot bias codec ranking. The
separate shaped matrix reserves wire headroom for WSS parity or WebRTC
overhead. A full campaign checkpoints every candidate and can resume after an
interruption; it includes a ten-minute soak, deterministic decoder recovery,
2× sharp-viewport scoring, a production-adaptive 8→4→2→4→8 staircase with
120-second dwells (override with `--adaptive-dwell-seconds` for a fast smoke), WSS and
WebRTC Gaming regressions, H264 fallback, and sixty seconds of post-cleanup
idle telemetry. Campaign acceptance also proves the helper stopped, the transient
display and QoS policy disappeared, and both lab Chrome children exited.
`lab:cleanup` removes only expired local run directories;
helper-owned QoS, profile, and virtual-display state is reverted by `end` or
the helper lease watchdog.

`lab:diagnostics` is deliberately non-elevated. It snapshots service state and
bounded tails of the six fixed Broker, Agent, Transport, and host logs beneath
`../artifacts/service-diagnostics/`. The installer grants only the configured
lab owner read/traverse access to those logs; service configuration, secrets,
write access, and service-control privileges remain protected. Reconnect storms
are rate-limited in-process and legacy oversized rotations are pruned on start.

Native HEVC acceptance uses a visible receiver on a display separate from the
synthetic source. From the repository root, run:

```powershell
node worker/tools/lab/native-hevc-acceptance.mjs --candidate-host host/target/release/bubbleplay-host.exe
```

The default run performs ten complete WSS-to-native-to-WSS round trips, each
including native monitor switching away and back, then a fresh 600-second
native soak and 60 seconds of cleanup/idle sampling.
It checks the negotiated HEVC codec, current monitor selection, fresh video-frame
callbacks, and uninterrupted frame progression. It does not measure native
glass-to-glass latency; native playback is rejected by the source-barcode
benchmark entry point. The run writes a JSON report under `artifacts/` and
detailed telemetry under `artifacts/stream-lab/<run-id>/`.
On failure it also saves source/receiver screenshots for diagnosis.

Use `--cycles 1 --soak-seconds 5 --idle-seconds 1` for a short reproduction;
that is not the full acceptance gate. If ActiveStore QoS inspection is denied,
the report remains unsupported. After the run ends, an administrator can obtain
separate read-only cleanup evidence with
`./tools/lab/verify-lab-qos.ps1 -RunId <run-id>`; retain both reports.
