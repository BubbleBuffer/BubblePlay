# BubblePlay

Windows desktop streaming: a Rust capture/encode host, a Cloudflare signaling
and media relay, and a Chromium receiver. See [worker setup](worker/README.md)
for authentication and the stream lab.

## Development checks

Use Node 22 or newer and stable Rust for Windows MSVC. Install Visual Studio C++
build tools and the NVIDIA Video Codec SDK, or generate local import libraries
with `host/scripts/make-nvenc-stubs.ps1` (requires the NVIDIA driver and lib.exe).
Set `NVIDIA_VIDEO_CODEC_SDK_PATH` to a real SDK to override `host/sdk-stub`.
Run `cargo fetch --locked --manifest-path host/Cargo.toml` and install the locked
Worker dependencies with `npm --prefix worker ci` once during setup.

From the repository root:

```powershell
node tools/check.mjs          # all deterministic checks, offline Rust resolution
node tools/check.mjs client   # Node tests and all JavaScript/inline-script syntax
node tools/check.mjs host     # Rust tests and checks across all targets
node tools/check.mjs syntax   # syntax only
```

The checker uses the current Node executable directly, so it does not require a
working global npm launcher. On the investigation machine that launcher pointed
to a missing user-profile npm installation; this is an environment setup issue.
Missing dependencies fail the offline check rather than triggering installation.

## Desktop tray deployment

`bubbleplay_tray.exe` replaces the BubblePlay Windows services. It runs as the
signed-in user and supervises `bubbleplay-host.exe --mode serve --worker <url>`
(local DXGI capture + SendInput, no privileged agent or broker). Tokens reach
the host only as `BUBBLEPLAY_TOKEN` / `BUBBLEPLAY_HOST_GATE_TOKEN` environment
variables.

- Config: `%LOCALAPPDATA%\BubblePlay\host.json` (`worker`, `tokenDpapiHex`,
  `hostGateTokenDpapiHex`, optional `monitor`; secrets CurrentUser DPAPI) and
  `tray.json` (`adminMode`, `startAtLogin`).
- Logs: `%LOCALAPPDATA%\BubblePlay\logs` (`tray.log`, `host.log`,
  `host-admin.log`), each capped at 5 x 5 MiB.
- One-time migration, elevated: `host/scripts/setup-tray.ps1` (use `-WhatIf`
  first). It copies the tokens out of `%ProgramData%\BubblePlay\transport.json`,
  installs into `%LOCALAPPDATA%\Programs\BubblePlay`, registers the on-demand
  `\BubblePlay\ElevatedHost` task, deletes the three services, sets the HKCU Run
  entry and starts the tray non-elevated. Program Files/ProgramData copies are
  kept for recovery.
- Updates, non-elevated: `host/scripts/update-tray.ps1 [-Build]`.
- Admin mode (tray menu, default off) runs the host through the elevated task,
  so input reaches elevated windows. The secure desktop (UAC prompts, lock
  screen) is still not captured. The task runs a binary from a user-writable
  folder; use `setup-tray.ps1 -SkipElevatedTask` if that trade-off is unwanted.

## Runtime boundaries

- `host/src/lib.rs` owns modules shared by services, probes, and the host binary.
- Binary-owned `host_*` modules separate capture/agent adapters, encoding,
  session/profile policies, and lab telemetry from the host event loop.
- The browser page keeps the early diagnostic bootstrap; `receiver.js` starts
  the session and uses the separately testable media ACK scheduler.
- The relay applies `protocol-validation.js` at its existing message boundaries.
- The lab lifecycle module owns lease renewal, checkpoints, and termination;
  the measurement module owns alignment, cohort readiness, and ranking policy.
  The live runner retains browser/process orchestration and measurement I/O.

`fixtures/wss-contract.json` is checked by both Rust and JavaScript. Existing
source-contract tests remain transitional; new lifecycle tests should import
the real controller rather than extract function text.

Candidate receiver overrides serve the local JavaScript set together. This keeps
new modules from falling through to an older deployed receiver during lab runs.
Preflight rejects a missing or unqueryable helper: live startup requires its
lease, and cleanup requires SCM access to verify shutdown, even without a
transient display.

## Stabilization acceptance

Deterministic checks do not prove Windows display/service or live decoder health.
Before further optimization, build an isolated candidate and use the stream lab
to compare with a same-machine baseline: WSS, WebRTC Gaming, H264 fallback,
relay/child recovery, ten-minute soak, then helper/display/QoS/browser cleanup
and idle recovery. Keep production deployment separate from candidate validation.
