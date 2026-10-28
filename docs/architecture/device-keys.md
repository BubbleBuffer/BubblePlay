# Device IDs and keys

Status: approved 2026-10-13 with these decisions (see [Decisions](#decisions)). Roadmap entry: [Device IDs and keys](../roadmap.md#device-ids-and-keys). [Wake-on-LAN](../roadmap.md#wake-on-lan) and [unattended access](unattended-access.md) depend on it.

Goal, in the owner's words: "a relay / ID and key based system, kinda like ssh keys but with IDs instead of IPs; we'd need a little GUI for management." Every device has a key, kept in a portable key file like an SSH key, and an ID derived from its public key. The relay routes by ID. The PC keeps its own list of trusted keys, like `authorized_keys`, and manages it in the tray. Device keys are the only way to connect; the application password and its session login are removed.

## Current state

Code reads on 2026-10-13; no live tests. This is what the plan replaces.

- **Host viewer authentication (updated 2026-10-27):** the host uses `bp-device-key::phase2` for signed handshakes, pins normal viewers against its CurrentUser DPAPI trusted list, and runs code/SAS pairing with a local Allow/Deny decision. `peer-joined.mediaEpoch` supplies the exact opaque ASCII transcript epoch; `pairing` fixes the handshake mode. An auth-hello before that relay event is rejected. Capture, input, clipboard and file access wait for authentication.
- **One room per relay.** Go keys peers as `signal:host` and `signal:client` (`relay/server.go` 403-408); Cloudflare maps every socket to the Durable Object `room-v3` (`worker/src/index.js` ~127). There is no room ID. A viewer `hello` mints the session epoch that pairs media and file sockets (`server.go` 463-466).
- **Host registration (updated 2026-10-27):** the PC key signs the relay challenge and registration requires a PC ID and data-socket ticket. Host, media-host and file-host use the phase 2 route without a bearer. The host publishes its admit list after hello-ok and after trusted-list changes. Legacy setup/tray credential fields remain for the later cleanup deliverable.
- **Viewer login:** one shared application password per relay, no accounts. Go stores an Argon2id hash (`relay/auth.go` 57, 79) and offers TOTP, ntfy or host approval; Cloudflare compares a SHA-256 digest of `APP_PASSWORD` (`worker/src/gate.js` 380-382) and offers ntfy or host approval. The result is two `__Host-` cookies (12 h, or 30 days remembered; `auth.go` 38-39, 363-368), rechecked on every viewer socket ([browser approval](browser-approval.md)).
- **Android** signs in through a WebView and replays its cookies on `/ws`, `/media-ws` and file sockets (`android/.../ui/LoginActivity.kt` 21-27, `net/Session.kt` 306-316). It stores one server URL in plain `SharedPreferences` (`ui/Prefs.kt` 48-54) and has no key material and no list of PCs.
- **Host transport binding (updated 2026-10-27):** every endpoint message on `/ws` uses ctl AEAD, including exact binary input/cursor bytes; plaintext endpoint frames are dropped. WebRTC requires `dtls-bind-v1`, signs the exact SDP fingerprints, and gates media and data-channel input until both binds verify. WSS media and file payload protocols remain unchanged; their end-to-end encryption is phase 4.
- **Host permission gates (updated 2026-10-27):** authentication and, for WebRTC, both DTLS binds precede the existing Admin mode, secure-desktop and first-frame input gates. Files use the authenticated viewer epoch. The Devices API uses mpsc commands/events; the tray sends `open-devices` through stdin. A redacted log UI is wired until the separately built Devices window is connected. Relay interoperability and on-screen UI operation have not been checked live.

What changes: the endpoints authenticate each other with keys, and the relay only routes and keeps strangers out. Password login, TOTP and host approval, the session cookie and the Android WebView login are deleted. ntfy approval stays at the relay as a [site gate](#site-gate) in front of the browser page, unrelated to device keys. On the PC, ntfy becomes the second factor for pre-login sessions and a heads-up for waiting pairing requests; remote pairing is approved at the PC or by a voucher from a paired device.

## Identity

### Key type: Ed25519

With keys in portable files, hardware binding no longer decides the curve. Ed25519 is chosen over P-256:

- Available in all three runtimes: WebCrypto `Ed25519` in desktop Chromium (since 137, 2025), Firefox and Safari; `ed25519-dalek` in Rust; on Android an in-process library (BouncyCastle or Tink) on every supported API level, since the Keystore is not the identity store any more.
- 32-byte keys keep the file and the ID derivation simple, and match the SSH `ssh-ed25519` model the owner has in mind.
- Signatures are deterministic, so no implementation can leak the key through a bad random nonce. P-256 ECDSA needs a good nonce on every signature.

Ephemeral key exchange in the handshake uses X25519 (WebCrypto, `x25519-dalek`, the same Android library). Phase 0 checks X25519 in the minimum supported Chromium; if it is missing there, the ephemeral exchange uses P-256 ECDH, which every runtime has. Ephemeral keys are not identities, so that fallback changes nothing else.

### Device ID

The first 80 bits of `SHA-256(raw 32-byte Ed25519 public key)`, written as 16 Crockford base32 characters in four groups: `B7QK-2M9X-H4PD-R8TN`. Case-insensitive, no 0/O or 1/I/L confusion. 80 bits makes finding a second key with the same ID infeasible (about 2^80 work), and the full key is pinned at pairing anyway. The ID is both the routing address and the fingerprint people compare.

### Key file format

One text file per key, extension `.bpkey`, UTF-8 JSON:

```json
{
  "format": "bubbleplay-device-key",
  "version": 1,
  "type": "ed25519",
  "id": "B7QK-2M9X-H4PD-R8TN",
  "label": "Pixel 8",
  "created": "2026-10-13T09:30:00Z",
  "public": "<base64url, 32 bytes>",
  "kdf": { "alg": "argon2id", "m": 65536, "t": 3, "p": 1, "salt": "<base64url, 16 bytes>" },
  "aead": { "alg": "aes-256-gcm", "nonce": "<base64url, 12 bytes>" },
  "private": "<base64url: AES-GCM ciphertext and tag of the 32-byte seed>"
}
```

- **Without a passphrase**, `kdf` and `aead` are `null` and `private` is the plain 32-byte seed.
- **KDF: Argon2id**, 64 MiB, 3 passes, 1 lane, the same parameters the Go relay uses for passwords today (`relay/auth.go` 57). Rust has the `argon2` crate and Android BouncyCastle's Argon2 generator. WebCrypto has no Argon2, so the browser ships a small vendored Argon2id WASM build with the other assets, listed in the third-party notices. PBKDF2 alone would make a stolen file much cheaper to guess on GPUs, which is why the owner's preference holds here despite the extra dependency.
- **AEAD: AES-256-GCM**, native in WebCrypto, Rust (`aes-gcm`) and Android (JCA). A file is encrypted once per passphrase change with a fresh random nonce. The associated data is the canonical JSON of every field except `private` and `aead.nonce`, so a tampered ID, public key or KDF parameters fail to decrypt.
- **On import**, the client derives the public key from the seed and refuses the file if it does not match `public` and `id`. Unknown `version` or `type` is refused with a clear message.
- `label` is a suggestion for the device name at pairing; the PC's own name for the device wins.

### Where keys live

| Device | At rest | Unlock | Export / import |
| --- | --- | --- | --- |
| Browser | The `.bpkey` file as created or imported, in IndexedDB for the relay origin. | Asks for the passphrase when the page opens, if the key has one. | **Export key** downloads the file; **Import key** reads one. |
| Android | `files/device-key.bpkey` in app-private storage, additionally wrapped with a Keystore AES key so a copied file is useless off the phone. Backup stays off. | Asks for the passphrase when the app opens, if the key has one. | Export and import through the system file picker; export unwraps the Keystore layer and writes the portable file. |
| PC user host | `%LOCALAPPDATA%\BubblePlay\device-key.bpkey`, CurrentUser DPAPI-wrapped, no passphrase, because the host starts unattended at sign-in. | None. | Tray menu **Export PC key...** asks for an optional export passphrase; **Import PC key...** keeps a PC's ID across a reinstall. |
| Pre-login logon host | `ProgramData\BubblePlay\prelogin\device-key.bpkey`, LocalMachine DPAPI, ACL SYSTEM and `NT SERVICE\BubblePlayLogon`. | None. | Not exported; it is created by the elevated pre-login opt-in. Its own ID, as decided in [unattended access](unattended-access.md#decisions). |
| Wake-on-LAN helper | Key file in its state volume, mode 600, no passphrase. | None. | Plain file. |

**First open of a viewer:** with no key yet, the browser and the Android app offer **Create key** and **Import key**. Create asks for an optional passphrase (recommended, with a short note that without one anyone who copies the file is this device) and offers export right away.

**Unlocked-key lifetime:** memory only, never written back in unlocked form.

- Browser: the decrypted seed is imported into WebCrypto as a non-extractable `CryptoKey` and the byte buffers are overwritten. The key lives until the tab closes or reloads; a reload asks again. Nothing goes to `sessionStorage` or `localStorage`.
- Android: the seed is held in process memory until the app is closed (task removed, or the user backs out of the PC list) or the process dies. Backgrounding during a session keeps it, so the planned background survival works.
- Unlocking is local; a wrong passphrase never reaches the relay or the PC.

### One key on several devices

A key file can be imported on several devices. To the PC they are one identity: one row in the tray, one set of permissions, one revocation. Revoking a shared key cuts off every device that holds it. The tray therefore shows connections per key: client kind (browser or Android), IP prefix and time for each recent connection, so the owner can see that a key is in use from two places. Separate keys per device remain the recommended setup and the default, since every new install offers **Create key** first.

### Rotation, revocation, loss

- **Rotation:** a viewer device rotates its ID by vouching for its own new key ([ID rotation](#id-rotation), phase 3). A PC rotating its own key signs a `rotate` statement with the old key naming the new one and sends it to its viewers; a viewer that has not seen the statement shows "PC key changed" and refuses, as SSH does. Manual only; no expiry.
- **Revocation** is local and immediate: removing a key from the PC's list ends its open session and refuses it from then on. The relay's admission list updates on the next sync.
- **Lost or stolen device or key file:** revoke the key in the tray. A passphrase-protected file costs the thief an Argon2id guess per try; an unprotected one is usable at once, until revoked.
- **Lost browser key** (site data cleared, new profile) without an exported copy: the browser is a new device and pairs again. The old row shows "not seen for N days" until revoked.
- **Passkeys** stay a possible later option and are not planned.

## Pairing

Pairing adds a viewer's public key to a PC's trusted list, with a name and permissions, and gives the viewer the PC's ID and key.

### A. At the PC

1. Owner clicks **Pair device** in the tray. For 5 minutes the tray shows the PC's ID, a one-time 10-character code, and a QR code with `bubbleplay://pair?relay=<url>&pc=<id>&code=<code>` (the browser uses `https://<relay>/pair#...`; the fragment never reaches the relay).
2. The viewer, key unlocked, scans the QR or types PC ID and code, and connects to the relay as an unpaired device asking for that PC.
3. Viewer and PC exchange public keys and a pairing proof keyed by the code (HMAC over the transcript), so the PC accepts only requests from someone who saw its screen.
4. Both sides show a 6-digit comparison number derived from the transcript. The tray shows "Pair 'Pixel 8' (ID B7QK-...)? Number on the device: 482 913" with the default permissions and **Allow** / **Deny**. The relay cannot fake the number without changing the keys. The code alone would not be enough: a relay could brute-force a short code under HMAC within its lifetime.
5. The PC adds the entry; the viewer stores the PC (ID, public key, relay URL, name).

This is also the only way to pair the first device on a fresh install, so first setup needs no password.

### B. Remotely, approved at the PC or by a paired device

For pairing while away from the PC. Off by default (**Allow remote pairing** in the tray). ntfy does not approve pairing.

1. The new device asks the relay to pair with a PC ID, without a code. The relay forwards the request only if the PC has remote pairing on (it publishes that flag), and within rate limits: one pending request per PC, a few per source prefix per hour, enforced at the relay and again at the PC.
2. New device and PC exchange keys; both compute the comparison number. The new device shows it.
3. The PC offers the request to two kinds of approver:
   - **the PC itself:** a tray notification "Pairing request from 'Work laptop' (ID M3ZD-...), number 482 913", which opens the confirmation in the Devices window;
   - **paired devices allowed to vouch:** the device in a session gets an in-app prompt over its authenticated control channel; other paired devices see the request under the PC in their app's PC list the next time they open it.

   If ntfy is configured on the PC, it sends a heads-up only: "A pairing request is waiting. Open BubblePlay on a paired device or at the PC." No number, no ID, no approve link.
4. The owner compares the number with the one on the new device, chooses permissions and approves, either at the PC or on the paired device. On a paired device, approving signs a [voucher](#vouchers).
5. The PC adds the entry, marked "paired remotely" and, for a voucher, "vouched by <device>", and writes an event log entry. A request nobody approves expires after 10 minutes.

Why this holds against a relay in the middle: the approver gets the comparison number from the PC, either on the PC's own screen or over its authenticated control channel, so the relay cannot change it. If the relay substitutes its own key, the approver sees a number that differs from the one on the new device. The relay cannot approve either: only the PC or a signature from a trusted key can.

Pairing traffic (requests, prompts, vouchers) does not take the room's viewer slot; the relay routes these small messages between the PC and admitted devices alongside a running session.

### Vouchers

A voucher is a statement signed by an already-paired device that admits a new key to one PC.

```json
{
  "type": "bubbleplay-voucher",
  "version": 1,
  "voucher": "B7QK-2M9X-H4PD-R8TN",
  "subject": { "id": "M3ZD-71QF-XK5A-0H2C", "public": "<base64url>" },
  "pc": "7HQ2-K9MX-4RTD-P0ZB",
  "permissions": ["view", "input"],
  "issued": "2026-10-13T10:02:00Z",
  "expires": "2026-10-13T10:12:00Z",
  "nonce": "<the PC's nonce for this pairing request>",
  "request": "<hash of the pairing transcript>",
  "signature": "<Ed25519 over the canonical JSON without this field>"
}
```

- The paired device shows the new ID, its fingerprint and the comparison number before the owner confirms; permissions are pre-filled with view and input.
- The PC accepts a voucher only if the signature verifies, the voucher's key is in its trusted list now (not revoked) and allowed to vouch, the PC ID, nonce and request hash match the pending request, it has not expired, and it was not used before.
- **Ceiling:** a voucher grants at most the voucher's own permissions. "Before sign-in" is never granted by voucher, and so nothing that leads to unattended access; that stays a UAC step at the PC.
- **Who may vouch:** a per-device switch in the tray. Default on for devices paired at the PC, off for devices paired remotely (open question 1).
- **Audit:** the PC records who vouched for whom and when; the tray shows the chain, and the event log has each voucher.
- **Revocation:** revoking a device that vouched for others asks "Also revoke the 2 devices it vouched for?" with **Revoke all** and **Only this one**. It never cascades silently. Devices kept after their voucher was revoked stay paired and show "voucher revoked".

### ID rotation

A device can vouch for its own new key. It creates a new key file and sends the PC a voucher with `"rotate": true`, signed by the old key, plus a signature by the new key over the same voucher to prove it holds the new key. The PC moves the old ID's name, permissions, vouching relations and history to the new ID and revokes the old ID in the same write of the trusted list, then updates the relay's admission list. "Before sign-in" does not move: the PC drops it and the owner grants it again at the PC through UAC. A device paired with several PCs rotates with each one as it next connects, and keeps the old key until every PC on its list has confirmed.

What it is worth:

- **Security, real:** rotate after a suspected exposure, for example after changing the passphrase while an old copy of the file may still exist somewhere, or as routine hygiene.
- **Privacy, small:** it limits how long the relay can link one ID across time. The PC and the relay still see the same device in the same room from the same IP prefix, so the link is easy to rebuild.
- **Not a fix for a leaked key:** whoever holds the old key can rotate too, and may do so first. After a leak, revoke at the PC, which removes the old ID and anything rotated from it.

The tray and app UI for rotation is not detailed here.

### Both relay types

Pairing runs end to end over the signaling socket, so Go and Cloudflare carry it the same way. Each relay needs: routing by PC ID, admission of unpaired devices only for an open pairing window (A) or a PC with remote pairing on (B), routing of pairing messages beside a running session, and the new message types. Neither relay holds a password or TOTP seed any more. The relay's own ntfy settings serve only the site gate. The PC has separate ntfy settings (`host.json`, DPAPI-wrapped) for the pairing heads-up and the pre-login second factor; they may point at the same server and topic.

## Site gate

Kept from today, independent of device keys: a browser must pass ntfy approval before the relay serves anything of the site. The owner does not want anyone browsing or probing what the relay hosts.

- **What is behind it:** every browser-facing path: the page, its scripts and styles, the Argon2id WASM, the key create/import and unlock screens, `/pair`, and the browser upgrades of `/ws`, `/media-ws` and `/file-ws`. A browser socket needs the gate cookie **and** a valid key signature. Phase 2 checks that no asset path is reachable without the cookie.
- **How it works, both relays:** today's ntfy flow from [browser approval](browser-approval.md), reduced to one method. Opening any path without a valid gate cookie holds the navigation and publishes Approve/Deny to the relay's ntfy topic, with the client prefix and a short browser summary; the decision links are single-use capabilities posted to `/__gate/decision`; at most ten pending requests and one per client prefix; requests expire after the approval TTL (Go `approvalTTLSeconds`, 30-120 s, default 90) or when the browser leaves. Go keeps `ntfyURL`, `ntfyTopic`, `ntfyToken`; Cloudflare keeps `NTFY_URL`, `NTFY_TOPIC`, `NTFY_TOKEN`. TOTP and host approval are removed, so `approvalMethods` / `APPROVAL_METHODS` go away and ntfy becomes required for any relay that serves the browser.
- **How long an approval lasts:** the `__Host-gate` cookie (Secure, HttpOnly, SameSite=Lax), valid for 30 days from approval for that browser profile, not extended by use. The operator revokes gate sessions with the admin token as today; the page offers **Forget this browser**, which clears the cookie. A revoked or expired cookie closes the browser's sockets on their next check and starts a new approval on the next navigation.
- **What an unapproved visitor sees:** a minimal wait page: no title, favicon, product name, version or script, only "Waiting for approval" and, after a deny or expiry, "Not approved". Every unknown path returns the same page; the default route, error pages and response headers carry no product name. `/__auth/methods` is removed. `/healthz` answers only on loopback or to the operator's admin token, since today it names the backend. The AGPL source link moves behind the gate (open question 3).
- **Android:** recommended exempt. The app does not load the page; its sockets send no `Origin`, and the relay admits them only after the device-key signature check. Before that check a native socket gets a nonce and nothing else, and an invalid or unlisted signature closes it without a reason text, so a native-protocol probe learns no more than the wait page shows. Requiring the gate in the app would bring back a WebView or a cookie flow just to repeat what the key already proves. A pairing request from an unpaired app still needs an open pairing window or remote pairing on the PC.
- **Host sockets** are not gated by the cookie either; they register with `TOKEN` and the PC key.

## Routing and the relay

- **Hosts register by ID.** On `/host-ws` the host sends its ID and public key and signs a relay nonce. The relay checks the ID against the key hash and the signature, and issues a short-lived ticket for that connection's media and file sockets.
- **Who may register:** the operator's registration secret (`TOKEN`) stays as the ticket for a PC's first registration; the relay then remembers the PC ID and key, and later registrations need only the signature. The operator lists and removes PC IDs with the admin token. `HOST_GATE_TOKEN` is replaced by the signature and the ticket, and deleted.
- **Viewers connect to a PC ID.** The PC ID is in the URL (`/ws?pc=<id>`). `hello` carries the viewer's ID and public key. The relay answers with a nonce, and the viewer signs it, the same way hosts register. The relay then answers with a ticket for that epoch's media and file sockets. The session cookie is gone; browsers also need the site gate cookie. For the wire format, see [Phase 2 relay protocol](#phase-2-relay-protocol).
- **Admission without accounts:** each PC publishes the list of viewer IDs it trusts (IDs only) and whether remote pairing is on. The relay admits a viewer to a PC's room only if its ID is listed, or for an open pairing window or a remote pairing request. Strangers never reach the host's parser beyond the pairing messages, and the PC still checks every key itself.
- **What the relay stores:** per PC: ID, public key, last seen, the published viewer-ID list, the remote-pairing flag, a hash of an open pairing code. Separately, the site gate's ntfy settings and hashed gate sessions, as today. No names, permissions, passwords, or second-factor secrets for pairing or pre-login. The Rust relay keeps it in `pcs.json` and `gate.json` on native, and in the storage of a single Durable Object (`relay-v1`) on Cloudflare. That one object runs the whole relay core, gate included, and keeps per-socket state in hibernation attachments. Rooms are per PC ID inside that relay. Splitting rooms into a Durable Object per PC is the path if one object's CPU budget becomes the limit; it is not built.
- **What the relay learns:** which device IDs talk to which PC and when, and their IP prefixes, as it sees today.
- **Accounts:** none. Multi-user accounts stay out of scope ([release status](../release-status.md)).
- **One room still pairs one host with one viewer**; a second viewer for the same PC gets "PC busy". Multi-viewer is separate work.
- **Multi-PC on the viewer:** the browser and Android keep a list of paired PCs (name, ID, key, relay URL, online state from the relay). The browser's list lives in IndexedDB beside the key file, per relay origin; Android gains the list it lacks today.

## Handshake

After the relay joins viewer and PC, and before the host starts capture or accepts input, they authenticate each other over the signaling socket:

1. Viewer → PC `auth-hello`: viewer ID, public key, 32-byte nonce, ephemeral X25519 key, capabilities.
2. PC → viewer `auth-host`: PC ID, public key, nonce, ephemeral key, signature over the transcript so far.
3. Viewer → PC `auth-finish`: signature over the full transcript.

The transcript is `"bubbleplay-auth-v1"`, the relay origin, the session epoch, both IDs and public keys, both nonces and both ephemeral keys. The viewer checks the PC key against its pinned key; the PC checks the viewer key against its trusted list and loads that key's permissions. A mismatch closes the session with a reason the user sees ("PC key changed", "This device is not paired").

**Binding, so a relay cannot splice:**

- **WebRTC:** once DTLS is up, each side signs the local and remote certificate fingerprints from the local and remote SDP (`a=fingerprint`). DTLS already checks the remote certificate against the remote SDP, so a relay that rewrote SDP to sit in the middle produces fingerprints the peer did not generate, and the signatures fail. Media and input wait for this step.
- **Control channel, both modes:** TLS ends at the relay, and input and clipboard travel on `/ws` in both transport modes. HKDF over the ephemeral shared secret gives per-direction keys no relay knows. Every control and input message is authenticated with AES-GCM under them (sequence number as nonce, replays refused). Without this, the handshake proves who connected but a relay could still inject input afterwards, so it ships in the same phase as the handshake.

**End-to-end encryption for WSS media** (future work in [release status](../release-status.md)) becomes possible with the same keys: AES-GCM per WSS v4 access unit, the 28-byte header as associated data, generation and sequence as the nonce. In scope here: the key schedule and a capability flag. The framing change, per-frame WebCrypto cost in the browser decode path, and file-socket encryption are a separate proposal.

**Browser caveat:** the relay serves the browser's JavaScript. A relay that turns hostile can ship script that captures the passphrase or uses the unlocked key while the page is open. Device keys protect a browser against everyone else, and against a relay that only forwards; not against its operator. The Android app and the PC do not have this problem, because their code does not come from the relay. For the self-hosted setup the operator is the owner.

## Permissions

Each trusted key has permissions, set at pairing and editable in the tray. The host enforces them after the handshake. Defaults are for pairing at the PC; remote pairing pre-fills view and input, and a voucher can never exceed the voucher's own permissions or grant "Before sign-in". A separate per-device switch, **May vouch**, is on by default for devices paired at the PC and off for devices paired remotely.

| Permission | Default | Existing gate it maps to |
| --- | --- | --- |
| View | on, always | The session itself. |
| Input (pointer, keyboard, text) | on | New check at the top of the input route (`host/src/input.rs` 3533). Off gives the view-only mode that is missing today. |
| Clipboard | on | New check in the clipboard control path. |
| Files | on | Replaces "host gate token present" (`host/src/main.rs` 950-963). |
| Elevated input | off | Admin mode stays the machine switch in HKLM, set through UAC. Elevated input needs Admin mode **and** this permission. The host enforces the device part, so the residual same-user risk in [security and privacy](../security-and-privacy.md) is unchanged. |
| Before sign-in | off | The logon host admits only keys with this permission, plus a fresh ntfy approval per session, sent by the logon host itself. |
| Wake | on | The Wake-on-LAN helper accepts wake requests only from keys the PC marked so. |

**Lists for the logon host and the helper.** The owner edits only in the user host's tray. Giving a key "Before sign-in" writes its ID and public key to `ProgramData\BubblePlay\prelogin\trusted.json` through the same UAC step that turns on pre-login (ACL: SYSTEM, the service SID, Administrators), so a same-user process cannot add pre-login devices. The Wake-on-LAN helper is paired with the PC like a viewer (pairing at the PC); while awake, the PC sends it, end to end, its MAC address and the IDs and keys of devices with "Wake". The helper checks a viewer's signed wake request against that list.

## Tray management window

Opened from the tray menu (**Devices…**), next to Admin mode, Start at login and Logs.

```
BubblePlay — Devices                               [Pair device] [Export PC key]
This PC   Office-PC   ID 7HQ2-K9MX-4RTD-P0ZB   Relay play.example.com (online)

 Name           ID                    Last seen          Permissions
 Pixel 8        B7QK-2M9X-H4PD-R8TN   now (connected)    View Input Clip Files   [Edit] [Revoke]
 Laptop Chrome  M3ZD-71QF-XK5A-0H2C   yesterday 22:14    View Input Clip Files   [Edit] [Revoke]
   └ 2 places:  browser 203.0.113.0/24 yesterday 22:14 · Android 198.51.100.0/24 3 days ago
 Work Edge      9PWE-RT4K-2NV8-JJ6Q   34 days ago        View Input  (remote)    [Edit] [Revoke]
   └ vouched by Pixel 8, 2026-09-09
 WoL helper     Q4XD-8B2M-ZT7C-1KHR   now                Wake                    [Edit] [Revoke]

 [ ] Allow remote pairing (approve here or on a paired device)    ntfy heads-up: ntfy.example.com / bp-pc  [Change]
```

- Each row is one key. A key seen from more than one client kind or network in the last 30 days expands to its recent connections, so a shared key file is visible.
- **Edit** opens name and permission checkboxes; "Elevated input" and "Before sign-in" are greyed with a note while their machine switch is off, and changing "Before sign-in" asks for UAC.
- **Revoke** asks once, removes the key, ends its session and syncs the relay list. If the device vouched for others, the same dialog asks whether to revoke them too.
- **Pair device** shows the code, the QR and the confirmation from pairing at the PC, inside this window.
- **Allow remote pairing** works without ntfy; ntfy only adds the heads-up. Pending requests appear at the top of the window with the comparison number and **Allow** / **Deny**.
- **Edit** includes **May vouch**; a vouched device shows its voucher under its row.
- Last seen comes from the host's own records, not the relay.
- Native window, like the existing approval dialog, following the BubblePlay product profile.

**Current state of PC key backup (2026-10-15):** the tray menu has **Export PC key...** and **Import PC key...**, built in `host/src/tray/key_ui.rs`. Export asks for an optional passphrase, typed twice, then saves `<PC ID>.bpkey`. Import reads a `.bpkey`, asks for its passphrase when it is protected, asks to confirm the ID change, replaces the PC key and restarts the host. Both run on their own thread, so the tray stays responsive. Not yet checked on screen.

## Cutover and removal

No migration period: device keys are the only way to connect. Relay, host and clients are updated together; an old client or host stops working against a new relay. There is no public release yet, so only the owner's own setups are affected.

Deleted in phase 2:

- **Go relay:** application password (`passwordHash`, Argon2id login in `auth.go`), TOTP login and replay state, the `__Host-bp-session` cookie and its revocation routes, `/__auth/login`, `/__auth/methods`, the `host` notifier and `approvalMethods`, `hostGateToken`; `init` and `rotate` lose `--password`, `--totp` and `--methods`.
- **Cloudflare relay:** `APP_PASSWORD`, `HOST_GATE_TOKEN`, `APPROVAL_METHODS`, the password login and session cookie in `gate.js`, the host notifier.
- **Browser:** `login.html` and the password form; the approval wait page is reduced to the minimal site-gate page. The key unlock or create/import screen and the PC list replace the login.
- **Host:** the browser approval dialog (`host/src/browser_approval.rs`) and `browserApproval` capability, `HOST_GATE_TOKEN` handling in setup, tray and signaling.
- **Android:** `LoginActivity` WebView login, cookie storage and replay, "Forget browser".
- **Docs and fixtures:** [browser approval](browser-approval.md) rewritten as the site gate (ntfy only); the host-approval messages in `fixtures/browser-approval-contract.json` removed; setup guides rewritten for key pairing.
- **Ticketless media and file sockets:** require a ticket on `/media-ws` and `/file-ws` for ID'd PCs, in both relays. In phase 1 a pc-less socket without a ticket is still accepted when its epoch matches the resolved room, because old viewers send no ticket and the upgrade stays behind today's login.

Kept: the ntfy site gate with the `__Host-gate` cookie and `/__gate/decision`, `TOKEN` as the first-registration ticket, the admin token for operator management, the per-prefix rate limiting (now also on pairing requests and handshakes), TURN credential issuance.

## Threats and mitigations

| Threat | Mitigation | Remaining |
| --- | --- | --- |
| Unauthenticated probing of the site and relay | Site gate: nothing of the page is served before ntfy approval; the wait page, errors and headers name no product; `/__auth/methods` removed, `/healthz` restricted; native sockets get only a nonce before a valid key signature. | The wait page itself and the TLS certificate show that something is hosted; a visitor can trigger notifications within the rate limits. |
| Stolen key file | Passphrase with Argon2id; on Android also the Keystore wrap; revoke in the tray. | An unprotected file works until revoked. |
| Key file shared across devices | Shown per connection in the tray. | One revocation covers all of them. |
| Malicious or compromised relay splicing a session | Handshake pinned to stored keys; DTLS fingerprints signed; control messages authenticated. | WSS media readable until the E2E phase. Browser clients run relay-served script. |
| Relay brute-forcing a pairing code | Comparison number confirmed on the PC; code single-use, 5 minutes. | None known. |
| Relay faking a remote pairing | Approval only at the PC or by a voucher; the approver gets the comparison number from the PC over a channel the relay cannot change; vouchers bound to the request nonce and transcript, single-use, short expiry. | None known. |
| Stolen paired device vouching for new keys | Permission ceiling; never "Before sign-in"; vouching off by default for remote-paired devices; revoking asks to cascade to vouched devices; every voucher in the tray chain and the event log. | Until revoked, a stolen device allowed to vouch can add devices within its own permissions. |
| Leaked key rotated by the attacker | Rotation needs the old key to be trusted; revoking at the PC removes the rotated ID too. | Rotation does not protect against a leaked key; only revocation does. |
| Strangers spamming remote pairing | Off by default; one pending request per PC; rate limits per PC and per source at the relay and the PC; requests shown as tray or in-app prompts, ntfy heads-up rate-limited. | Requests now also reach paired devices. Anyone who knows relay URL and PC ID can trigger prompts within those limits. |
| ID squatting at the relay | ID must match the key hash and a signature; first registration needs `TOKEN`. | Denial of service by a holder of `TOKEN`. |
| Other same-user process on the PC adding a device | Trusted list DPAPI-wrapped; pre-login list needs UAC. | Same residual risk as Admin mode today. |
| PC key copied by malware running as the user | DPAPI at rest. | A same-user process can read it; same boundary as today's host credentials. |
| Replay of a handshake or input | Fresh nonces and epoch in the transcript; per-message sequence numbers. | None known. |

Stays prohibited: accepting a key without a confirmation at the PC or a valid voucher from a trusted device allowed to vouch; approving a pairing through ntfy; granting "Before sign-in" by voucher or rotation; pairing before sign-in; any password or shared-secret login for viewers; treating the site gate as access to a PC; serving any page asset before the site gate; the relay storing names, permissions or the PC's second-factor secrets; writing an unlocked key to disk; any inbound listener on the PC; and everything already prohibited in [unattended access](unattended-access.md#recommendation).

## Phased plan

| Phase | Content | Size | Unblocks |
| --- | --- | --- | --- |
| 0 | `fixtures/device-keys-contract.json`: ID derivation, key file vectors (with and without passphrase), message shapes, transcript and signature vectors, for Rust, Go, JS and Kotlin. Check X25519 and Ed25519 in the minimum Chromium; pick and vendor the Argon2id WASM. | S | everything |
| 1 | Key files on host, browser and Android: create, import, export, passphrase, unlock screen. Hosts register by ID; both relays route rooms by PC ID; tickets for media and file sockets; operator PC-ID list. | M | multiple PCs; logon host and helper as separate IDs |
| 2 | Pairing at the PC, handshake with DTLS binding and the authenticated control channel, trusted list, tray Devices window, viewer PC lists, relay admission by published IDs. **Cutover:** delete the password, TOTP, host approval, session cookie and `HOST_GATE_TOKEN` paths in relay, worker, browser, host and Android (see above); reduce the site gate to ntfy only, with the minimal wait page, and confirm every asset sits behind it. | L | **Wake-on-LAN** (helper pairs, verifies signed wake requests; all paired keys may wake until phase 3) |
| 3 | Per-device permissions in the host gates; ntfy on the PC (heads-up, pre-login second factor); pre-login trusted list and logon host key in ProgramData; remote pairing with approval at the PC or by voucher, the pairing side channel on both relays, revocation cascade and the vouching chain in the tray; ID rotation. | L | **Unattended access** step 2, which needs only the permissions, the pre-login list and the PC-side second factor; those land first within the phase, before vouchers and rotation |
| 4 | Separate proposal: WSS media and file E2E with the phase 2 keys. | M-L | release-status future work |

Sizes: S under a week, M one to three weeks, L three to six weeks, each with tests against the shared fixture. Phase 2 is one release for all components, since the old login disappears with it.

## Open questions for the owner

1. May a remotely paired device ever be allowed to vouch? Proposed: only if the owner turns on **May vouch** for it at the PC; never by default.
2. How long does a voucher stay valid before use? Proposed: 10 minutes, the same as an unanswered pairing request.
3. AGPL-3.0 asks the relay to offer its source to users who interact with it over a network. The minimal wait page shows no source link, so that it names nothing; the link is behind the gate. Is that acceptable, or should the wait page carry a plain unbranded "Source" link?

## Decisions

2026-10-13, owner:

1. **Portable key files, SSH-style.** Keys can be exported and imported; each key has an optional passphrase. Browser and Android ask for it on open, and offer **Create key** and **Import key** when there is none. No non-extractable WebCrypto key and no Keystore-bound identity; the Keystore may wrap the file at rest. This supersedes the earlier recommendation of a non-extractable browser key with optional passkeys. Passkeys remain a possible later option, not planned. Consequences above: Ed25519 instead of P-256, the `.bpkey` format with Argon2id and AES-256-GCM, unlocked keys in memory only, and one tray row per key with connections shown per place.
2. **Remote pairing:** allowed, off by default, view and input only, flagged in the tray. The ntfy approval in the first version of this decision is superseded by decision 6.
3. **Android fingerprint or PIN per connection:** replaced by the passphrase-on-open rule.
4. **No legacy path:** the application password, its session login, TOTP login and host approval are deleted at once, with no compatibility mode, no "Require device keys" switch and no migration period. Device keys are the only way to connect. Kept: `TOKEN` for a PC's first registration. (ntfy as the authentication for remote pairing is superseded by decision 6.)
5. **Site gate stays:** ntfy approval in front of the browser page, before the relay serves anything of the site, independent of device keys and not part of the removal. Proposed details above: 30-day gate cookie per browser, a minimal unbranded wait page, Android exempt.
6. **Remote pairing approval, vouchers and ID rotation** (owner's direction, refined by us). Owner: "Remote pair requires approval on a connected device or the pc itself (either just a on device notification you can confirm or like an actual flow where one paired id can 'vouch' for a new id? This could also allow us to later allow easy switching of device ids (idk if that'd at all be a worthy privacy thing)".
   1. Approving a remote pairing needs a confirmation at the PC (tray notification with the comparison number) or a voucher from an already-paired device. ntfy no longer approves pairing; at most it sends a heads-up that a request is waiting. This replaces the earlier question about authenticator codes for remote pairing.
   2. A voucher: the paired device receives the request over its own authenticated session, shows the new ID's fingerprint and the comparison number, and after the owner confirms signs the new public key, its own ID, the PC ID, the granted permissions, an expiry and a nonce. The PC verifies the signature and that the voucher's key is still trusted, records who vouched for whom, and the tray shows the chain.
   3. A voucher grants at most the voucher's own permissions; "Before sign-in" and unattended rights are never granted by voucher. Vouching is a per-device switch, default on only for devices paired at the PC.
   4. Revoking a device asks whether to revoke the devices it vouched for; no silent cascade.
   5. ID rotation: a device vouches for its own new key; the PC moves permissions and history to the new ID and revokes the old one atomically. Real security value after a suspected exposure, small privacy value, no help against a leaked key (the attacker can rotate too; revoke at the PC instead). Phase 3, with remote pairing; tray and app UI not detailed here.
   6. Pairing-request spam now reaches paired devices too, so requests show as tray or in-app prompts with rate limits per PC and per source. Remote pairing stays off by default.

## Phase 0 results

2026-10-15: [the fixture](../../fixtures/device-keys-contract.json) contains three Ed25519 ID derivations (including lowercase/O/I/L/unhyphenated parsing), plain and protected `.bpkey` files, wrong-passphrase and tampered-AAD failures, a full handshake, control ciphertexts for sequences 0/1 in each direction, both DTLS bindings and four signaling examples. Private inputs are public, fixed and **test-only**. Fields ending in `Hex` are hexadecimal; other binary fields use unpadded base64url. `aadUtf8` and `plaintextUtf8` fix exact UTF-8 bytes. The decimal epoch vector exceeds JavaScript's safe integer range.

[Generate](../../tools/device-keys/gen-contract.mjs) on linux01 with Node 24: `crypto.argon2Sync` is required; output is UTF-8 JSON, two-space indentation, final LF. Independent [Node](../../tools/tests/device-keys-contract.test.mjs) and [Go](../../relay/devicekeys_contract_test.go) checks recompute from inputs. Node 22 skips only three Argon2 cases; Go always checks them with the existing `golang.org/x/crypto/argon2` dependency. Go runs only on linux01: `cd relay && go test ./... -run DeviceKeys -v`.

Verified: repeated generation is byte-identical and matches the staged fixture; Node 24 passes 10 checks, Node 22 passes 7 with the required 3 skips. On 2026-10-15, `cd relay && go test ./... -run DeviceKeys -v` passes all four checks on linux01 with Go 1.27.1, including relay proofs; `go vet ./...` also passes. No Go ran on the PC.

2026-10-25: the fixture gained a `phase2` section (phase 0 content unchanged): pairing code and its normalisation, the relay admit hash (`"bubbleplay-pair-admit-v1"` || canonical PC ID || normalised code, no length prefixes), pair key (HKDF-SHA256, code ASCII as IKM, empty salt, info `bubbleplay-pair-v1`), both `pair-proof` MACs plus a wrong-code MAC, the comparison number, the QR and link forms, `pair-open`/`pair-close`/`pair-closed`/`pair-done`/`pair-denied`, an `admit` list, mandatory caps, and `ctl` envelopes for sequences 0/1 in each direction (the viewer's sequence 0 is a binary input frame, the rest JSON) with replay, skip, wrong-kind and wrong-direction failures. `auth-bind` keeps the phase 0 `handshake.dtls` encoding below. Where phase 0 defines an encoding, it wins.

### Encodings for implementers

- **IDs/key files:** SHA-256(raw 32-byte Ed25519 public), first 10 bytes, most-significant-bit-first Crockford base32, uppercase `XXXX-XXXX-XXXX-XXXX`. Ignore input hyphens/case; O→0, I/L→1. Seed: 32 bytes. Plain: `kdf`/`aead` null, `private` = seed. Protected: Argon2id v19, UTF-8 passphrase, m=65536 KiB, t=3, p=1, salt 16 bytes, output 32 bytes; AES-256-GCM nonce 12 bytes, `private` = ciphertext || 16-byte tag.
- **Key-file AAD:** UTF-8 [RFC 8785 JCS](https://www.rfc-editor.org/rfc/rfc8785) canonical JSON after removing `private` and nested `aead.nonce`; keep `aead.alg`, all other metadata and null `aead`. No trailing newline. The fixture records the AAD string/hex and exercises quotes and non-ASCII UTF-8.
- **Transcript:** each field is uint32 big-endian byte length || bytes, ordered: `bubbleplay-auth-v1`, relay origin, decimal epoch string, canonical viewer ID, viewer public, canonical PC ID, PC public, viewer nonce, PC nonce, viewer ephemeral X25519 public, PC ephemeral X25519 public. Strings are UTF-8; public keys/nonces are raw 32 bytes. Preserve epoch bytes without numeric coercion.
- **Relay proof:** each field is uint32 big-endian byte length || bytes, ordered: UTF-8 `bubbleplay-relay-v1`, UTF-8 role (`host` or `viewer`), UTF-8 relay origin (the same origin form as the transcript), raw 32-byte nonce, UTF-8 canonical device ID, raw 32-byte Ed25519 public key. H = SHA-256(encoding); Ed25519 signs the 32 bytes of H directly. The label inside the encoding is the domain separator; no extra context prefix.
- **Signatures:** H = SHA-256(transcript). PC signs `bubbleplay-auth-host-v1` || H; viewer signs `bubbleplay-auth-viewer-v1` || H || raw 64-byte PC signature. Ed25519 signs these bytes directly; contexts are raw UTF-8 without length prefixes.
- **Control:** X25519 shared secret → HKDF-SHA-256, salt H, 32-byte outputs, UTF-8 info `bubbleplay-ctl-v1 viewer-to-pc` / `bubbleplay-ctl-v1 pc-to-viewer`. Per-direction AES-256-GCM nonce: four zero bytes || uint64 big-endian sequence starting at 0. AAD: raw UTF-8 `type`; ciphertext || 16-byte tag. `plaintextUtf8` fixes sample JSON serialization; capability strings are illustrative.
- **DTLS:** each side signs raw UTF-8 `bubbleplay-dtls-v1` || H || local fingerprint || newline || remote fingerprint. Fingerprints are UTF-8 SDP `a=fingerprint` **values**, including algorithm/spaces/colons, excluding `a=fingerprint:`. Each signer uses its own local/remote order.
- **Signaling:** `auth-hello` `{type, id, public, nonce, ephemeral, caps}` (string-array `caps`); `auth-host` `{type, id, public, nonce, ephemeral, signature}`; `auth-finish`/`auth-bind` `{type, signature}`. `type` is the hyphenated message name, IDs canonical, binary fields unpadded base64url. Both sides send `auth-bind`, each signing its own local/remote order (decided 2026-10-22, D9). The fixture's `messages.authBind` is the viewer's, and `messages.authBindPc` is the PC's.

### Browser research (docs and web only)

No numeric minimum appears in `docs/` or `worker/`: [release status](../release-status.md) targets desktop Chromium; [worker guide](../../worker/README.md) says Chrome. Workers `compatibility_date` is a server setting. [MDN compatibility data](https://github.com/mdn/browser-compat-data/blob/main/api/SubtleCrypto.json): Ed25519 generate/import/sign/verify starts at Chrome 137; X25519 generate/import/deriveBits at 133. Therefore 137+ covers both, but availability at BubblePlay's minimum remains unresolved. No support floor or curve choice was changed; no browser runtime test ran.

Two browser Argon2id WASM candidates were compared:

| Candidate | Size and license | Maintenance | 64 MiB in a Web Worker |
| --- | --- | --- | --- |
| **hash-wasm 4.12.0** | Upstream [README](https://github.com/Daninet/hash-wasm) reports approximately **11 kB gzipped** for the combined Argon2d/i/id bundle with embedded WASM; not the entire package. [MIT](https://github.com/Daninet/hash-wasm/blob/v4.12.0/LICENSE). | Latest npm release 2024-11-19, corroborated by its [changelog](https://github.com/Daninet/hash-wasm/blob/v4.12.0/CHANGELOG.md); no newer npm release as checked today. | Worker support is documented. [API/source](https://github.com/Daninet/hash-wasm/blob/v4.12.0/lib/argon2.ts) accepts `memorySize: 65536` in KiB and allocates that memory plus overhead. Support at these parameters is inferred from source, not device-tested. |
| **argon2-browser 1.18.0** | [README](https://github.com/antelle/argon2-browser) reports approximately **14 kB JS + 25 kB WASM**, file sizes rather than gzip figures, excluding additional wrapper/bundling overhead. MIT. | Latest [npm release](https://registry.npmjs.org/argon2-browser) 2021-06-05; no newer npm release as checked today. Its README's SIMD/browser guidance is dated. | [Loader/source](https://github.com/antelle/argon2-browser/blob/master/lib/argon2.js) uses `self`, fetch/dynamic import and a memory allocator that permits `mem: 65536` (about 80 MiB initial WASM memory including overhead). Worker feasibility is inferred from source; asset paths and the exact build still need testing. |

Recommend **hash-wasm's Argon2 bundle** for explicit Worker support, simple loading and its newer release; its release gap remains a maintenance consideration. Its documented download is ~11 kB gzip (not directly comparable to raw file sizes). Parameters: `argon2id`, `memorySize: 65536`, `iterations: 3`, `parallelism: 1`, `hashLength: 32`, `outputType: "binary"`. Memory availability, worker startup and latency remain unverified. Nothing was vendored/added; dependency selection and the browser floor remain for the main session.

## Phase 1 decisions

2026-10-15, main session defaults (the owner may override):

1. **Viewer target:** `/ws?pc=<id>`. Without `pc`, one shared resolver serves viewer `/ws`, `/media-ws` and `/file-ws`: the most recently registered online PC with an ID; else the legacy room if an ID-less host is online; else the only listed PC; else the most recently registered listed PC; else the legacy room. Viewer signaling `hello-ok` names the resolved `pc`. A pc-less media/file socket must carry the epoch issued by signaling, and that epoch must still belong to the resolved PC's active viewer; if the default changed to another PC, refuse rather than cross-connect. Explicit `pc` and ticket routing retain their phase 1 behavior. The browser takes `?pc=` from the page URL; master Android sends neither `pc` nor tickets.
2. **ID-less hosts** keep working in the legacy room (`signal:host` / `room-v3`) until the phase 2 cutover.
3. **Host registration:** `hello` adds `id` and `public`; the relay answers `register-challenge {nonce}`; the host answers `register-proof {signature}` (encoding under "Relay proof" above). An unknown ID needs `TOKEN`; a known ID needs only the signature. The `HOST_GATE_TOKEN` bearer stays required in phase 1.
4. **Tickets:** 32 random bytes in every `hello-ok`, bound to PC ID, side and (viewers) epoch; valid while the issuing signaling socket is open, at most 12 h. Media and file URLs add `pc=`; the socket `hello` carries the ticket, never the URL. Compared in constant time and never logged. Host media and file sockets accept the bearer or a ticket in phase 1.
5. **Operator PC list:** Go `auth.json` gains an optional `pcs` map ({public, firstSeen, lastSeen}, `version` stays 1); Cloudflare stores `pc:<id>` in `GATE`. Admin routes `GET /__gate/manage/pcs` and `DELETE /__gate/manage/pcs/<id>`. The Room object for a PC is named `pc-v1:<id>`.
6. **PC key at rest:** the tray creates `%LOCALAPPDATA%\BubblePlay\device-key.bpkey` as a raw CurrentUser DPAPI blob of the plain key file and passes the key to the host in an environment variable, as with `TOKEN`; the host removes the variable from its own environment after reading it. Export and import use native dialogs.
7. **Key screens in phase 1 are optional:** a first-open offer with **Not now** plus a settings entry in the browser and Android. Unlock on open becomes mandatory at the phase 2 cutover, so today's login is untouched.

Android state, 2026-10-15 (lane `dk-androidui`, not yet on master): `ui/DeviceKeyActivity` (create, import, export with keep, re-seal or drop passphrase, change passphrase, remove), `ui/PassphraseDialog` and `ui/KeyViews`, and an offer card on the start screen (`ui/MainActivity`, flag `deviceKeyOfferDismissed` in `Prefs`). Export and passphrase work run on a worker thread and close the unlocked seed on every path. Remove calls `DeviceKeyStore.remove()`, which deletes the store's file (`device-key.bpkey` and its `.bak`) under the same lock as load and save; the UI names no file. Not run on a device; the JVM tests cover the offer, file name and error mapping only.

## Phase 2 relay protocol

Decided 2026-10-22 for the Rust relay (`relay-rs/`, lane rust-relay, M2). Constants live in `relay-rs/core/src/limits.rs`. Every failure before admission closes the socket with 1008 and an empty reason, with nothing else sent first. Every message before admission is at most 16 KiB.

**Viewer hello on `/ws?pc=<id>`** (the same exchange as host registration):

1. The viewer sends `{"type":"hello","role":"viewer","id","public","turn"?,"pair"?}`. Unknown keys are refused. `id` is canonical and must match `public`; `pair` is 32 bytes. Until the cutover the relay also accepts the old role `client`.
2. The relay answers `{"type":"register-challenge","nonce"}`. The nonce is 32 random bytes for this socket only. It is valid for one answer and 30 s.
3. The viewer answers `{"type":"register-proof","signature"}`, a relay proof with role `viewer` and the relay's configured public origin, checked with strict Ed25519 verification.
4. The relay checks admission (below). Only then does it send `hello-ok`, replay the cached capabilities and rotate the epoch. Until admission the socket receives nothing but the challenge, and it neither takes the viewer slot nor causes peer events.

Media and file sockets keep the phase 1 ticket hello. A browser socket (one with `Origin`) also needs the gate cookie. A socket without `Origin` is native and is judged by the signed hello alone.

**Admission list:**

- The host sends `{"type":"admit","viewers":[<id>...],"remotePairing":<bool>}` on its authenticated `/host-ws` after `hello-ok` and on every change.
- Each message fully replaces the list. It holds at most 256 canonical IDs with no duplicates. The relay consumes it and never forwards it. An invalid `admit` is dropped.
- The relay persists the list and the flag with the PC record. The operator's `GET /__gate/manage/pcs` shows only the count. `remotePairing` is stored but unused until remote pairing (phase 3).
- A viewer is admitted when its ID is listed. A list that was never published is empty.
- A connected viewer that drops off the list is closed 1008.

**Pairing window (pairing at the PC):**

- The host sends `{"type":"pair-open","admit":<b64url SHA-256("bubbleplay-pair-admit-v1" || canonical PC ID || code)>}`. It also sends `{"type":"pair-close"}`. The relay consumes both.
- There is one window per PC. It is kept in memory only, ends 300 s after `pair-open`, and is replaced by a newer `pair-open`. It is removed by `pair-close` or by the host's signalling socket closing.
- An unlisted viewer adds `pair` (the same hash, computed from the code it scanned or typed) to its hello. After a valid proof, the relay compares the value in constant time.
  - On a match, the window is consumed and the socket is admitted in **pairing mode**: `hello-ok` carries `"pairing":true` and no ticket.
  - Failures are limited to 5 per client prefix per 300 s. After 20 failures against a PC, its window is dropped and the host gets `{"type":"pair-closed"}`. This cap against brute force lets a stranger who knows the PC ID end a pairing window early. The tray then says so, and the user opens a new one.
- Pairing mode takes the normal viewer slot.
  - It forwards only `auth-hello`, `auth-finish`, `auth-bind` and `pair-proof` from the viewer.
  - It forwards only `auth-host`, `auth-bind`, `pair-proof`, `pair-done` and `pair-denied` from the host.
  - Everything else is dropped in both directions, including binary frames.
  - A later `admit` does not close it. The viewer reconnects normally once it is listed.
- Remote pairing (B) is phase 3.

**Forwarded phase 2 types.** Each has exact keys, a direction and a size cap. None is buffered while the host is away. Anything with the wrong direction, shape or size is dropped.

| Type | Direction | Cap | Fields |
| --- | --- | --- | --- |
| `auth-hello` | viewer to PC | 2 KiB | `id`, `public`, `nonce`, `ephemeral`; `caps`: at most 32 strings of 1-64 chars `[a-z0-9-]` |
| `auth-host` | PC to viewer | 2 KiB | `id`, `public`, `nonce`, `ephemeral`, `signature` |
| `auth-finish` | viewer to PC | 1 KiB | `signature` |
| `auth-bind` | both | 1 KiB | `signature` |
| `pair-proof` | both | 1 KiB | `mac` (32 bytes) |
| `pair-done`, `pair-denied` | PC to viewer | 1 KiB | none |
| `ctl` | both | 512 KiB signal cap | `kind` `[a-z0-9-]{1,32}`, `seq` an integer 0 to 2^53-1, `data` base64url of at most `CTL_DATA_MAX` (263,168) chars |

`CTL_DATA_MAX` is the base64url length of the largest clipboard chunk the relay allows today (`CLIPBOARD_RAW_MAX`, 196,864 bytes) plus the 16-byte AEAD tag, rounded up to the next KiB. The relay does no cryptography on these types and does not track `seq`.

Binary signalling frames are forwarded from the viewer to the PC only.

**Cutover commit** (lands with the host and client lanes, not before):

- The relay accepts only the roles `viewer`, `media-viewer` and `file-viewer`. `client`, `media-client` and `file-client` are refused.
- Input, clipboard and other control travel only inside `ctl`. The plaintext signalling types for them are removed from the allowlist. So are plaintext binary input frames from the viewer, which in WSS mode travel as `ctl` with `kind` `input` and `data` = AEAD(the binary frame). The list of types comes from the host's routes when the commit is made.

**Proposed for the host and client lanes** (the lane's proposals, not settled; the relay forwards these as opaque bounded messages):

- *Pairing exchange (3b):*
  - The code is 10 Crockford base32 characters from the PC's CSPRNG, normalised like IDs.
  - Pairing reuses `auth-hello`, `auth-host` and `auth-finish`, with `pair-v1` in `caps`.
  - Each side sends `pair-proof` with `mac` = HMAC-SHA256(HKDF(code, "bubbleplay-pair-v1"), "viewer" or "pc" || H), where H is the transcript hash.
  - The comparison number is the first 4 bytes of SHA-256("bubbleplay-pair-sas-v1" || H), as big-endian u32 mod 10^6, zero-padded to 6 digits.
  - The PC ends with `pair-done` or `pair-denied`.
- *Control channel (6):*
  - The envelope is `{"type":"ctl","kind":<inner type>,"seq","data":<ciphertext || tag>}`. AAD is `kind`, and after decryption `plaintext.type` must equal `kind`.
  - The receiver accepts only `seq` equal to the expected value. Any gap, repeat or authentication failure ends the session.
  - Each handshake brings new keys and starts again at `seq` 0.
- *Caps (8):*
  - Unknown caps are ignored, and a missing cap means the feature is off.
  - Every cap needed at cutover (`control-aead-v1`, and `dtls-bind-v1` in WebRTC mode) is mandatory. A relay that strips caps can only cause a refusal, not a downgrade.
  - Caps are not in the transcript (above), so the endpoints may repeat them inside `ctl` after the handshake.
- *PC key rotation:* there is no relay change. The new ID registers with `TOKEN`, and the operator removes the old record. The `rotate` statement reaches viewers inside `ctl`.
