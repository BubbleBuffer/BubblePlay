# Secure desktop (UAC prompts, lock screen)

Status: approved 2026-10-08 (Admin mode storage revised 2026-10-08); Milestones A and B implemented, protected Admin mode in progress.

## Current state

Client image verification blocks other programs, but not code injected into a BubblePlay process by the same user. With Admin mode on, same-user malware can therefore still send elevated and secure-desktop input; keep Admin mode off unless you need it.

- The host runs as the interactive user. Desktop Duplication `E_ACCESSDENIED` and access-lost are treated as a temporarily unavailable source. Signaling and RTC/WSS sessions stay alive; the viewer holds the last frame and input is suspended with held keys/buttons released.
- Recovery probes run every 500 ms independently of media admission. Monitor and follow-window switches are refused while capture is unavailable; recovery uses the previously selected monitor. The one exception is the host's own temporary switch to the UAC prompt's monitor (below). Normal capture requires the input desktop to be Default both before and after acquisition; access denied, unreadable names or non-Default names block recovery even if DDA yields a stale frame. Helper secure reports also block normal recovery. The first verified Default DDA frame starts a new media generation and forces a keyframe. Normal input checks Default again immediately before injection.
- On a secure desktop the host sends `desktop-state` with `state:"unavailable"`, `desktop:"secure"`, `reason:"secure-desktop"`, `secureKind:"uac"|"lock"|"other"` and `secureCapabilities:[]`. The medium-integrity host enumerates consent windows only when Windows permits it, queries WTS lock state, and reports `other` when it cannot identify the kind. These optional fields preserve older desktop-state messages.
- `bubbleplay_secure_helper.exe` ships in every release (`tools/package-release.ps1` requires it), and Admin mode depends on it. It runs as SYSTEM in the active console session. The tray starts `\BubblePlay\SecureDesktop` when a viewer connects, whether Admin mode is on or off. The task launches the protected Program Files copy with `--launch`; it duplicates its SYSTEM token into the console session and starts `--serve` on `winsta0\default`. A per-session mutex prevents duplicate servers. The helper exits after host disconnect or 30 seconds without a host; tray version probes do not extend its lifetime.
- A fresh worker binds to the input desktop on each transition. Only Winlogon is captured: the selected monitor rectangle is copied with GDI at most 10 times per second, sent on image changes, and refreshed once per second. The worker composites the secure desktop cursor into the image; its movement participates in change detection. Two shared-memory slots carry bounded BGRA images; notifications and acknowledgments identify each slot/sequence/desktop epoch. The host copies and validates each image, uploads it to a texture on its own encoder device, and uses its existing capture/encode pipeline. Default-desktop media remains DDA in the user host. Entry and verified DDA recovery on exit each force a new generation and keyframe.
- **UAC prompt on another monitor.** The helper captures the monitor the host selected, but Windows shows the UAC prompt on the primary monitor. When the helper reports `secureKind:"uac"` and the viewed monitor is not the primary one, the host swaps its capture source to the primary monitor for as long as the secure desktop is up: the helper's region changes (a new worker epoch and media generation, announced as for any secure transition), input geometry is re-read and a `monitor-list` moves the selection. The viewed monitor and its geometry are saved in the capture source and restored when the secure desktop ends, before normal capture recovery; the normal DDA capture is never touched, so nothing needs re-opening. Lock screens and unidentified secure desktops keep the viewed monitor. Monitor picks stay refused on the secure desktop, so the user's choice cannot change meanwhile. The remembered monitor for later sessions is unchanged. Viewers just follow `monitor-list`; log lines `dialog follow: ...` ([logging.md](../logging.md)). A UAC prompt on the normal desktop (secure desktop switched off) is handled by [follow-active-window.md](follow-active-window.md#elevation-prompt-on-another-monitor).
- While helper frames are available the host announces `state:"active"`, `desktop:"secure"`, `reason:"desktop-switched"`, `secureKind` and `secureCapabilities`. Kind or Admin-mode capability changes resend the state within the same generation. A missing, failed or incompatible helper falls back to Milestone A's unavailable secure state with capabilities `[]`.
- Admin mode uses protected `HKLM\SOFTWARE\BubblePlay` values `AdminMode` and `AdminModeSid`, effective only for the current console user; `tray.json` grants no rights. The tray enables it through UAC, disables it through the SYSTEM `\BubblePlay\AdminModeOff` task, and disables enabling while a viewer is connected. The admin agent and secure helper re-read the policy for each request. Off allows secure view and UAC deny; on additionally permits pointer, keyboard and Unicode input and runs the elevated admin agent for normal-desktop elevated windows.
- The tray owns `\BubblePlay\AdminAgent` startup. It checks current host viewer demand independently of INFO logging or viewer log edges; with Admin mode on and an active viewer, a missing agent answer causes an idempotent task attempt, with retries at least 5 seconds apart. Launch/probe failure details stay in logs, and protected targets retain `input-status` reason `admin-unavailable` until the agent answers. The 30-second idle grace keeps a running agent alive after disconnect but makes no launch attempts without an active viewer. Host exit ends viewer demand; Admin mode off and tray exit stop the agent. Existing protected policy, image/session/SID trust checks and version negotiation remain unchanged.
- The helper's local pipe and read-only user mapping allow SYSTEM and the console user SID only and reject remote clients. A version handshake precedes requests; both privileged helpers require the client image to reside under `%ProgramFiles%\BubblePlay` with protected image/directory ACLs, in addition to their session and SID checks. The host verifies the server session, SYSTEM pipe owner, exact protected image path and ACL. Medium-integrity token queries failed live despite the launcher's token DACL, so pipe ownership supplies the SYSTEM check. Setup checks effective task rights because Task Scheduler normalizes the SDDL. Setup grants the installing user task read/execute and SYSTEM/Administrators full control. The tray offers an update when protocol versions do not overlap, attempts the task on every viewer start regardless of Admin mode or prior helper status, and logs launch and probe results.
- All shipped program files, scripts and runtime assets install under `%ProgramFiles%\BubblePlay`, with privileged helpers in `agent`; `%LOCALAPPDATA%\BubblePlay` remains the per-user data directory. Setup and update run from the chosen new release and request one UAC machine-install child; configuration, autostart, user data and receipts stay in the original unelevated process. Migration verifies the protected machine copy before removing an old per-user program directory. Tray helper repair only launches a setup source already protected under Program Files; otherwise the user must extract a new release and run its setup.
- Clients: the browser (`receiver.js`, `#desktop-state` card, `#secure-deny`) and Android (`SecureBanner`, `net/SecureDesktop.kt`) show a top banner while `desktop:"secure"`: "Admin prompt open on the PC", "PC is locked" or "Secure screen open on the PC", a line saying whether the screen is hidden, view-only or usable, and a "Deny prompt" button only when `deny` is in the capabilities. The button sends `secure-action` once and shows the `secure-action-result` (or "no answer from the PC" after 8 s).
- The retired SYSTEM broker/agent path (`bubbleplay_broker`, `bubbleplay_agent`, `service_runtime.rs`, `privileged_backend.rs`) was removed from the repo in October 2026 and survives only in git history. It captured Winlogon with GDI, encoded inside the SYSTEM agent, never shipped and was never validated live on Winlogon.
- The owner tested the launch/recovery fixes at 00:40–00:41 UTC on 2026-10-08: helper frames and Admin-mode capabilities worked, but the pointer appeared stationary despite received viewer packets. Secure absolute coordinates now remain normalized to the selected capture rectangle until the Winlogon-bound worker maps them against that desktop's virtual-screen metrics. The worker and probe explicitly use per-monitor V2 DPI awareness to keep physical monitor rectangles, metrics, GDI and cursor positions consistent on scaled displays. Previously the host used Default-desktop metrics, and secure GDI frames omitted the cursor while normal cursor feedback was suspended. These source defects are corrected; live input behavior still requires qualification.
- Helper protocol v2 adds request IDs and bounded input receipts with received, policy-accepted and SendInput-applied counts, rejection reasons and the immediate error code for partial injection. Secure frames carry the last applied input request ID at capture time, allowing the probe to reject queued images captured before a move. Host and helper emit one-second input summaries without key values, text or pixels; the worker logs its desktop, session and token binding. V1 helpers require an owner-run update with `powershell -File .\update-tray.ps1 -SourceDir $PWD.Path -IncludeAgent` from a newly extracted release before using the new host/probe.
- The owner-only development probe (`host/src/bin/probe_secure_helper.rs`, excluded from the release ZIP) runs through `powershell -File host/scripts/test-secure-desktop.ps1 -OutDir <new-directory>` in normal PowerShell with all viewers disconnected. Do not launch other local UAC requests during the test: an initially empty consent list and a single consent process during the pending runas request detect ambiguity but cannot prove the originating request. `-ExePath` optionally selects the probe binary, defaulting to `host/target/release/probe_secure_helper.exe`. A probe running from `target/` is refused by the same client image check; live qualification requires an administrator-installed probe under `%ProgramFiles%\BubblePlay`, supplied through `-ExePath`. It uses the real authenticated exclusive Host connection, restores the original tray settings in its finally path, and saves a JSON summary and secure PNGs. OFF requires helper policy rejection, a prompt still open after two seconds, successful DenyConsent and Default within five seconds. ON requires complete SendInput receipts for centre/corner moves with subsequent PNGs, then Escape closing the prompt within five seconds. Its input allowlist contains only Escape down/up and absolute movement: it never approves. Error paths attempt DenyConsent and require the asynchronous trigger to settle, Default to return and consent to disappear; cleanup failures produce a failing result. The implementation session runs only offline unit/build checks; it does not open live prompts or install/restart binaries.
- Offline checks for this input follow-up passed on 2026-10-08: `cargo test` across all targets (503 library tests passed, two existing ignored; all nine probe tests passed), `cargo build --release --bins`, and PowerShell syntax parsing of the probe wrapper. Only the existing `temporal_diff.rs` dead-code warnings remained. The owner must install the rebuilt v2 helper and run the probe to qualify actual UAC input, screenshots, denial and cleanup.
- Live probe run on 2026-10-08 (`artifacts/secure-desktop-input-test-3`): the OFF scenario passed. Both input records were rejected with `policy-off`, the prompt stayed open, and DenyConsent closed it in 241 ms. Secure frames show the consent dialog and the cursor; the very first frame can predate the dialog's fade-in. The ON scenario failed: SendInput returned error 5 because `InputDesktop::open` lacked `DESKTOP_JOURNALPLAYBACK`. That right is now requested. After the reinstall, run `artifacts/secure-desktop-input-test-4` passed both scenarios. OFF: policy rejection, then Deny closed the prompt in 294 ms. ON: every move was applied, the cursor appears at the requested centre and corner positions in the PNGs, and Escape closed the prompt in 277 ms. Cleanup was clean. The lock screen and real viewer clients are not covered by the probe.
- On 2026-10-08 the tray rolled back a new host while the relay returned 500 for a too-short `APP_PASSWORD`, because the host did not register within 120 s. The old host could not talk to the v2 helper, so viewers saw only the generic secure-screen banner. The host now logs `secure helper connect refused` for any connect failure other than "helper not running".

## Intent

On Default, the existing 250 ms foreground sample checks the window process's
integrity level and UIAccess on foreground changes. A target above the host's
integrity level, or with UIAccess, produces an additive warning, sent as the `input-status` `ctl` kind:
`reason:"admin-required"` when Admin mode is off, or `"admin-unavailable"`
when Admin mode is on and its helper is unavailable. `route` identifies
`local`, `admin` or `secure`; `reason:null` clears the warning. Warnings do not
disable client controls. An unavailable admin agent does not fall back to
local injection for a known protected target; Winlogon retains its exclusive
SYSTEM helper route and per-request protected policy.

One-second input diagnostics count received packet kinds before input gates,
drops by reason, injection routes and SendInput requested/applied events and
errors. Flow/drop transitions identify the reason; logs contain no key codes,
typed text or coordinates. These diagnostics support incident investigation;
the WhatsApp blackout's cause remains unproven.

Host admin-agent I/O uses an overlapped local pipe. The session thread never
waits for a connect: the pipe open, server check and hello (deadline 10 s,
retried once) run on a background thread (`admin-agent-connect`) that the next
input batch polls. Until it finishes, input that needs the agent is dropped as
`admin-unavailable` like any outage, and a pipe with every instance busy counts
as "not responding", not "needs update". Only the injection batch blocks the
session thread: each complete injection batch shares a
500 ms write/reply deadline across chunks. Protocol encoding and flushing
happen in memory; pipe I/O is overlapped.
Timeout or uncertain delivery reports agent loss and backs off; input that may
already have reached the agent is never repeated locally. Protected-target
`admin-unavailable` reporting and secure-desktop routing remain unchanged.
Cancelled operations retain their event, buffer and handle until Windows
reports completion; a fixed lock-free capacity makes stalled cancellation fail
closed instead of allocating indefinitely or waiting on another thread.

The host secure-helper client shares those overlapped I/O bounds: its hello
write/reply takes at most two seconds, and each background message uses a
500 ms read or write deadline. A partial frame stays queued until complete.
Failures retain the existing unavailable/reconnect behavior and SYSTEM trust
checks.

1. **Survive the prompt.** A secure-desktop switch keeps the viewer session open. The client shows "Admin prompt open on the PC" (or "PC is locked"). The host polls for the normal desktop every 500 ms and resumes on the same monitor.
2. **Secure-desktop helper.** Every release ships `bubbleplay_secure_helper.exe`, installed once with admin rights, runs with a SYSTEM token in the console session only while a viewer is connected. A SYSTEM scheduled task launches it; there is no permanent service. It captures the secure desktop (GDI, low frame rate, on change) and hands BGRA frames to the host through shared memory; the host encodes them with its normal encoder.
3. **Permission levels, enforced in the helper:**
   - Admin mode off: view only, plus one "Deny" action that cancels the current UAC prompt. The action takes no coordinates or keys; the helper finds the consent window itself. The lock screen is view only.
   - Admin mode on: full mouse and keyboard on the secure desktop, including the lock screen.
   - Admin mode is stored where only administrators can write (`HKLM\SOFTWARE\BubblePlay`, values `AdminMode` and `AdminModeSid`). The helper, the tray and the admin agent all read it from there; `tray.json` no longer grants anything.
   - Turning it on needs a UAC prompt. A remote viewer with Admin mode off can only Deny that prompt, so a session can never raise its own rights. The tray greys out "turn on" while a viewer is connected.
   - Turning it off is one click with no prompt: the tray runs a SYSTEM task that can only clear the value.
4. Without the helper, behaviour is (1) only.

## Hung foreground windows and input

Rule: **the input path never blocks on another window**, including clipboard
owners. Previously `InputSink` read clipboard formats and replaced clipboard
text inline with input dispatch. Windows can message the owner for delayed
rendering or `EmptyClipboard`; a hung owner stalled input before either local
or elevated injection. File uploads also held the input owner's file-service
mutex across clipboard replacement. See Microsoft's [clipboard documentation](https://learn.microsoft.com/en-us/windows/win32/dataxchg/clipboard-operations).

`clipboard_worker.rs` now runs reads/writes through bounded queues, and uploads
release the shared mutex before clipboard calls. Input submission, completion
reads and clipboard-worker teardown never wait for native clipboard work.
Live workers are capped, and session/desktop epochs discard stale queued work
and results. An already running Windows clipboard call cannot be cancelled.
Paste requires a successful write, the original foreground identity and an
age under two seconds; new presses, scrolling, release-all, geometry or session/
desktop changes cancel it. Modifier releases from Ctrl+V remain usable. The
admin-agent and secure-helper protocols and security checks are unchanged.

Regressions use a temporarily unpumped message-only window and controlled
backends, without real input, focus changes or access to the user's clipboard.
The owner's exact application, Windows wait/close dialog and live Alt+Tab still
need VM qualification.

### Ghost foreground: input through the secure helper (2026-11-12)

About 5 s after a top-level window stops pumping, DWM replaces it with a `Ghost`
window owned by `dwm.exe` at System integrity (0x4000). UIPI then drops all
input from the elevated admin agent (High, 0x3000) and from the host (Medium):
`SendInput` still reports success, but nothing reaches even the low-level
hooks, so the viewer cannot click any other window to recover. Measured in CI by
`host/tests/hung_foreground_input.rs` before this change: control clicks
arrived, the click on a healthy window while the ghost was foreground did not.

With Admin mode on, the LocalSystem secure helper (System integrity, already
running in the console session on `winsta0\default`) injects normal-desktop
input in that one case:

- Helper protocol v3 adds `Kind::ElevatedForeground` (`elevated-foreground`).
  Only a host that negotiated v3 gets the new mode; v2 hosts are unchanged.
- The helper verifies the foreground itself, never the host's claim
  (`protocol::elevated_foreground_matches`): class `Ghost`, owner image
  `%SystemRoot%\System32\dwm.exe` in the console session, owner integrity above
  High. Any other window, including other System-integrity ones, stays on the
  admin-agent path. Admin mode must be on for the console user, as for secure input.
- The worker binds to the Default input desktop, checks that it is LocalSystem in
  the console session on `Default`, and reports `State{secure:false,
  kind:elevated-foreground}` every 100 ms; caps are `["input"]` only (no pixels,
  no deny). It re-verifies the foreground before every batch (rejection
  `foreground-not-elevated`), rate-limits like the secure worker, and injects
  the records unchanged (virtual-desk normalized, as for the admin agent). When
  the foreground changes it leaves, releases any button or key it pressed and
  reports `other`; the host falls back to the admin agent.
- Host: `InputSink` routes a normal-desktop batch to the helper only when Admin
  mode is on and the helper's last state is `elevated-foreground` with input.
  Submission is the same non-blocking `try_send` as the secure route; nothing on
  the session thread waits for the helper. If the helper refuses, the batch
  takes the admin-agent path. Capture is untouched (the host keeps DDA).
- Same pipe, authentication, identity and version checks as before; no new
  service, binary or uiAccess.

CI (`Host checks`, windows-latest) installs the helper with
`host/scripts/ci-install-secure-helper.ps1` (protected Program Files copy, SYSTEM
task, Admin mode bound to the runner account) and the test sends the recovery
click as viewer packets through `InputSink` -> `SecureHelperLink` -> the real
helper. It also checks that the stall report names the hung window behind the
ghost (user32 `HungWindowFromGhostWindow`).

## Milestone A validation and owner test

- `cargo test` in `host/`: passed (all binaries, library and integration tests; two existing library tests ignored).
- `cargo build --release --bins` in `host/`: passed. Existing dead-code warnings remain in `temporal_diff.rs`.
- `go test ./...` in `relay/`: passed.
- `npm test` in `worker/`: 643 passed, zero failed.
- `powershell -File host/scripts/setup-tray.ps1 -WhatIf` and `-IncludeAgent -WhatIf`: both passed. `-IncludeAgent` explicitly selects the existing default agent install; it conflicts with `-SkipAgent`.
- Nothing was installed or deployed, and the running tray/host/admin agent were not restarted. The admin-agent integration tests use their own unelevated development instances.

To qualify A on the existing installation, the owner runs
`powershell -File .\update-tray.ps1 -SourceDir $PWD.Path` from a normal PowerShell in
a newly extracted release directory. That updates the protected program files
through one UAC prompt and restarts the tray and host. For a first install, use
`powershell -File .\setup-tray.ps1 -SourceDir $PWD.Path` from that directory.
Connect a viewer on a non-primary monitor, open a UAC prompt locally, then
dismiss it locally; repeat with lock/unlock. Check that signaling and the
RTC/WSS session remain connected, input is suspended during the transition,
the UAC prompt shows on the primary monitor (the monitor list's selection
moves there) and the viewed monitor returns when it is dismissed, the lock
screen keeps the viewed monitor, manual/follow switches are refused meanwhile, the client receives the secure desktop state, and a fresh keyframe
in a new generation resumes streaming. Measure the approximately one-second
recovery target on both transports. All of these live claims are pending.

## Milestone B contract and owner test

After the owner's live report, the recovery gate and explicit viewer-start
helper launch/logging fixes passed `cargo test` (all targets; library 496,
host 81, tray 27 passed, two existing library tests ignored) and
`cargo build --release --bins`. New tests cover a DDA frame rejected when the
input desktop is secure/unknown, missing or delayed Default helper reports,
and helper launch on every viewer start with Admin mode off/on and a prior
failure status. These repairs have not been installed or tested live by the
implementation session. The owner's pipe-owner check and effective task-rights
check were preserved.

Offline acceptance on 2026-10-08:

- `cargo test`: all binaries, library, integration tests and doc tests passed;
  library 495 passed with two existing ignored tests, host binary 80 passed.
  Coverage includes unavailable HRESULT classification, pinned monitor/recovery,
  secure state/action serialization, warm/cold recovery, policy matrix, consent
  identity, handshake gating, malformed frame headers and secure input fallback.
- `cargo build --release --bins`: passed; only existing `temporal_diff.rs`
  dead-code warnings remained.
- `go test ./...`: passed. Worker `npm test`: 647 passed, zero failed.
- Both required setup `-WhatIf` invocations passed and planned the helper
  binary plus SYSTEM task without blocked steps. Setup/update/uninstall/package
  scripts also passed PowerShell syntax parsing.
- A local candidate ZIP passed entry/size/SHA256 verification, with the helper
  in both the ZIP and `release-manifest.json`. Nothing was published or installed.

[`fixtures/secure-desktop-contract.json`](../../fixtures/secure-desktop-contract.json)
defines unavailable states, active states for both Admin-mode policies and all
three kinds, and the parameterless request
`{"type":"secure-action","action":"deny"}`. Its result is
`{"type":"secure-action-result","action":"deny","ok":bool,"reason":string}`.
Both relays validate message direction and reject added deny parameters.
Only a top-level window owned by the exact `%SystemRoot%\System32\consent.exe`
image in the console session qualifies as UAC. Deny posts `WM_CLOSE`, waits up
to two seconds, then tries `WM_COMMAND/IDCANCEL` on remaining verified targets
and waits two more seconds. It never synthesizes keyboard or mouse input;
success requires confirmed absence of consent windows. Query failures and
remaining windows return failure. No generic cancellation is attempted.

The helper protocol validates dimensions/strides/lengths, monotonic sequences,
desktop epochs and copied frame headers/hash. Each slot is limited to 64 MiB,
with dimensions at most 8192 pixels; unsupported geometry falls back to an
unavailable source. Logs contain transitions, policy decisions and deny
attempts/results, never pixel contents or typed text.

For an existing installation, the owner runs from a normal PowerShell in a
newly extracted release directory:

```powershell
powershell -File .\update-tray.ps1 -SourceDir $PWD.Path -IncludeAgent
```

For a first installation, use
`powershell -File .\setup-tray.ps1 -SourceDir $PWD.Path -IncludeAgent`.
The agent and helper install together in one elevated child, with one UAC
prompt. The updater/setup restarts the tray/host as part of installation;
disconnect existing viewers first and reconnect afterward. `-AgentOnly`
repairs both protected binaries/tasks. Removal removes the helper task and
binary too. The candidate ZIP and SHA256 manifest include the helper.

On RTC and WSS, select a non-primary monitor and open a local UAC prompt with
Admin mode off. Verify secure video, `view,deny`, dropped pointer/keyboard/text,
and deny result followed by resumed DDA in a new generation. Repeat with
Admin mode on and verify full input, including Unicode text on lock/unlock.
Toggle Admin mode while a prompt is open and inspect resent capabilities and
held-input release. Test lock and other secure states without deny capability,
helper missing/failure/incompatible fallback, viewer disconnect/30-second
no-client exit, and monitor/follow switching rejection. Measure the one-second
recovery target on the prior monitor. Task launch/ACLs, cross-session rejection,
secure pixels, protected input and Windows consent cancellation remain live
qualification requirements.

## In-place desktop transitions on WebRTC

Status: implemented 2026-10-12; live UAC qualification pending.

- Desktop-source transitions (UAC, Winlogon, lock, `desktop-switched`, `capture-recovered`, `session-changed`, `agent-restarted`) stay on the existing WebRTC peer when the browser can prove which frames belong to the new media generation. Before this change every such transition restarted signaling and the peer, which showed as a short disconnect on each UAC prompt. Held input is still released on these transitions and is never carried onto the secure desktop.
- Host: `RtcSession::write_video` (`host/src/rtc.rs`) returns the 32-bit RTP timestamp it gave the access unit; str0m copies the 90 kHz media time verbatim into the RTP header. The media time is kept strictly increasing, so every access unit written before an IDR has an earlier timestamp. The WebRTC `first-frame` `admitted` message carries that IDR's `rtpTimestamp` (optional; older clients and both relays ignore it). WSS admissions are unchanged. The cached replay IDR (`CachedIdr`) records its media generation and is never replayed or reused for admission in another generation, since a replay gets a fresh RTP timestamp and would otherwise pass the gate with old-desktop pixels.
- Browser, native `<video>`: once admitted, the gate opens only for a `requestVideoFrameCallback` frame whose `metadata.rtpTimestamp` is at or after the admitted one (32-bit serial-number comparison, so wraparound is handled). Frames with earlier timestamps, and `loadeddata`/`playing` events, do not open it. A frame presented before the admission message arrived counts only if its timestamp already reached the IDR.
- Browser, WebCodecs: after admission the pipeline resyncs and drops frames until a keyframe at or after the admitted timestamp; during a transition, a keyframe whose encoded frame exposes no RTP timestamp is unprovable and triggers the peer-restart fallback (input stays gated). Only startup and post-restart handshakes accept any keyframe after admission. The transform worker reports draws for the gated request (`transitionToken`/`transitionDrawn`), so its cumulative frame count no longer opens the gate.
- Fallback: the browser restarts the peer in a new signaling epoch, as before, when the host omits `rtpTimestamp`, native video lacks rVFC or `rtpTimestamp` (decided before the transition from earlier frames, and again at admission), or no qualifying frame is presented within the first-frame watchdog (two 5 s attempts). Input stays gated until the restarted peer's own handshake presents: that handshake is forced, requires admission, and is RTP-gated when the host sends a timestamp (as a startup handshake, so a missing timestamp cannot loop into another restart). On the host, an offer that starts a new peer turns input off in the user host as in the privileged Agent until that peer's handshake presents. A `desktop-state` that re-gates input after a handshake already presented starts a new handshake instead of waiting on the finished one.
- Monitor transitions on WebRTC use the same gate and fallback ([follow-active-window.md](follow-active-window.md#monitor-transitions-in-the-browser)).
- Known limits: the host's own input gate is not independent of the browser. It enables input when the client sends `first-frame` `presented` (a `ctl` kind; its contents are now authenticated end to end, but a modified client can still claim presentation early) for the current request and generation, and trusts that message; the RTP-timestamp proof runs only in the browser. A modified client can therefore claim presentation early; what the host still enforces is the generation match, the secure-desktop input policy of the helper or Agent, and the new-peer gate above. A browser tab in the background presents no rVFC frames and therefore falls back to a restart after the watchdog. Not yet qualified live with a real UAC prompt.

## Decisions

- 2026-10-12: WebRTC desktop-source transitions happen in place, gated on the admitted IDR's RTP timestamp, instead of restarting the peer. Reason: the restart showed as a disconnect on every UAC prompt; the RTP timestamp gives native video the generation proof it lacked. Supersedes the "desktop-source transitions restart the WebRTC peer" rule.

- 2026-10-08: Admin mode moves from user-writable `tray.json` to an admin-only HKLM value. Reason: a remote viewer controls the user's desktop and could flip the tray menu item, gaining input on UAC prompts and the lock screen. Supersedes the earlier "residual risk" note.

## Open questions

- Which cancel mechanism (targeted `WM_CLOSE` or `IDCANCEL` on the consent window) works on Windows 11 24H2+; must be qualified live and fail closed.
