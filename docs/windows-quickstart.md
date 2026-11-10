# BubblePlay for Windows: first connection

This is the quickstart for an unsigned Windows/browser alpha candidate. A public download is not available yet. Set up the Rust relay ([relay setup](../relay-rs/README.md)) in Docker or on Cloudflare Workers; each new browser is approved through an ntfy notification. Docker runtime and a live desktop connection against the Rust relay remain release checks.

## Requirements

- Windows x64 with an NVIDIA GPU and its current display driver. AMD, Intel and software encoding are not supported by the demonstrated host path.
- A desktop Chromium browser for the receiver. HTTPS is required for normal internet access and browser clipboard features.
- Your own configured BubblePlay relay. The relay address and its `TOKEN` are required. In a source checkout, the relay setup guide is `relay-rs/README.md`; this packaged quickstart assumes that setup is complete.
- A visible Windows desktop or an existing compatible virtual display. The download does not install a display or mouse driver.

The host runs as the signed-in Windows user. Secure desktop capture, including lock screens and UAC prompts, needs the secure-desktop helper. The default install adds it, and `-SkipAgent` leaves it out. Live acceptance of that capture is pending.

Use infrastructure you trust: TLS protects each WebSocket connection, but the relay can inspect stream, input, clipboard and file payloads. WebRTC media uses its own encrypted transport.

## Install the candidate ZIP

Extract the complete ZIP into a new folder, keeping all executables, scripts and runtime assets together. Open a normal PowerShell as the desktop user in that folder and enter your own relay address and TOKEN:

```powershell
$relay = Read-Host 'Your relay host WebSocket URL, for example wss://bubbleplay.example.com/host-ws'
$registration = Read-Host 'Relay TOKEN' -AsSecureString
```

`TOKEN` must contain at least 32 characters and must match the relay's `TOKEN`.

Preview installation, then install without the optional elevated input helper:

```powershell
.\setup-tray.ps1 -SourceDir $PWD.Path -WorkerUrl $relay -Token $registration -SkipAgent -WhatIf
.\setup-tray.ps1 -SourceDir $PWD.Path -WorkerUrl $relay -Token $registration -SkipAgent
Remove-Variable registration
```

Setup requests one UAC prompt to install all shipped executables, scripts and runtime assets under `%ProgramFiles%\BubblePlay`; the privileged helpers remain in its `agent` subfolder. Configuration, user data, autostart registration and receipts are written by the original unelevated desktop user under `%LOCALAPPDATA%\BubblePlay` or HKCU. Approve UAC using the same account, and start setup from the newly extracted release you chose to install.

`-SkipAgent` leaves out the optional privileged helpers. Do not use it to downgrade an existing installation that relies on them: it also removes installed helpers. For secure desktop capture or input in elevated apps, omit `-SkipAgent`. The input helper only injects validated Windows input; it is separate from the private Node companion. When migrating an older `%LOCALAPPDATA%\Programs\BubblePlay` installation, setup verifies the protected machine copy before removing the old program files; it retains the user's data.

Windows may warn about the unsigned binaries. The candidate does not bypass those warnings. Compare the ZIP's SHA256 with the published receipt before running it once a release exists.

## Connect

1. Confirm the BubblePlay tray is running on the host PC.
2. Open your relay's HTTPS address in the receiver browser.
3. Approve the browser in the ntfy notification, then create or unlock your device key ([browser viewer](browser-viewer.md)).
4. Pair this device. In the tray, open **Devices** and click **Pair device**. Scan the QR code it shows with the phone camera. Check that the 6-digit number on the phone matches the one on the PC, then approve the request on the PC.
5. Pick the PC and wait for the desktop stream.

The Files menu uploads files into the host user's `Downloads\BubblePlay`. Desktop drag/drop can pass virtual files to a compatible Windows drop target directly. Native cursor shapes and clipboard synchronization work while input is ready. Image clipboard writes may need the browser's clipboard permission or the existing clipboard retry action.

## Update

Extract a newer ZIP and run from that extracted folder:

```powershell
.\update-tray.ps1 -SourceDir $PWD.Path
```

The updater uses the new release's setup script and requests one UAC prompt for the protected machine files, while user settings and receipts remain unelevated. Full updates refresh both privileged helpers; `-IncludeAgent` remains accepted for compatibility. To restore an older version, run setup from that trusted release; the unelevated tray cannot replace Program Files binaries. Tray helper repair can only use the setup source already protected under Program Files; if that source is unavailable or untrusted, extract a new release and run its `setup-tray.ps1`. A candidate ZIP is not an automatic updater service.

## Remove or diagnose

Remove BubblePlay through Windows Settings > Apps, or run `uninstall-tray.ps1` from its install folder. The uninstall retains user configuration unless `-RemoveData` is explicitly supplied.

Installed executables, scripts and runtime assets live in `%ProgramFiles%\BubblePlay`, with privileged helpers in `agent`. Configuration, bounded logs and receipts live in `%LOCALAPPDATA%\BubblePlay`. Host credentials use CurrentUser DPAPI with fixed BubblePlay application entropy; valid legacy blobs migrate on their next successful load. Keep raw configuration, relay secrets and unrelated desktop/file content out of bug reports.

A clean second-PC installation and update of the protected Program Files layout remain release acceptance checks.
