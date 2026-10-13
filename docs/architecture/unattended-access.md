# Unattended access (before sign-in)

Status: approved 2026-10-13 (option B). Implementation waits for the [device IDs and keys](device-keys.md) design and the NVENC probe. Roadmap entry: [Unattended access](../roadmap.md#unattended-access).

Goal: a viewer can reach the PC when no user is signed in (after a reboot, after sign-out, at the "switch user" screen) and sign in remotely. Related: [secure desktop](secure-desktop.md), [security and privacy](../security-and-privacy.md).

## Current state

Nothing from BubblePlay runs until a user signs in locally. Evidence (code reads on 2026-10-12, not live tests unless noted):

| Situation | Today | Why |
| --- | --- | --- |
| Boot, before any sign-in | No host, no relay registration. | The tray starts from `HKCU\...\CurrentVersion\Run` value `BubblePlayTray` ([setup-tray.ps1](../../host/scripts/setup-tray.ps1) lines 111-112, 424-431). There is no service and no boot-triggered task in the supported install. |
| Signed in, locked | Session stays up; helper captures Winlogon while a viewer is connected. Admin mode off: view only. Admin mode on: full input, so the viewer types the lock-screen password. Pending live acceptance on the lock screen. | The user host keeps running; `\BubblePlay\SecureDesktop` launches `bubbleplay_secure_helper.exe` per viewer ([secure-desktop.md](secure-desktop.md#current-state), `host/src/secure_helper/runtime.rs` 45-108, `secure_helper/desktop.rs` 247-254). Lock detection: `WTS_SESSIONSTATE_LOCK` in `host/src/desktop_session.rs` 499-527. |
| After sign-out | Viewer disconnects; nothing reconnects until the next local sign-in. | Tray, host and admin agent are user processes and end with the session. The helper exits on the console change or after 30 s without a host (`secure_helper/runtime.rs` 524-528). |
| After reboot | Same as boot. | As above. |
| Fast user switching | User A's host keeps running in its disconnected session but cannot capture or inject; the new console user has no host unless they installed the tray themselves. The "switch user" screen is unreachable. | Helper and admin agent refuse a client whose session or SID is not the console's (`secure_helper/runtime.rs` 118-153, 235; `bin/bubbleplay_admin_agent.rs` 249-250, 408-413). Session changes are polled; nothing registers for `WM_WTSSESSION_CHANGE` or `SERVICE_CONTROL_SESSIONCHANGE` in the host or tray (inferred from a grep of `host/src`). |

What the existing pieces already provide:

- `bubbleplay_secure_helper.exe` (SYSTEM, console session, no network listener) captures Winlogon with GDI, composites the cursor, hands BGRA frames to the host over shared memory, and applies pointer, keyboard and Unicode input under policy. Its pipe `\\.\pipe\BubblePlay.SecureHelper.s<session>` allows SYSTEM and the console user SID only (`secure_helper/trust.rs` 259-261). Its input policy is Admin mode, which is bound to the console user's SID (`HKLM\SOFTWARE\BubblePlay` `AdminMode`/`AdminModeSid`, `host/src/admin_mode.rs`). Before sign-in there is no console user, so today it has no client and no policy.
- The user host already encodes helper frames with its own encoder and runs the secure-desktop state/banner protocol ([secure-desktop-contract.json](../../fixtures/secure-desktop-contract.json)).
- The retired service path (`install-service.ps1`: `BubblePlayBroker` as LocalSystem, `BubblePlayTransport` as LocalService with its own service SID; `host/src/service_runtime.rs`) already follows the active console session and launches a process into it. It is excluded from the release and was never validated on Winlogon. Its LocalService transport split is a useful precedent.
- No BubblePlay process accepts inbound network connections; all relay traffic is outbound.

## Options

### A. No code: Windows auto sign-in, then lock

The owner configures Windows automatic sign-in (Sysinternals Autologon stores the password as an LSA secret) and a scheduled lock at logon. After a reboot the user session, tray and host start; the lock screen is then handled by the existing helper path.

- Changes: a documented procedure only.
- New attack surface: the Windows password is stored on the PC; anyone with physical access during the boot-to-lock window gets the desktop. No new BubblePlay code.
- Admin mode / secure desktop: unchanged. Unlocking remotely needs Admin mode on (today's rule).
- Install/update/removal: none.
- Size: half a day of docs and a live check.
- Does not cover sign-out or the "switch user" screen.

### B. Pre-login service plus a restricted logon host (recommended)

A small SYSTEM service `BubblePlayLogon` with no network code supervises sessions. When the console session has no signed-in user (boot, sign-out, switch-user screen, detected with `SERVICE_CONTROL_SESSIONCHANGE` plus `WTSQueryUserToken` failing), it:

1. launches the existing secure helper in the console session (as `--launch` does today), with a pre-login policy instead of Admin mode, and
2. starts a logon host: `bubbleplay-host.exe` in a pre-login mode, running as a restricted service account (`NT SERVICE\BubblePlayLogon`, not SYSTEM). It holds the relay connection, encodes helper frames and forwards input requests to the helper.

When a user signs in, the service stops the logon host; the viewer is disconnected, the user's tray starts from the Run key as today, and the viewer reconnects to the user host.

- Changes: new service binary (session following can be lifted from `service_runtime.rs`); host pre-login mode with file transfer, clipboard, monitor follow and the admin agent compiled out or refused; helper trust check extended to accept the logon host's service SID from session 0 when the console has no user; a machine-scoped host credential; relay support for a pre-login flag and fresh approval (Go and Cloudflare, plus a fixture); tray/setup UI for the opt-in.
- New attack surface: an always-on SYSTEM service (local only, no network); a network-connected process that runs before anyone is signed in, parsing relay signaling and WebRTC/WSS, but as a service account without SYSTEM rights; input on the sign-in screen, which means the viewer types a Windows password or PIN that passes through the relay as key events (a WSS relay sees them above TLS). A compromised logon host can still drive the helper within the pre-login policy, that is, type on the sign-in screen.
- Admin mode / secure desktop: Admin mode keeps governing the signed-in user's lock screen and UAC. Pre-login gets its own machine-wide value (`HKLM\SOFTWARE\BubblePlay\PreLogin`), set only through UAC, cleared by a SYSTEM task like `AdminModeOff`. The helper reads it per request, as it reads Admin mode.
- Install/update/removal: the existing elevated child installs the service, its account and ACLs, and `ProgramData\BubblePlay\prelogin` credentials (ACL: SYSTEM and the service SID). Update stops the service before replacing Program Files binaries and is covered by rollback. Uninstall removes the service, the value and the credentials. `package-release.ps1` requires the new binary.
- Size: medium. Roughly 1.5-3k lines of Rust, a few hundred lines of PowerShell, small relay and client changes, plus live acceptance on reboot, sign-out and switch-user.
- Unknown to probe first: whether NVENC encodes from a session-0 service account (no display attached). If not, the logon host runs in the console session under the service account, or encoding moves to a lower-rate software path for this mode.

### C. Full SYSTEM host (the Sunshine/RustDesk model)

A SYSTEM service always runs the host as SYSTEM in the active console session, covering pre-login, lock, UAC and the user desktop with one process; the tray becomes UI only.

- Changes: replaces the current privilege model; effectively revives the retired broker/agent path.
- New attack surface: the whole host (signaling, WebRTC, file transfer, clipboard, input) is network-connected SYSTEM code, before and after sign-in. Remote input as SYSTEM on the user's desktop bypasses UAC by design.
- Admin mode / secure desktop: Admin mode loses its meaning unless re-enforced inside the SYSTEM host; the user/helper separation built in Milestones A and B goes away.
- Install/update/removal: per-machine install only; per-user setup and update scripts are replaced.
- Size: large, plus a new security review of everything.

### D. SYSTEM logon host (variant of B)

Same as B, but the logon host runs as SYSTEM in the console session and captures directly. Smaller than B (no new trust path in the helper), but the relay-facing parser becomes SYSTEM code. Listed to make the trade-off explicit; not recommended.

## Recommendation

Approve option B, built in this order; each step is separately reviewable:

1. Probe: NVENC from a session-0 service account, and the helper capturing the Winlogon sign-in screen with no user signed in (dev binaries only).
2. Minimum viable step: boot and sign-out only, WebRTC and WSS, view plus pointer and keyboard on the sign-in screen, behind the opt-in. Switch-user screen and Ctrl+Alt+Del (`SendSAS`) come later.
3. Optional A in the docs for owners who do not want a service.

Required controls for step 2:

- Opt-in: off by default. Turned on in the tray through UAC; off in one click via a SYSTEM task; greyed out while a viewer is connected (same pattern as Admin mode). With it off, the service does not start the logon host at all.
- Separate approval: the logon host registers as pre-login; the relay requires a fresh ntfy approval, sent by the PC itself, per pre-login session and never accepts a remembered browser approval for it. The viewer sees "PC is at the sign-in screen".
- Audit and notification: Windows Application event log entries for pre-login session start and end, approval method and viewer address; after the next local sign-in the tray shows "Someone connected before sign-in at HH:MM". Optional ntfy message on start.
- Credential handling: BubblePlay never stores, captures, logs or auto-types Windows passwords or PINs. Logs keep counts only, as today. The machine relay credential is separate from the user's DPAPI-protected credential and can be revoked on its own.
- Narrow pre-login mode: no file transfer, no clipboard, no admin agent, no process launch, no monitor follow; input only to the Winlogon desktop; the session ends when a user signs in.
- Stays prohibited: storing or replaying Windows credentials, creating or changing accounts, changing sign-in settings, running anything in session 0 that accepts inbound connections, and any SYSTEM process that parses relay traffic.

## Open questions for the owner

1. Is pre-login worth a permanent SYSTEM service, or is option A (auto sign-in plus lock) enough for now?
2. Should the logon host use the same relay room and host credential as the user host (simpler for the viewer), or a separate pre-login identity (cleaner revocation and approval rules)?
3. Is input on the sign-in screen required for the first step, or is view only (to confirm the PC is up after a reboot) useful on its own?
4. Should the pre-login opt-in require Admin mode to be on as well, or stay independent?
5. Decided: pre-login sessions use a fresh ntfy approval sent by the PC itself, no authenticator codes (decision 5).
6. Is the switch-user screen in scope, given that fast user switching then hands the console between two hosts?
7. If NVENC does not work from session 0, accept a lower-quality software encoder for pre-login, or run the logon host in the console session?

## Decisions

2026-10-13, owner approved option B with the recommended answers:
1. Option B: the pre-login service plus a restricted logon host. Option A stays a documented workaround.
2. The logon host has its own identity, separate from the user host, so it can be revoked and approved on its own terms.
3. The first step includes input on the sign-in screen; view-only alone is not the goal.
4. The pre-login opt-in is independent of Admin mode.
5. Only already-paired devices may connect before sign-in, and each session still needs a fresh ntfy approval that the PC itself sends (see [device IDs and keys](device-keys.md); the PC sends that approval, while pairing needs a confirmation at the PC or a voucher from a paired device, never ntfy). Pairing is not possible before sign-in. This depends on [device IDs and keys](device-keys.md).
6. The switch-user screen is out of scope for the first version.
7. Open until the NVENC probe: if NVENC fails from session 0, choose between a console-session logon host and a software encoder with the probe results.
