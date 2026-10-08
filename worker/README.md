# BubblePlay signaling worker

`SOURCE_URL` defaults to `https://github.com/BubbleBuffer/BubblePlay` and is returned by `/__auth/methods` for the browser and Android About links. Operators of a modified relay must point it at their modified Corresponding Source, as required by AGPL-3.0-or-later. Set this Worker variable to an HTTP(S) source URL without embedded credentials.

One Cloudflare Worker: static hosting of the browser client + a WebSocket
relay that pairs exactly one host with one client.

The assets in `public/` are also used by the [Go relay](../relay/README.md). This README covers the Cloudflare implementation and its development lab; operator setup is in [Cloudflare self-hosting](../docs/self-host-cloudflare.md). See [architecture](../docs/architecture/README.md) for component boundaries and shared fixtures.

## Deploy

Run these commands from `worker/` after `npm ci`, using the pinned local Wrangler. Choose the browser approval methods described below.

```powershell
npx wrangler login
npx wrangler secret put TOKEN             # host signaling registration secret
npx wrangler secret put HOST_GATE_TOKEN   # host endpoint bearer credential
npx wrangler secret put GATE_ADMIN_TOKEN  # approval management and password fingerprinting
npx wrangler secret put APP_PASSWORD      # browser application login
npx wrangler secret put NTFY_TOKEN        # required when ntfy approval is enabled
npx wrangler deploy
```

`TOKEN`, `HOST_GATE_TOKEN` and `GATE_ADMIN_TOKEN` must be at least 32
characters and `APP_PASSWORD` at least 12. Until all four are, the relay
answers every request with a 500 configuration error that names the variables
to fix (never their values). Generate each token with 32 random bytes:

```powershell
node -e "console.log(require('crypto').randomBytes(32).toString('base64url'))"
```

`wrangler.toml` ships without anyone's infrastructure. The Worker deploys as
`bubbleplay-relay`; change `name` to deploy under another name.

### Browser approval methods

Set `APPROVAL_METHODS` in `wrangler.toml` to `ntfy`, `host`, or `ntfy,host`.
Leaving it unset preserves the existing `ntfy` default. Cloudflare does not
offer TOTP; the [Go relay](../relay/README.md) supports authenticator codes.
There is no password-only mode: a new browser first needs approval, then the
application password. Opening the site starts a held approval request directly.

When `ntfy` is enabled, configure `NTFY_URL` and `NTFY_TOPIC` to your own server
and store `NTFY_TOKEN` as a secret. `host` sends an Allow/Deny dialog over the
existing `/host-ws` connection to a Windows host that announced
`browserApproval: true`. Update the host before relying on this method. With
both enabled, the request goes to both at once; the first decision wins and
the remaining dialog/request is cancelled. Host approval proves someone is
present at the PC; anyone viewing that desktop can respond.

Host-only approval fails promptly when no capable host is connected; it does
not wait for the approval timeout. Reconnect the PC and reload the site to try
again. With both enabled, ntfy can still approve while the PC is unreachable.
An empty list, an unknown method, or missing `NTFY_*` configuration when ntfy
is enabled fails closed with a configuration error. Host-only needs no ntfy
configuration. The approval timeout and browser session defaults are unchanged.

`GET /__auth/methods` is public and returns
`{"totp":false,"approval":["ntfy","host"],"hostOnline":true}` for a deployment
offering both methods with a capable connected PC. The login page explains
where a new request goes and offers **Request approval instead**. It keeps the
password form for a browser that already passed approval.

Browser signaling, media and file sockets retain their browser session
identity and recheck approval and login validity. Incoming traffic uses a
short cache (about three seconds); idle sockets are checked by an alarm
scheduled every five seconds. Logout and revocation notify registered rooms
immediately. These are best-effort Durable Object timers, not a hard wall-clock
disconnect guarantee under runtime scheduling delays. Password rotation and
session expiry also invalidate an open connection on its next check.

The Cloudflare WebSocket API does not expose `bufferedAmount` or a send
completion signal. Message size and protocol limits do not establish a bounded
slow-consumer output queue; see the open [workerd API issue](https://github.com/cloudflare/workerd/issues/988).

WebSocket upgrades that carry an `Origin` header are accepted only from the
relay's own origin. The host and the Android app send no `Origin` header.

## Local dev

```powershell
Copy-Item .dev.vars.example .dev.vars   # configure separate local credentials
npx wrangler dev
```

Open the local site in Chromium. Browser access uses approval and application
login cookies; a token query parameter does not authenticate the browser. The
approval flow needs either a reachable ntfy service configured through
`NTFY_URL`, `NTFY_TOPIC`, and `NTFY_TOKEN`, or a capable connected host, according
to `APPROVAL_METHODS`. The host connects to `/host-ws` with its
registration secret and separate endpoint credential:

```powershell
bubbleplay-host.exe --mode serve --worker ws://localhost:8787/host-ws --token <TOKEN> --host-gate-token <HOST_GATE_TOKEN>
```

## TURN is opt-in

The browser only receives TURN credentials (and the TURN hostname) when the
page URL has `?turn=1` or `?ice=relay`. Otherwise WebRTC gets STUN only and
falls back to WSS if no direct route works. TURN is disabled unless the relay
has both `TURN_HOST` (your coturn server, `host` or `host:port`, port 3478 by
default) and the `TURN_SECRET` secret (its REST shared secret); without them
the opt-in still returns STUN only.

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
