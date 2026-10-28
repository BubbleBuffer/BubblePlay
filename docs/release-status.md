# Release status

As of 2026-10-12, BubblePlay is preparing its first open-source Windows/browser alpha under [AGPL-3.0-or-later](../LICENSE). No public release has been published.

The first milestone is one operator, one native Windows host and one desktop browser viewer. The Rust relay (`relay-rs/`, in Docker or on Cloudflare Workers) provides the site gate and relay; each new browser is approved through an ntfy notification. Docker does not contain Windows capture or encoding. Multi-user accounts, public command execution and endpoint-to-endpoint WSS encryption are separate work.

## Available evidence

- The Windows/browser path has local build and live interaction evidence, including native cursor handling and Chrome/Explorer file drops. A candidate ZIP packages the host, tray, optional admin-input helper, secure-desktop helper, setup/update/removal scripts, license, notices and a hash manifest.
- Before CUT-1 deleted the Go relay, its authentication/protocol tests and vet passed; a static Linux build and disposable browser-login smoke test succeeded. Those tests included real local WebSocket pairs and browser-generated packet fixtures. These results do not establish a native Windows desktop stream through the relay.
- Windows credential/setup and release-packaging checks use synthetic fixtures. The browser checks and the JavaScript Worker dry-run build passed before CUT-1 deleted that relay. CI definitions exist; a hosted CI run has not been demonstrated.

## Before publication

1. Complete independent operator setup on the documented Docker image (`relay-rs/Dockerfile`). The earlier Go image was built and run on a Linux Docker host on 2026-10-12, with health, login and WebSocket checks passing; the Rust image has no live run yet, and the native Windows stream through it is still item 2.
2. Connect the native Windows host and Chromium receiver through the relay: verify frames, cursor/input, clipboard, file transfer, held large native drops, reconnects and slow-peer behavior. Test the intended WebRTC/TURN or restrictive-network routes separately.
3. Prove a clean Windows installation and removal on another PC. Android needs its own packaged/device acceptance.
4. Publish versioned source/downloads with hashes and notices.

The target is Windows x64, NVIDIA NVENC and desktop Chromium. AMD, Intel and software encoding are not demonstrated. Windows binaries are unsigned. Secure desktops, including lock screens and UAC prompts, are captured by a SYSTEM helper; the implementation is done, but live acceptance is pending ([secure desktop](architecture/secure-desktop.md)). See [security and privacy](security-and-privacy.md) for relay visibility and elevated-input boundaries.

Local deployment logs, owner infrastructure, benchmark artifacts and older troubleshooting records are not public setup instructions. Current guides live in [this documentation tree](README.md); acceptance records should identify the exact build, environment and checks performed.
