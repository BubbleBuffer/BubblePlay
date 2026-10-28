# Browser viewer

The browser viewer shows your PC's desktop. It signs in with a device key, not a password. This page covers the viewer served by the Rust relay ([`relay-rs`](../relay-rs/README.md)). The Go and Cloudflare relays still use their password login until their cutover; see [browser approval](architecture/browser-approval.md). The protocol is in [device keys](architecture/device-keys.md).

## Site gate

The relay serves nothing of the viewer until the browser is approved.
An unapproved browser sees only a wait page: "Waiting for approval", or "Not approved" after a deny or expiry.
The relay sends the approval request to your ntfy topic as Approve and Deny links.
An approved browser keeps a `__Host-gate` cookie.
Forget this browser clears that cookie. It does not delete the device key or the paired PCs.

## Device key

When the page opens, it asks for a device key. You can:

- create a new key,
- import a key file, or
- unlock a protected key with its passphrase.

The key is kept in the IndexedDB database `bubbleplay-device`.
The unlocked private key is held in memory only.
Lock forgets the unlocked key in memory. The next use asks you to unlock it again.

## Paired PCs

Paired PCs are kept in the `pcs` store of the same database, per relay origin.
Each record has a name, an ID, the PC's pinned public key, a paired flag and a last-used time.
The PC list shows the paired PCs. Pick one, or open `?pc=<id>` to go straight to it.
Removing a PC here forgets it in this browser. The PC keeps its own list.

## Pairing a new PC

1. On the PC, open the tray menu and choose Devices, then Pair device.
2. The tray shows a code and a QR code. Both carry `https://<relay>/pair#pc=<id>&code=<code>`.
3. Open that link in the browser, or scan the QR code with it.
4. The browser reads the fragment and removes it from the address bar at once.
5. The browser checks both pairing proofs.
6. The browser then shows a six-digit comparison number.
7. The browser stores the PC only after you click Allow on the PC.

The pairing code is valid for 5 minutes.

## Each session

1. The browser sends a signed viewer hello to the relay.
2. The browser runs the handshake with the PC's pinned key. If the PC answers with a different key, the browser refuses with "PC key changed".
3. Control, input and clipboard travel on `/ws` inside an encrypted channel (AES-GCM).
4. In WebRTC mode, media and input wait until the PC's DTLS auth-bind is verified.
5. Media and file sockets use the ticket from `hello-ok`, with the roles `media-viewer` and `file-viewer`.

## Settings

Settings has two groups that matter here.

- **Device key:** export the key, change its passphrase, or remove it.
- **Browser access:** Switch PC, Lock and Forget this browser.

## Tests

- `worker/test` runs the unit tests. CI job `browser` runs them.
- `worker/test-e2e` runs the browser end-to-end tests against `bp-relay-server`. CI job `browser-e2e` runs them.
