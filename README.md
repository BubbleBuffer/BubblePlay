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

BubblePlay is a per-user app. `bubbleplay_tray.exe` runs as the signed-in user
and always supervises `bubbleplay-host.exe --mode serve --worker <url>` as that
user (local DXGI capture + SendInput). Tokens reach the host only as
`BUBBLEPLAY_TOKEN` / `BUBBLEPLAY_HOST_GATE_TOKEN` environment variables.

- Install: `%LOCALAPPDATA%\Programs\BubblePlay` (host, tray, staged admin
  agent, `setup-tray.ps1`, `uninstall-tray.ps1`), HKCU Run autostart, a
  Start-menu shortcut and a per-user "Apps" entry (HKCU `...\Uninstall\BubblePlay`).
- Config: `%LOCALAPPDATA%\BubblePlay\host.json` (`worker`, `tokenDpapiHex`,
  `hostGateTokenDpapiHex`, optional `monitor`; secrets CurrentUser DPAPI) and
  `tray.json` (`adminMode`, `startAtLogin`).
- Logs: `%LOCALAPPDATA%\BubblePlay\logs` (`tray.log`, `host.log`), each capped
  at 5 x 5 MiB; the elevated agent logs to
  `%ProgramFiles%\BubblePlay\agent\logs\admin-agent.log`.
- Setup, from a normal PowerShell: `host/scripts/setup-tray.ps1` (use
  `-WhatIf` first). The user part needs no admin rights. Everything
  machine-wide runs in one elevated child (one UAC prompt, only when needed):
  install the admin agent and its `\BubblePlay\AdminAgent` task, remove the old
  `\BubblePlay\ElevatedHost` task and `%ProgramFiles%\BubblePlay\elevated`,
  delete the old services and QoS policies, and migrate
  `%ProgramData%\BubblePlay\transport.json` into `host.json` when no
  `host.json` exists. Old service binaries in `%ProgramFiles%\BubblePlay` are
  deleted only if they match `installed-hashes.json`; ProgramData is left as
  is. `-SkipAgent` installs no agent; `-AgentOnly` does just the agent part.
- Updates, non-elevated: `host/scripts/update-tray.ps1 [-Build]`. It never
  prompts for UAC; `-IncludeAgent` also installs the staged agent (one UAC
  prompt). Host changes never need a new agent.
- Uninstall: Settings > Apps > BubblePlay, or `uninstall-tray.ps1` in the
  install folder. It removes the agent and old elevated leftovers first (one
  UAC prompt, only if present), then the per-user install. `%LOCALAPPDATA%\BubblePlay`
  is kept unless `-RemoveData` is given.
- Host rollback: update-tray moves the replaced binaries to
  `<install>\previous\` and marks a new host `pending` in
  `%LOCALAPPDATA%\BubblePlay\update-state.json`. The tray restores the
  previous host (once, never twice in a row) if the new one does not log
  "signaling registered as host" within 120 s or exits 3 times in 5 min, and
  marks it `good` after 10 healthy minutes. The menu shows the host build and
  status, plus "Roll back host" while a different previous build exists. This
  works the same with Admin mode on, since the host always runs as the user.
- Admin mode (tray menu, default off) starts the admin input agent
  (`bubbleplay_admin_agent.exe`) through the on-demand `\BubblePlay\AdminAgent`
  task. The agent is a small elevated pipe server that only calls SendInput.
  The host sends it validated input records over a local named pipe, so input
  reaches elevated windows. The secure desktop (UAC prompts, lock screen) is
  still not captured. The agent exits when Admin mode is turned off or the
  tray goes away.
- Agent versioning: the pipe protocol is versioned (`host/src/admin_agent`).
  If the agent is missing, does not start, or speaks another protocol version,
  the host injects at user level and the tray offers "Admin agent needs update
  (one admin prompt)", which reruns `setup-tray.ps1 -AgentOnly` elevated.
- Agent security: only the admin-owned copy in `%ProgramFiles%\BubblePlay\agent`
  (protected ACL, verified by setup) runs elevated; the agent refuses to run
  elevated from anywhere else. Its pipe:
  - allows only the interactive user, SYSTEM and Administrators, gives the
    user no right to create pipe instances, and carries a medium-integrity
    label;
  - rejects remote clients and clients from other sessions, and is created
    with `FILE_FLAG_FIRST_PIPE_INSTANCE`;
  - requires a versioned hello first, validates every record, rate-limits
    clients, logs each connect, and releases held keys and buttons on
    disconnect.

  The host connects only to a server whose image is the Program Files agent.
  `--dev` (tests) works only unelevated.
- Residual risk: while Admin mode is on, any process running as the same user
  at medium integrity can connect to the pipe and drive elevated windows.
  Turn Admin mode on only when you need it.

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
