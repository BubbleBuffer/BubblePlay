# BubblePlay documentation

Start with a relay setup guide, then install the native Windows host. BubblePlay is preparing its first Windows/browser alpha; no public release has been published yet.

- [Windows quickstart](windows-quickstart.md) — relay credentials, installation, first connection, update and removal.
- [Go/Docker self-hosting](self-host-docker.md) — one service, authenticator login and optional ntfy approval.
- [Cloudflare self-hosting](self-host-cloudflare.md) — deploy your own existing relay and approval service.
- [Security and privacy](security-and-privacy.md) — authentication, relay visibility and platform boundaries.
- [Development](development.md) — contributor setup, repository map, builds and checks.
- [Architecture](architecture/README.md) — runtime components, transports and shared fixtures.
- [File transfer](architecture/file-transfer.md), [monitor following](architecture/follow-active-window.md) and [session UI](architecture/session-ui.md) — current feature behavior and contracts.
- [Secure desktop](architecture/secure-desktop.md) — UAC prompts and lock screen; [in-place desktop transitions on WebRTC](architecture/secure-desktop.md#in-place-desktop-transitions-on-webrtc).
- [Unattended access](architecture/unattended-access.md) — proposal for remote access before sign-in (pre-login service and restricted logon host); not approved.
- [Third-party notices](THIRD-PARTY-NOTICES.md) — locked Windows dependencies and retained license texts.
- [Release status](release-status.md) — available evidence, alpha limits and acceptance still required.
- [Roadmap](roadmap.md) — planned and deferred work beyond the release blockers, with status per entry.
- [Android](../android/README.md) — separate mobile client setup and validation scope.

Docker runs the relay and browser site; Windows capture and encoding stay native. The Go relay has automated checks and a browser-login smoke test. Container execution and native streaming against Go remain [release acceptance checks](release-status.md).
