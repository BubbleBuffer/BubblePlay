# Secure desktop (UAC prompts, lock screen)

Status: approved 2026-10-08 (Admin mode storage revised 2026-10-08); Milestones A and B implemented, protected Admin mode in progress.

## Current state

- The host runs as the interactive user. Desktop Duplication `E_ACCESSDENIED` and access-lost are treated as a temporarily unavailable source. Signaling and RTC/WSS sessions stay alive; the viewer holds the last frame and input is suspended with held keys/buttons released.
- Recovery probes run every 500 ms independently of media admission. Monitor and follow-window switches are refused while capture is unavailable; recovery uses the previously selected monitor. Normal capture requires the input desktop to be Default both before and after acquisition; access denied, unreadable names or non-Default names block recovery even if DDA yields a stale frame. Helper secure reports also block normal recovery. The first verified Default DDA frame starts a new media generation and forces a keyframe. Normal input checks Default again immediately before injection.
- On a secure desktop the host sends `desktop-state` with `state:"unavailable"`, `desktop:"secure"`, `reason:"secure-desktop"`, `secureKind:"uac"|"lock"|"other"` and `secureCapabilities:[]`. The medium-integrity host enumerates consent windows only when Windows permits it, queries WTS lock state, and reports `other` when it cannot identify the kind. These optional fields preserve older desktop-state messages.
- `bubbleplay_secure_helper.exe` ships in every release (`tools/package-release.ps1` requires it), and Admin mode depends on it. It runs as SYSTEM in the active console session. The tray starts `\BubblePlay\SecureDesktop` when a viewer connects, whether Admin mode is on or off. The task launches the protected Program Files copy with `--launch`; it duplicates its SYSTEM token into the console session and starts `--serve` on `winsta0\default`. A per-session mutex prevents duplicate servers. The helper exits after host disconnect or 30 seconds without a host; tray version probes do not extend its lifetime.
- A fresh worker binds to the input desktop on each transition. Only Winlogon is captured: the selected monitor rectangle is copied with GDI at most 10 times per second, sent on image changes, and refreshed once per second. The worker composites the secure desktop cursor into the image; its movement participates in change detection. Two shared-memory slots carry bounded BGRA images; notifications and acknowledgments identify each slot/sequence/desktop epoch. The host copies and validates each image, uploads it to a texture on its own encoder device, and uses its existing capture/encode pipeline. Default-desktop media remains DDA in the user host. Entry and verified DDA recovery on exit each force a new generation and keyframe.
- While helper frames are available the host announces `state:"active"`, `desktop:"secure"`, `reason:"desktop-switched"`, `secureKind` and `secureCapabilities`. Kind or Admin-mode capability changes resend the state within the same generation. A missing, failed or incompatible helper falls back to Milestone A's unavailable secure state with capabilities `[]`.
- Admin mode uses protected `HKLM\SOFTWARE\BubblePlay` values `AdminMode` and `AdminModeSid`, effective only for the current console user; `tray.json` grants no rights. The tray enables it through UAC, disables it through the SYSTEM `\BubblePlay\AdminModeOff` task, and disables enabling while a viewer is connected. The admin agent and secure helper re-read the policy for each request. Off allows secure view and UAC deny; on additionally permits pointer, keyboard and Unicode input and runs the elevated admin agent for normal-desktop elevated windows.
- The tray owns `\BubblePlay\AdminAgent` startup. It checks current host viewer demand independently of INFO logging or viewer log edges; with Admin mode on and an active viewer, a missing agent answer causes an idempotent task attempt, with retries at least 5 seconds apart. Launch/probe failure details stay in logs, and protected targets retain `input-status` reason `admin-unavailable` until the agent answers. The 30-second idle grace keeps a running agent alive after disconnect but makes no launch attempts without an active viewer. Host exit ends viewer demand; Admin mode off and tray exit stop the agent. Existing protected policy, image/session/SID trust checks and version negotiation remain unchanged.
- The helper's local pipe and read-only user mapping allow SYSTEM and the console user SID only and reject remote clients. A version handshake precedes requests; client session/SID checks meet the admin agent's checks. The host verifies the server session, SYSTEM pipe owner, exact protected image path and ACL. Medium-integrity token queries failed live despite the launcher's token DACL, so pipe ownership supplies the SYSTEM check. Setup checks effective task rights because Task Scheduler normalizes the SDDL. Setup grants the installing user task read/execute and SYSTEM/Administrators full control. The tray offers an update when protocol versions do not overlap, attempts the task on every viewer start regardless of Admin mode or prior helper status, and logs launch and probe results.
- Clients: the browser (`receiver.js`, `#desktop-state` card, `#secure-deny`) and Android (`SecureBanner`, `net/SecureDesktop.kt`) show a top banner while `desktop:"secure"`: "Admin prompt open on the PC", "PC is locked" or "Secure screen open on the PC", a line saying whether the screen is hidden, view-only or usable, and a "Deny prompt" button only when `deny` is in the capabilities. The button sends `secure-action` once and shows the `secure-action-result` (or "no answer from the PC" after 8 s).
- The repo still has the retired SYSTEM broker/agent path (`bubbleplay_broker`, `bubbleplay_agent`, `service_runtime.rs`, `privileged_backend.rs`). It captured Winlogon with GDI and encoded inside the SYSTEM agent. It is excluded from the release and was never validated live on Winlogon.
- The owner tested the launch/recovery fixes at 00:40–00:41 UTC on 2026-10-08: helper frames and Admin-mode capabilities worked, but the pointer appeared stationary despite received viewer packets. Secure absolute coordinates now remain normalized to the selected capture rectangle until the Winlogon-bound worker maps them against that desktop's virtual-screen metrics. The worker and probe explicitly use per-monitor V2 DPI awareness to keep physical monitor rectangles, metrics, GDI and cursor positions consistent on scaled displays. Previously the host used Default-desktop metrics, and secure GDI frames omitted the cursor while normal cursor feedback was suspended. These source defects are corrected; live input behavior still requires qualification.
- Helper protocol v2 adds request IDs and bounded input receipts with received, policy-accepted and SendInput-applied counts, rejection reasons and the immediate error code for partial injection. Secure frames carry the last applied input request ID at capture time, allowing the probe to reject queued images captured before a move. Host and helper emit one-second input summaries without key values, text or pixels; the worker logs its desktop, session and token binding. V1 helpers require an owner-run update with `powershell -File host/scripts/update-tray.ps1 -IncludeAgent` before using the new host/probe.
- The owner-only development probe (`host/src/bin/probe_secure_helper.rs`, excluded from the release ZIP) runs through `powershell -File host/scripts/test-secure-desktop.ps1 -OutDir <new-directory>` in normal PowerShell with all viewers disconnected. Do not launch other local UAC requests during the test: an initially empty consent list and a single consent process during the pending runas request detect ambiguity but cannot prove the originating request. `-ExePath` optionally selects the probe binary, defaulting to `host/target/release/probe_secure_helper.exe`. It uses the real authenticated exclusive Host connection, restores the original tray settings in its finally path, and saves a JSON summary and secure PNGs. OFF requires helper policy rejection, a prompt still open after two seconds, successful DenyConsent and Default within five seconds. ON requires complete SendInput receipts for centre/corner moves with subsequent PNGs, then Escape closing the prompt within five seconds. Its input allowlist contains only Escape down/up and absolute movement: it never approves. Error paths attempt DenyConsent and require the asynchronous trigger to settle, Default to return and consent to disappear; cleanup failures produce a failing result. The implementation session runs only offline unit/build checks; it does not open live prompts or install/restart binaries.
- Offline checks for this input follow-up passed on 2026-10-08: `cargo test` across all targets (503 library tests passed, two existing ignored; all nine probe tests passed), `cargo build --release --bins`, and PowerShell syntax parsing of the probe wrapper. Only the existing `temporal_diff.rs` dead-code warnings remained. The owner must install the rebuilt v2 helper and run the probe to qualify actual UAC input, screenshots, denial and cleanup.
- Live probe run on 2026-10-08 (`artifacts/secure-desktop-input-test-3`): the OFF scenario passed. Both input records were rejected with `policy-off`, the prompt stayed open, and DenyConsent closed it in 241 ms. Secure frames show the consent dialog and the cursor; the very first frame can predate the dialog's fade-in. The ON scenario failed: SendInput returned error 5 because `InputDesktop::open` lacked `DESKTOP_JOURNALPLAYBACK`. That right is now requested. After the reinstall, run `artifacts/secure-desktop-input-test-4` passed both scenarios. OFF: policy rejection, then Deny closed the prompt in 294 ms. ON: every move was applied, the cursor appears at the requested centre and corner positions in the PNGs, and Escape closed the prompt in 277 ms. Cleanup was clean. The lock screen and real viewer clients are not covered by the probe.
- On 2026-10-08 the tray rolled back a new host while the relay returned 500 for a too-short `APP_PASSWORD`, because the host did not register within 120 s. The old host could not talk to the v2 helper, so viewers saw only the generic secure-screen banner. The host now logs `secure helper connect refused` for any connect failure other than "helper not running".

## Intent

On Default, the existing 250 ms foreground sample checks the window process's
integrity level and UIAccess on foreground changes. A target above the host's
integrity level, or with UIAccess, produces an additive `input-status` warning:
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

Host admin-agent I/O uses an overlapped local pipe: the hello write and reply
share a two-second deadline, and each complete injection batch shares a
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

## Milestone A validation and owner test

- `cargo test` in `host/`: passed (all binaries, library and integration tests; two existing library tests ignored).
- `cargo build --release --bins` in `host/`: passed. Existing dead-code warnings remain in `temporal_diff.rs`.
- `go test ./...` in `relay/`: passed.
- `npm test` in `worker/`: 643 passed, zero failed.
- `powershell -File host/scripts/setup-tray.ps1 -WhatIf` and `-IncludeAgent -WhatIf`: both passed. `-IncludeAgent` explicitly selects the existing default agent install; it conflicts with `-SkipAgent`.
- Nothing was installed or deployed, and the running tray/host/admin agent were not restarted. The admin-agent integration tests use their own unelevated development instances.

To qualify A on the existing installation, the owner runs
`powershell -File host/scripts/update-tray.ps1` from a normal PowerShell in
the repository root. That updates the per-user binaries and restarts the tray
and host. For a first install, use `powershell -File host/scripts/setup-tray.ps1`.
Connect a viewer on a non-primary monitor, open a UAC prompt locally, then
dismiss it locally; repeat with lock/unlock. Check that signaling and the
RTC/WSS session remain connected, input is suspended during the transition,
the selected monitor does not change (including attempted manual/follow
switches), the client receives the secure desktop state, and a fresh keyframe
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

For an existing installation, the owner runs from a normal PowerShell in the
repository root:

```powershell
powershell -File host/scripts/update-tray.ps1 -IncludeAgent
```

For a first installation, use
`powershell -File host/scripts/setup-tray.ps1 -IncludeAgent`.
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
- Known limits: the host's own input gate is not independent of the browser. It enables input when the client sends `first-frame` `presented` for the current request and generation, and trusts that message; the RTP-timestamp proof runs only in the browser. A modified client can therefore claim presentation early; what the host still enforces is the generation match, the secure-desktop input policy of the helper or Agent, and the new-peer gate above. A browser tab in the background presents no rVFC frames and therefore falls back to a restart after the watchdog. Not yet qualified live with a real UAC prompt.

## Decisions

- 2026-10-12: WebRTC desktop-source transitions happen in place, gated on the admitted IDR's RTP timestamp, instead of restarting the peer. Reason: the restart showed as a disconnect on every UAC prompt; the RTP timestamp gives native video the generation proof it lacked. Supersedes the "desktop-source transitions restart the WebRTC peer" rule.

- 2026-10-08: Admin mode moves from user-writable `tray.json` to an admin-only HKLM value. Reason: a remote viewer controls the user's desktop and could flip the tray menu item, gaining input on UAC prompts and the lock screen. Supersedes the earlier "residual risk" note.

## Open questions

- Which cancel mechanism (targeted `WM_CLOSE` or `IDCANCEL` on the consent window) works on Windows 11 24H2+; must be qualified live and fail closed.
