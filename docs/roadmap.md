# Roadmap

Work beyond the [release blockers](release-status.md#before-publication), from a gap review on 2026-10-12. Each entry has a status: proposed, approved YYYY-MM-DD, in progress, done YYYY-MM-DD, deferred or dropped. Done entries link to the page that now describes the feature.

Current focus: device keys phase 2 with a Rust relay that replaces both relays (in progress on `lane/rust-relay`); then Wake-on-LAN and unattended access, which build on device keys.

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
Status: done 2026-10-13 for all four: follow active window (including edge-hold drags), game mode, background survival and keyboard lock.
Built on lane/android-parity and merged on 2026-10-13: follow active window in the switch-screen popover, Work/Game pointer modes (Game uses Android pointer capture), keyboard capture (Esc and Win combinations; Android keeps Win alone, Alt+Tab and Print Screen), and a `dataSync` foreground service that keeps the session and transfers alive in the background with a Disconnect notification, falling back to the 30 s background stop. Still browser-only: desktop file drops with preview.

### Compose UI and Game controls
Status: planned; starts after the encoders and Windows-client lanes are further along.
Decided 2026-10-14: the Android UI (today plain Views built in code, no layouts) moves to Jetpack Compose step by step, through ComposeView interop. The SurfaceView video path and the networking stay as they are. The session overlays and the new Game controls come first. Ruled out: a WebView UI (the touch routing and the JS bridge sit in the game-input path), egui on Android (weak IME), Flutter (rewrite). Game controls: an on-screen gamepad and virtual keys with editable layouts, a key bar (Esc, Tab, Win, F-keys, modifiers), real key down/up from hardware keyboards and gamepads, and touch controls hidden while a physical controller is in use. Shares design tokens with the browser and Windows clients. Claude designs it with mockups for the owner first; no Codex for UI.

### Typing over laggy connections
Status: proposed; investigate first.
Idea from 2026-10-12: on a slow link, typed keys could be buffered and sent in chunks instead of one packet per key. Control packets are already queued in order, so first measure what goes wrong today: lost keys, reordering, or only delay. Then decide between batching key packets and sending text chunks. Applies to Android first, the browser if the same issue shows.
Idea from 2026-10-13: while keys are buffered or in flight on a slow link, show a small, unobtrusive bar at the bottom edge ("Sending 12 keys…") so the user sees the input was taken and is on its way. It hides when the queue drains and never covers the stream's centre.

### Key events in Game mode
Status: proposed; investigate first.
Idea from 2026-10-13: Game mode is underdeveloped. It should send physical key down and up events (scancodes, held state, no auto-repeat from the client) instead of text input, as games expect. Text-style input stays for Work mode. First check what each client sends today (browser, Android, the Windows client). Open points: keyboard layout (scancodes follow the host's layout, so text typed into a game's chat box comes out in the host layout), keys held when focus or the connection is lost (release them all), and Android, whose soft keyboard only gives text.

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
Status: approved 2026-10-13 with decisions; phase 2 built (device keys, pairing, Devices window, ctl channel, Rust relay admission); phase 3 still open (per-device permissions, pre-login access).
Each device (PC host, browser, phone, Wake-on-LAN helper, pre-login logon host) has an Ed25519 key in a portable key file (export, import, optional passphrase) and an ID derived from its public key. The relay routes by ID; the endpoints authenticate each other's keys, like SSH `authorized_keys`. The trusted-device list lives on the PC and is managed in the tray. Device keys are the only way to connect: the application password and its login are deleted without a migration period. Remote pairing is off by default and approved at the PC or by a voucher from an already-paired device; a device can vouch for its own new key to rotate its ID. ntfy approval stays as a gate in front of the browser site. Phase 2 unblocks Wake-on-LAN, phase 3 unattended access. Design and decisions: [device-keys.md](architecture/device-keys.md).

### Wake-on-LAN
Status: approved 2026-10-12: a small LAN helper sends the packet.
An asleep PC cannot be reached. A magic packet has to come from a device on the same LAN, because the relay and the viewer are not on it. A small helper runs on any always-on LAN box (Docker image or a single binary), stays connected to the relay with its own credential, and sends the magic packet when an authenticated viewer asks. The host registers its MAC address while awake. The helper authenticates as a paired device under device IDs and keys. Open: the viewer's wake button when the host is offline.

### Unattended access
Status: approved 2026-10-13 (option B). Waits for device IDs and keys, then an NVENC-from-session-0 probe.
The host starts at Windows sign-in, so a signed-in user is required. Lock-screen and UAC capture are helper-based and pending live acceptance. A service-based host that works before sign-in is a larger change with its own security review. Design and decisions: [unattended-access.md](architecture/unattended-access.md).

### More encoders and host platforms
Status: done 2026-10-14 for Windows (merged from lane/encoders).
Encoding is pluggable:
- NVENC first.
- Then a Media Foundation hardware encoder, which is experimental and untested on AMD and Intel.
- Then the Windows software H.264 encoder ("CPU mode"), which can also be forced from the tray.

On a GPU-less test VM, CPU mode streams at the VM's capture rate (about 22 fps) with 25 ms mean latency, using 3-6% host CPU. Streaming the UAC secure desktop in CPU mode is not working yet; the cause is under investigation. See [encoders.md](architecture/encoders.md). macOS and Linux hosts are not planned for the first alpha.

## Documentation

### Stale statements
Status: done 2026-10-12.
- [release-status.md](release-status.md) is dated 2026-10-06 but cites 2026-10-12 evidence, and still says secure desktops are outside capture.
- [SECURITY.md](../SECURITY.md) refers to "the latest release" while no public release exists.
