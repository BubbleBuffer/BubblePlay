# Release status

As of 2026-10-06, BubblePlay is preparing its first open-source Windows/browser alpha under [AGPL-3.0-or-later](../LICENSE). No public release has been published.

The first milestone is one operator, one native Windows host and one desktop browser viewer. A Go/Docker service provides the site, authentication and relay, with an application password plus an operator-chosen second factor: authenticator codes, ntfy approval or approval on the PC. Cloudflare remains an alternative. Docker does not contain Windows capture or encoding. Multi-user accounts, public command execution and endpoint-to-endpoint WSS encryption are separate work.

## Available evidence

- The Windows/browser path has local build and live interaction evidence, including native cursor handling and Chrome/Explorer file drops. A candidate ZIP packages the host, tray, optional admin-input helper, setup/update/removal scripts, license, notices and a hash manifest.
- Go authentication/protocol tests and vet pass; a static Linux build and disposable browser-login smoke test succeeded. Tests include real local WebSocket pairs and browser-generated packet fixtures. These results do not establish a native Windows desktop stream through Go.
- Windows credential/setup and release-packaging checks use synthetic fixtures. The browser checks and Cloudflare dry-run build pass. CI definitions exist; a hosted CI run has not been demonstrated.

## Before publication

1. Complete independent operator setup on the documented Docker image. The image was built and run on a Linux Docker host on 2026-10-12, with health, login and WebSocket checks passing; the native Windows stream through it is still item 2.
2. Connect the native Windows host and Chromium receiver through Go: verify frames, cursor/input, clipboard, file transfer, held large native drops, reconnects and slow-peer behavior. Test the intended WebRTC/TURN or restrictive-network routes separately.
3. Prove a clean Windows installation and removal on another PC. Android needs its own packaged/device acceptance.
4. Review the exact source and publication history, run hosted CI, select a private vulnerability-reporting channel, and publish versioned source/downloads with hashes and notices.

The target is Windows x64, NVIDIA NVENC and desktop Chromium. AMD, Intel and software encoding are not demonstrated. Windows binaries are unsigned; secure desktops, including lock screens and UAC prompts, are outside capture. See [security and privacy](security-and-privacy.md) for relay visibility and elevated-input boundaries.

Local deployment logs, owner infrastructure, benchmark artifacts and older troubleshooting records are not public setup instructions. Current guides live in [this documentation tree](README.md); acceptance records should identify the exact build, environment and checks performed.
