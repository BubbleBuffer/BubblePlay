# BubblePlay

Stream your Windows desktop to a browser, with native cursor shapes, keyboard and mouse input, clipboard sharing, and file transfer. Use WebRTC or WebSocket streaming through your own relay.

BubblePlay is being prepared for its first open-source alpha under the GNU Affero General Public License v3.0 or later (AGPL-3.0-or-later). The current Windows/browser path works. You run your own relay: a small Go/Docker service, or a Cloudflare Worker. A browser passes the relay's ntfy approval, then unlocks or creates a device key and picks your PC ([browser viewer](docs/browser-viewer.md)). The Go and Cloudflare relays still ask for an application password until their cutover ([browser approval](docs/architecture/browser-approval.md)). The public GitHub release is still pending.

## Get started

The Windows host currently requires **Windows x64, an NVIDIA GPU and its display driver**. Use a desktop Chromium browser as the receiver. The host runs as the signed-in Windows user; an optional elevated helper enables input in elevated applications.

- [Windows installation and first connection](docs/windows-quickstart.md)
- [Go/Docker self-hosting](docs/self-host-docker.md)
- [Cloudflare self-hosting](docs/self-host-cloudflare.md)
- [Documentation](docs/README.md)
- [Build and development checks](docs/development.md)
- [Architecture and feature contracts](docs/architecture/README.md)
- [Android client](android/README.md)

Docker runs the site, authentication and relay. Desktop capture and encoding run on the native Windows host. The Go implementation has automated authentication/protocol checks and a browser-login smoke test; actual Docker runtime and live desktop-stream acceptance remain pending.

See [release status](docs/release-status.md) for the remaining acceptance checks. Contributors can start with the [repository map and checks](docs/development.md).

## Desktop interaction

The browser translates Windows cursor types into native local cursors. Clipboard sharing supports text and images, subject to browser permissions. Drag files onto a compatible remote Windows application to supply virtual files, or use the Files menu to upload them into `Downloads\BubblePlay`. Mobile interaction has its own client and validation scope.

Remote input acts with the permissions of the Windows user. Secure desktops, including lock screens and UAC prompts, are outside the capture path. AMD, Intel and software encoding have not been demonstrated. The first Windows package is unsigned.

## Privacy

TLS protects each WebSocket connection. A WebSocket relay can read stream, input, clipboard and file payloads, so run it on infrastructure you trust. WebRTC media uses its own encrypted transport. See [security and privacy](docs/security-and-privacy.md) for authentication and platform boundaries.

Private command-execution companion components are excluded from the public relay and Windows package. The optional Windows admin-input helper has a separate, limited purpose.

## License

BubblePlay is licensed under the [GNU Affero General Public License v3.0 or later](LICENSE). If you run a modified relay or host for other people, you must offer them its source. Third-party components retain their own licenses; see [dependency notices](docs/THIRD-PARTY-NOTICES.md).
