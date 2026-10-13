# Roadmap

Work beyond the [release blockers](release-status.md#before-publication), from a gap review on 2026-10-12. Each entry has a status: proposed, approved YYYY-MM-DD, in progress, done YYYY-MM-DD, deferred or dropped. Done entries link to the page that now describes the feature.

Current focus: audio, Android parity and the device ID and key proposal in parallel; then Wake-on-LAN and unattended access, which build on device keys.

## Testing and CI

### Host checks in CI
Status: in progress — workflow added, first hosted run pending.
What runs: the `host` job in [checks.yml](../.github/workflows/checks.yml) on `windows-latest`, on every push and pull request: `node tools/check.mjs host` (cargo test and check), `cargo clippy -D warnings` and `cargo fmt --check`, on Rust 1.97.1. NVENC and NVDEC import libraries are generated from export names, since hosted runners have no NVIDIA driver. NVENC, DDA and live capture tests stay out of CI; they need a real GPU and desktop.

### Android checks in CI
Status: in progress — workflow added, first hosted run pending.
What runs: the `android` job in [checks.yml](../.github/workflows/checks.yml) on `ubuntu-latest`, on every push and pull request: `./gradlew testDebugUnitTest lintDebug` in android/ on Temurin 17. APK builds and publishing stay on build01 and the private repository.

### Live acceptance runs
Status: proposed.
Not yet run live: the 10-minute soak and stabilization list in [development.md](development.md), UAC, lock screen and in-place desktop transitions ([secure-desktop.md](architecture/secure-desktop.md#in-place-desktop-transitions-on-webrtc)), and edge-hold drags across monitors ([follow-active-window.md](architecture/follow-active-window.md)). Some steps need someone at the PC, for example to raise a UAC prompt. Goal: a scripted checklist per run, with results recorded against the exact build.

## Android

### Keep held input across monitor switches
Status: done 2026-10-12. Android now follows the browser's carry rule: held keys and buttons survive a monitor switch, and release-all is sent on resume only if something held was let go during the gate. Desktop-source changes and dead transports still release. See [follow-active-window.md](architecture/follow-active-window.md#monitor-transitions-in-the-browser).

### Feature parity with the browser
Status: approved 2026-10-12 for all four: follow active window (including edge-hold drags), game mode, background survival and keyboard lock.
The browser has these and Android does not: follow active window, game mode (captured relative mouse), keyboard lock and desktop file drops with preview. Android also stops the session 30 s after the app is backgrounded, and file transfers end with the session. Background survival means a foreground service keeps the session and transfers alive while backgrounded, within Android's limits.

### Typing over laggy connections
Status: proposed; investigate first.
Idea from 2026-10-12: on a slow link, typed keys could be buffered and sent in chunks instead of one packet per key. Control packets are already queued in order, so first measure what goes wrong today: lost keys, reordering, or only delay. Then decide between batching key packets and sending text chunks. Applies to Android first, the browser if the same issue shows.

## Features

### Audio streaming
Status: approved 2026-10-12: PC sound only, no microphone. Implemented 2026-10-13; live qualification pending. Current state: [audio.md](architecture/audio.md).
Decisions:
- The host captures the default playback device with WASAPI loopback and encodes Opus at 48 kHz stereo in 10 ms frames.
- WebRTC carries it as an audio track on the existing peer. The WSS media path carries it as its own message type, so Android gets audio too.
- The viewer plays it with a mute toggle in the session UI. Audio starts unmuted when the browser allows it, otherwise on the first click.
- The host has a setting to turn audio off. Without it, PC sound goes to the viewer whenever a session is open.
- No A/V sync beyond low latency on both streams; audio is not held back to match video.
- A new capability flag announces audio, so older clients and hosts keep working without it.
- Audio follows the session, not the desktop: it keeps playing across monitor and secure-desktop transitions.
- The off switch is the tray's **Stream PC sound** item; mute in the viewer is local and starts unmuted each session.

### Device IDs and keys
Status: approved in principle 2026-10-13; design proposal in progress.
Each device (PC host, browser, phone, Wake-on-LAN helper, pre-login logon host) gets a key pair and an ID derived from its public key. The relay routes by ID; the endpoints authenticate each other's keys, like SSH `authorized_keys`. Decisions so far: the trusted-device list lives locally on the PC and is managed there. Recommended defaults until the proposal says otherwise: keys for everyday connections, password and second factor only to pair a new device and as a fallback; one viewer may pair with several PCs; browsers use passkeys, Android uses the Keystore. Design: [device-keys.md](architecture/device-keys.md).

### Wake-on-LAN
Status: approved 2026-10-12: a small LAN helper sends the packet.
An asleep PC cannot be reached. A magic packet has to come from a device on the same LAN, because the relay and the viewer are not on it. A small helper runs on any always-on LAN box (Docker image or a single binary), stays connected to the relay with its own credential, and sends the magic packet when an authenticated viewer asks. The host registers its MAC address while awake. The helper authenticates as a paired device under device IDs and keys. Open: the viewer's wake button when the host is offline.

### Unattended access
Status: approved 2026-10-13 (option B). Waits for device IDs and keys, then an NVENC-from-session-0 probe.
The host starts at Windows sign-in, so a signed-in user is required. Lock-screen and UAC capture are helper-based and pending live acceptance. A service-based host that works before sign-in is a larger change with its own security review. Design and decisions: [unattended-access.md](architecture/unattended-access.md).

### More encoders and host platforms
Status: deferred.
Only NVIDIA NVENC on Windows x64 is supported. AMD, Intel and software encoding, and macOS or Linux hosts, are not planned for the first alpha.

## Documentation

### Stale statements
Status: done 2026-10-12.
- [release-status.md](release-status.md) is dated 2026-10-06 but cites 2026-10-12 evidence, and still says secure desktops are outside capture.
- [SECURITY.md](../SECURITY.md) refers to "the latest release" while no public release exists.
