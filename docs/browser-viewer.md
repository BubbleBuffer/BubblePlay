# Browser viewer

The browser viewer shows your PC's desktop. It signs in with a device key, not a password. This page covers the viewer served by the Rust relay ([`relay-rs`](../relay-rs/README.md)). The protocol is in [device keys](architecture/device-keys.md).

## Site gate

The relay serves nothing of the viewer until the browser is approved.
The relay sends the approval request to your ntfy topic as Approve and Deny links.
Meanwhile it holds the page load for up to 30 seconds and shows nothing.
Approve within that time and the page opens.
Approve later (within the approval TTL, 90 seconds by default) and the next reload opens it, once.
After a deny, a timeout or a rate limit, the browser shows a connection error. The relay sends no page and no message.
An approved browser keeps a `__Host-gate` cookie.
Forget this browser clears that cookie. It does not delete the device key or the paired PCs.

## Device key

When the page opens, it asks for a device key. You can:

- create a new key,
- import a key file, or
- unlock a protected key with its passphrase.

The key file is kept in the IndexedDB database `bubbleplay-device`, exactly as created or imported; it stays the export and backup format.
The passphrase is asked once per browser. After unlocking, the page keeps the private key in the same database as a non-extractable WebCrypto `CryptoKey`: page script can sign with it but cannot read or export it.
A refresh or a new tab uses that key directly, with no passphrase.
**Lock this browser** (PC list footer, or Settings > Browser access) deletes the kept key. The next visit asks for the passphrase again.
Importing or creating a key, or changing its passphrase, also deletes it.

The browser profile holds the kept key on disk. Anyone who can copy that profile can use the key in that browser until it is locked or revoked on the PC.

## Staying connected

A refresh on a PC page (`?pc=<id>`) goes straight back to that PC. If that PC is no longer in the list, the PC list opens.
The page pings the relay every 15 seconds. If a ping goes unanswered for 30 seconds, the page reopens its relay socket and re-authenticates without showing anything; other socket drops reconnect the same way after 250 ms.

## Online status

The relay also answers `GET /presence?ids=<id>,<id>` with which of up to 20 PCs have their host connected (see [the relay README](../relay-rs/README.md#presence-get-presence)). Native clients use it. The browser viewer does not yet; it may later use it to mark paired PCs online or offline in the PC list. The route is for same-origin use only: a request from another origin is refused, and there is no CORS.

## Paired PCs

Paired PCs are kept in the `pcs` store of the same database, per relay origin.
Each record has a name, an ID, the PC's pinned public key, a paired flag and a last-used time.
The PC list shows the paired PCs. Pick one, or open `?pc=<id>` to go straight to it.
Removing a PC here forgets it in this browser. The PC keeps its own list.

## Pairing a new PC

1. On the PC, left-click the tray icon (or choose Open BubblePlay in its menu), then Pair device.
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
4. In WebRTC mode, media and input wait until the PC's DTLS auth-bind is verified. Both sides sign and verify the sha-256 `a=fingerprint` values from the SDP each one sent and received, normalised to `sha-256 AA:BB:...` (lowercase algorithm, uppercase hex). An SDP without a sha-256 fingerprint ends the session with the ctl error `dtls-bind-required`.
5. Media and file sockets use the ticket from `hello-ok`, with the roles `media-viewer` and `file-viewer`.

## Decode paths

Over WSS the browser decodes with WebCodecs and draws in a Dedicated Worker (`wss-worker.js`), on `OffscreenCanvas`s it owns: the desktop canvas, the sharp-viewport crop and the hybrid PNG/HEVC tile overlay. The page keeps layout, input, HUD and the cursor overlay, ACKs frames on receipt (the worker reports `decodedSeq`) and sends the worker the viewport, hybrid mode and desktop state with each frame. If the browser lacks Worker, `VideoDecoder`, `OffscreenCanvas` or `transferControlToOffscreen`, or the worker fails to start or crashes, the page uses the main-thread decode path. `?decoderPath=main` forces that path for debugging, and the HUD shows `(worker)` after the codec when the worker path is active. WebRTC is unchanged: it decodes natively unless you pass `?decoderPath=webcodecs`.

**Sharp viewport (zoom).** When you zoom in, the browser sends a `media-control` `viewport` request (revision plus a Q16 crop rectangle, throttled to one per 100 ms) and the host encodes just that region of the desktop at full quality. On WSS v4 the crop travels out of band: the 28-byte frame header has no crop fields, so the host moves to a new media generation, forces a keyframe, announces `media-view` `pending` with the generation, and sends `active` once the receiver ACKs that keyframe. The viewer draws a frame on the sharp canvas only when its generation matches a known crop, and a request that does not complete in 1.5 s falls back to the full desktop (`viewport-timeout-full`). Zooming back out requests the full desktop the same way.

## Settings

Settings has two groups that matter here.

- **Device key:** export the key, change its passphrase, or remove it.
- **Browser access:** Switch PC, Lock this browser and Forget this browser.

## Tests

- `worker/test` runs the unit tests. CI job `browser` runs them. CI has no real browser, so the WSS worker path is covered by `wss-pipeline.test.mjs` (the pipeline against a fake decoder and canvases) and `wss-worker-path.test.mjs` (path selector, worker handshake and source contracts).
- `worker/test-e2e` runs the browser end-to-end tests against `bp-relay-server`. CI job `browser-e2e` runs them.

## Code layout

`worker/public/receiver.js` is being split into modules that each own their state behind a small interface (plain ES modules, no bundler). Split so far:

- `sharp-session.js`: the sharp-viewport negotiation with the host (requested, committed and pending crop, the transition timeout and the session-wide disable). The receiver supplies the desktop size, viewport geometry and transport through `createSharpSession(env)` and renders the crop the session reports. Crop math stays in `sharp-viewport.js`.
- `composition-gate.js`: commits secure-desktop Android composition text once, through one pending transaction.
- `connection-status.js`: pure model of the connection chip (state, label, tooltip, Admin warning).
- `keyboard-draft.js`: the on-screen keyboard field: diffs the draft against what was sent, chunks text under the packet limit, and sends protected input only on a secure desktop.
- `wss-audio.js`: the WSS audio player for one media session, its sound-controller sink, and cumulative counters across restarts.
- `hud-text.js`: the text of the diagnostics HUD overlay (pure formatter).
