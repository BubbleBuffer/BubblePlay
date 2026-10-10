# Development

Run development commands below from the repository root. For the public product and installation guide, start at [the documentation index](README.md).

## Repository map

| Path | What to change here |
| --- | --- |
| [`host/`](../host) | Rust Windows capture/encoding, input, clipboard, files, tray and optional admin-input helper |
| [`relay/`](../relay/README.md) | Go relay/authentication and Docker definitions |
| [`worker/src/`](../worker/src) | Cloudflare relay, authentication and protocol validation |
| [`worker/public/`](../worker/public) | Browser receiver shared by Go and Cloudflare; no frontend bundler |
| [`android/`](../android/README.md) | Native Kotlin receiver, gestures, codec and APK build |
| [`fixtures/`](../fixtures) | Shared packet and feature contracts; update affected language implementations together |
| [`tools/`](../tools/README.md) | Repository checks, release packaging and dependency notices |
| [`worker/tools/`](../worker/tools/README.md) | Worker administration and isolated stream-lab tooling |
| [`branding/`](../branding) | Editable branding source |
| [`docs/`](README.md) | Current public guides, architecture and release status |

Generated reports, captures and packages belong under ignored `artifacts/`; Rust and Android build caches remain in their module build directories. Historical local troubleshooting is archived outside the public documentation.

Start with the affected module's checks. Changes to packet semantics should include the shared fixture and both sides of the exchange. Keep credentials, local deployment logs, captures and benchmark outputs outside public documentation. See [architecture](architecture/README.md) for runtime entry points and [release status](release-status.md) for checks requiring a real environment.

## Go relay and release checks

The standalone relay is in `relay/`. It serves the existing browser assets; Windows capture and encoding remain in the Rust host. Use `go test ./...` and `go vet ./...` from `relay/`, then follow its setup guide for a separate development instance. Never reuse production credentials for fixtures.

The CI definition covers browser checks on Node 22/24, synthetic Windows packaging/setup checks in both PowerShell versions, Go race tests and a container build. It does not establish an NVENC host build or a physical desktop-stream test: those need the Windows/NVIDIA build environment and explicit runtime evidence.

From the root, run the release tooling checks separately:

```powershell
node --test tools/tests/*.test.mjs
./tools/tests/package-release.test.ps1
./host/scripts/test-setup-credentials.ps1
./worker/tools/test-set-app-password.ps1
```

`tools/package-release.ps1` packages only explicit public files and verifies every ZIP entry. Supply `docs/windows-quickstart.md`, `docs/THIRD-PARTY-NOTICES.md` and the root LICENSE. `-PublicRelease` requires that license; source provenance must reflect uncommitted changes. Review the exact binaries and manifest before publishing.

`tools/generate-notices.mjs` reads an existing locked Windows Cargo metadata snapshot and cached dependency licenses offline. It retains full license/notice texts and fails if provenance/texts are missing. The exact upstream exceptions live in `tools/third-party/notice-overrides.json`.

Windows desktop streaming uses the Rust capture/encode host, either relay, and a Chromium receiver. See [Go relay setup](../relay/README.md) or [Worker setup](../worker/README.md); the stream lab currently uses the Worker/local lab paths.

## Cursor translation

The host sends semantic cursor types: arrow, text, hand, crosshair, move,
directional resize, wait, progress, forbidden, help, upward arrow and hidden.
The browser uses native CSS cursors; Android uses system mouse pointer icons.
Touch and captured-pointer modes draw the corresponding cursor glyph at the
locally predicted position. Unknown/custom cursor images fall back to an arrow.
In browser Work mode, a hidden host cursor also falls back to an arrow so the
local pointer stays usable. Game mode uses browser pointer lock to hide it.
The local tray host temporarily enables Windows Mouse Keys when a viewer is
connected and no mouse device is installed, allowing Windows to expose cursor
shapes. It preserves existing Mouse Keys preferences and restores its change
on disconnect or tray shutdown. Numeric-keypad behavior follows those
preferences while the session is active. An ownership record lets the tray
clean up after a host crash and the next host recover after an abrupt tray exit.
Temporary activation or cleanup failures get two spaced recovery attempts.

The existing 8-byte position packet (type 128) stays unchanged. Type 129 is a
2-byte reliable packet `[129, kind]`, with IDs and client mappings specified in
[`fixtures/cursor-types.json`](../fixtures/cursor-types.json). Cursor changes are
sampled independently of remote movement, replayed on sync/reconnect, and use
one latest pending type rather than an accumulating queue. The browser selects
RTC control while it is open and signaling otherwise to avoid duplicate updates
arriving out of order. Older clients ignore the new packet; older hosts keep the
client's default arrow.

For the optional privileged capture backend, the host and transport Agent must
both use privileged IPC version 5. Upgrade those components together; this is
separate from the tray's admin input agent protocol.

## Development checks

Use Node 22 or newer and stable Rust for Windows MSVC. Install Visual Studio C++
build tools and the NVIDIA Video Codec SDK, or generate local import libraries
with `host/scripts/make-nvenc-stubs.ps1` (requires the NVIDIA driver and lib.exe).
Set `NVIDIA_VIDEO_CODEC_SDK_PATH` to a real SDK to override `host/sdk-stub`.
Keep the trailing `/` on `sdk-stub/` in `host/.cargo/config.toml`: without it rust-lld drops `/OUT` and writes `*.rcgu.o.rcgu.exe` files into `host/`.
Run `cargo fetch --locked --manifest-path host/Cargo.toml` and install the locked
Worker dependencies with `npm --prefix worker ci` once during setup.

From the repository root:

```powershell
node tools/check.mjs          # all deterministic checks, offline Rust resolution
node tools/check.mjs client   # Node tests and all JavaScript/inline-script syntax
node tools/check.mjs host     # Rust tests and checks across all targets
node tools/check.mjs syntax   # syntax only
```

The checker uses the current Node executable directly. Missing dependencies fail the offline check rather than triggering installation.

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
- Admin mode (tray menu, default off) runs the admin input agent
  (`bubbleplay_admin_agent.exe`) through the on-demand `\BubblePlay\AdminAgent`
  task. The agent is a small elevated pipe server that only calls SendInput.
  The host sends it validated input records over a local named pipe, so input
  reaches elevated windows. The secure desktop (UAC prompts, lock screen) is
  not captured without the optional secure-desktop helper. The host keeps
  the viewer session and signaling/media transports alive, suspends input,
  and probes DDA recovery every 500 ms on the previously selected monitor.
  Recovery starts a new generation with a keyframe. See
  [the secure-desktop contract](architecture/secure-desktop.md); live UAC/lock
  qualification is still pending.
- The agent runs only while Admin mode is on and a viewer is connected. The
  host logs "viewer session started" once a viewer's media transport (RTC or
  WSS) is up and "viewer session ended" when that session ends. The tray
  starts the task on the first session. It stops the agent 30 s after the last
  session ended (a reconnect or page reload within that time keeps it
  running), when Admin mode is turned off, or when the tray goes away. A host
  exit counts as the session ending. The menu shows "Admin mode: armed (starts
  when a viewer connects)", "starting" or "active". Until the agent answers
  (about 1 s after start, plus up to 2 s until the host's next connection
  attempt), the host injects at user level, so no input is dropped, but it
  does not reach elevated windows yet.
- Agent versioning: the pipe protocol is versioned (`host/src/admin_agent`).
  If the agent is missing, does not start, or speaks another protocol version,
  the host injects at user level and the tray offers "Admin agent needs update
  (one admin prompt)", which reruns `setup-tray.ps1 -AgentOnly` elevated.
- Secure desktop: `setup-tray.ps1 -IncludeAgent` / `-AgentOnly` installs the
  admin agent and SYSTEM secure helper together in the same elevated child.
  The tray starts `\BubblePlay\SecureDesktop` for every viewer session,
  independently of Admin mode. The helper supplies secure BGRA frames to the
  user host's encoder and exits on host disconnect (or after 30 seconds with
  no host). It sends no normal-desktop frames. Its separate version handshake
  drives the tray's "Secure desktop helper needs update" action.
  Admin mode off permits view plus parameterless UAC deny; on adds mouse,
  keyboard and Unicode text. Policy comes from the console user's tray.json
  at each request, fails off, and is enforced again inside the helper.
  See the architecture page and fixture for active secure states and deny
  results. Protected capture/input and task launch remain unverified live.
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
- Residual risk: while the agent runs (Admin mode on and a viewer connected,
  plus the 30 s grace), any process running as the same user at medium
  integrity can connect to the pipe and drive elevated windows.
  Turn Admin mode on only when you need it.
- The SYSTEM helper likewise trusts processes running as the console user:
  they can connect to its pipe, view secure frames or change user-writable
  `adminMode` to enable protected input. Exact image/ACL/SYSTEM checks protect
  the host from a substituted helper; they do not isolate same-user processes.
  Secure video may reveal a consent identity or lock-screen information to the
  viewer and to a WSS relay. Logs omit frame data and typed text.

## Web session UI

- A docked strip holds every session control: a 32 px row at the top on a
  desktop browser, a 48 px bar at the bottom on touch. The stream is laid out
  beside it, never under it. Fullscreen on a desktop hides the strip; resting
  the pointer on the top edge shows a handle that brings it back.
- Monitors, Shortcuts, Files, Keys, Arrows and More open small popovers that
  close on an outside click or Esc. Settings (quality, keyboard lock, scroll
  speed, pointer mode, default monitor, sign out, diagnostics, about) is a
  slide-over saved in `localStorage` under `bubbleplay.settings`.
- Behavior and implementation: [session UI](architecture/session-ui.md).

## File transfer

- Desktop browser to PC: drop files on the stream to hand them to the Windows
  app under the pointer through native virtual-file drag and drop. Explorer
  saves them in the target folder; compatible browser drop zones receive files.
  A small marker shows the intended spot; compatible hosts show a native hover
  while dragging, then replace that preview with the real files on release.
  Files > Choose files… keeps saving to `%USERPROFILE%\Downloads\BubblePlay`
  with Mark-of-the-Web and PC clipboard placement. Paste uploads still paste
  into the focused app. Mobile and older hosts keep the existing save behavior.
- PC to browser: copy files in Explorer or use Send To > BubblePlay; the
  Files icon gets a dot and its popover shows "N files from PC · Download".
  Only offered files can be read.
- Bytes use a separate `/file-ws` socket, with host downloads rate-capped below video. Contract
  and behavior: [file transfer](architecture/file-transfer.md); lab check:
  `node worker/tools/lab/stream-lab-file-transfer.mjs`.
  Native desktop acceptance: `node worker/tools/lab/stream-lab-native-file-drop.mjs`
  (synthetic files and local destinations).

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
