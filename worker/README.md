# BubblePlay signaling worker

One Cloudflare Worker: static hosting of the browser client + a WebSocket
relay that pairs exactly one host with one client.

## Deploy

```powershell
wrangler login
wrangler secret put TOKEN             # host signaling registration secret
wrangler secret put HOST_GATE_TOKEN   # host endpoint bearer credential
wrangler secret put GATE_ADMIN_TOKEN  # approval management and password fingerprinting
wrangler secret put APP_PASSWORD      # browser application login
wrangler secret put NTFY_TOKEN        # approval notification service credential
wrangler deploy
```

## Local dev

```powershell
Copy-Item .dev.vars.example .dev.vars   # configure separate local credentials
wrangler dev
```

Open the local site in Chromium. Browser access uses approval and application
login cookies; a token query parameter does not authenticate the browser. The
approval flow also needs a reachable notification service configured through
`NTFY_URL`, `NTFY_TOPIC`, and `NTFY_TOKEN`. The host connects to `/ws` with its
registration secret and separate endpoint credential:

```powershell
bubbleplay-host.exe --mode serve --worker ws://localhost:8787/ws --token <TOKEN> --host-gate-token <HOST_GATE_TOKEN>
```

## Latency tests

The deterministic receiver tests have no third-party dependencies:

```powershell
npm test
```

The live runner occupies the room's single client slot, injects a 750 ms
main-thread stall, and writes a JSON report under `../artifacts/latency/`:

```powershell
$env:BUBBLEPLAY_TOKEN = "<TOKEN>"
npm run latency:live -- --url https://bubbleplay-signal.example.workers.dev/ --seconds 30
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
npm run latency:bench -- --url https://bubbleplay-signal.example.workers.dev/ --decoder-path webcodecs --seconds 30
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
npm run lab:smoke -- --url https://playfull-cat.your-subdomain.workers.dev/
npm run lab:baseline -- --url https://playfull-cat.your-subdomain.workers.dev/
npm run lab:sweep -- --url https://playfull-cat.your-subdomain.workers.dev/
npm run lab:campaign -- --url https://playfull-cat.your-subdomain.workers.dev/
npm run lab:campaign -- --resume
npm run lab:compare
npm run lab:cleanup
npm run lab:diagnostics
```

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
