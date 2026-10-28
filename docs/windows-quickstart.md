# BubblePlay for Windows: first connection

This is the quickstart for an unsigned Windows/browser alpha candidate. A public download is not available yet. Choose the Go/Docker relay with authenticator codes and optional ntfy, or the existing Cloudflare relay with ntfy approval. Docker runtime and a live desktop connection against Go remain release checks.

## Requirements

- Windows x64 with an NVIDIA GPU and its current display driver. AMD, Intel and software encoding are not supported by the demonstrated host path.
- A desktop Chromium browser for the receiver. HTTPS is required for normal internet access and browser clipboard features.
- Your own configured BubblePlay relay. The relay address and its two host credentials are required. In a source checkout, relay setup guides are `docs/self-host-docker.md` and `docs/self-host-cloudflare.md`; this packaged quickstart assumes that setup is complete.
- A visible Windows desktop or an existing compatible virtual display. The download does not install a display or mouse driver.

The host runs as the signed-in Windows user. Secure desktop capture, including lock screens and UAC prompts, needs the secure-desktop helper. The default install adds it, and `-SkipAgent` leaves it out. Live acceptance of that capture is pending.

Use infrastructure you trust: TLS protects each WebSocket connection, but the relay can inspect stream, input, clipboard and file payloads. WebRTC media uses its own encrypted transport. Browser approval/password credentials are separate from host registration credentials.

## Install the candidate ZIP

Extract the complete ZIP into a folder. Keep its scripts beside the three executables. Open a normal PowerShell in that folder and enter your own relay address and credentials:

```powershell
$relay = Read-Host 'Your relay URL, for example https://bubbleplay.example.com'
$registration = Read-Host 'Relay TOKEN' -AsSecureString
$hostEndpoint = Read-Host 'Relay HOST_GATE_TOKEN' -AsSecureString
```

Both host credentials must contain at least 32 characters and must match the relay configuration. They are separate from the browser login password.

Preview installation, then install without the optional elevated input helper:

```powershell
.\setup-tray.ps1 -SourceDir $PWD.Path -WorkerUrl $relay -Token $registration -HostGateToken $hostEndpoint -SkipAgent -WhatIf
.\setup-tray.ps1 -SourceDir $PWD.Path -WorkerUrl $relay -Token $registration -HostGateToken $hostEndpoint -SkipAgent
Remove-Variable registration, hostEndpoint
```

`-SkipAgent` installs the normal per-user host and tray. Do not use it to downgrade an existing installation that relies on the elevated helper: it also removes an installed helper. For a new installation that needs input in elevated apps, omit `-SkipAgent`; setup requests one UAC prompt for the optional input helper and any necessary legacy cleanup. This helper only injects validated Windows input; it is separate from the private Node companion.

Windows may warn about the unsigned binaries. The candidate does not bypass those warnings. Compare the ZIP's SHA256 with the published receipt before running it once a release exists.

## Connect

1. Confirm the BubblePlay tray is running on the host PC.
2. Open your relay's HTTPS address in the receiver browser.
3. Approve the browser through ntfy if asked, then create or unlock your device key and pick the PC ([browser viewer](browser-viewer.md)). On Go, the relay first asks for the application password and an authenticator code, or ntfy approval and then the password. On Cloudflare, it asks for ntfy approval and then the password.
4. Wait for the desktop stream.

The Files menu uploads files into the host user's `Downloads\BubblePlay`. Desktop drag/drop can pass virtual files to a compatible Windows drop target directly. Native cursor shapes and clipboard synchronization work while input is ready. Image clipboard writes may need the browser's clipboard permission or the existing clipboard retry action.

## Update

Extract a newer ZIP and run from that extracted folder:

```powershell
.\update-tray.ps1 -SourceDir $PWD.Path
```

The tray keeps the previous host for rollback. Admin input is a separate optional component; update only installs it when `-IncludeAgent` is supplied. A candidate ZIP is not an automatic updater service.

## Remove or diagnose

Remove BubblePlay through Windows Settings > Apps, or run `uninstall-tray.ps1` from its install folder. The uninstall retains user configuration unless `-RemoveData` is explicitly supplied.

Installed executables live in `%LOCALAPPDATA%\Programs\BubblePlay`. Configuration and bounded logs live in `%LOCALAPPDATA%\BubblePlay`. Host credentials are protected with CurrentUser DPAPI. Keep raw configuration, relay secrets and unrelated desktop/file content out of bug reports.

This guide has been checked against source and a locally built package. A clean second-PC installation remains a release acceptance check.
