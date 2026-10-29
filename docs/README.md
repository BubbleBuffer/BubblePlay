# BubblePlay documentation

Start with a relay setup guide, then install the native Windows host. BubblePlay is preparing its first Windows/browser alpha; no public release has been published yet.

- [Windows quickstart](windows-quickstart.md) — relay credentials, installation, first connection, update and removal.
- [Relay setup](../relay-rs/README.md) — the Rust relay: Docker image, Cloudflare Worker, configuration and ntfy approval.
- [Security and privacy](security-and-privacy.md) — authentication, relay visibility and platform boundaries.
- [Development](development.md) — contributor setup, repository map, builds and checks.
- [Architecture](architecture/README.md) — runtime components, transports and shared fixtures.
- [PC sound](architecture/audio.md) — host capture, Opus, WebRTC/WSS audio and the tray setting.
- [File transfer](architecture/file-transfer.md), [monitor following](architecture/follow-active-window.md) and [session UI](architecture/session-ui.md) — current feature behavior and contracts.
- [Secure desktop](architecture/secure-desktop.md) — UAC prompts and lock screen; [in-place desktop transitions on WebRTC](architecture/secure-desktop.md#in-place-desktop-transitions-on-webrtc).
- [Unattended access](architecture/unattended-access.md) — remote access before sign-in (pre-login service and restricted logon host); approved 2026-10-13, waits for device keys.
- [Device IDs and keys](architecture/device-keys.md) — per-device key files and IDs, pairing, routing by ID, trusted-device list in the tray, ntfy site gate; approved 2026-10-13.
- [Third-party notices](THIRD-PARTY-NOTICES.md) — locked Windows dependencies and retained license texts.
- [Release status](release-status.md) — available evidence, alpha limits and acceptance still required.
- [Roadmap](roadmap.md) — planned and deferred work beyond the release blockers, with status per entry.
- [Android](../android/README.md) — separate mobile client setup and validation scope.

Docker runs the relay and browser site; Windows capture and encoding stay native. The Rust relay has automated checks. Container execution and native streaming against it remain [release acceptance checks](release-status.md).
