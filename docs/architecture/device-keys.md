# Device IDs and keys

Status: approved 2026-10-13 with these decisions (see [Decisions](#decisions)). Roadmap entry: [Device IDs and keys](../roadmap.md#device-ids-and-keys). [Wake-on-LAN](../roadmap.md#wake-on-lan) and [unattended access](unattended-access.md) depend on it.

Goal, in the owner's words: "a relay / ID and key based system, kinda like ssh keys but with IDs instead of IPs; we'd need a little GUI for management." Every device has a key, kept in a portable key file like an SSH key, and an ID derived from its public key. The relay routes by ID. The PC keeps its own list of trusted keys, like `authorized_keys`, and manages it in the tray. Device keys are the only way to connect; the application password and its session login are removed.

## Current state

Code reads on 2026-10-13; no live tests. This is what the plan replaces.

- **The relay is the authenticator; the endpoints trust it.** The host checks nothing about the viewer: its `hello` carries no viewer identity (`host/src/signaling.rs` 1298-1302).
- **One room per relay.** Go keys peers as `signal:host` and `signal:client` (`relay/server.go` 403-408); Cloudflare maps every socket to the Durable Object `room-v3` (`worker/src/index.js` ~127). There is no room ID. A viewer `hello` mints the session epoch that pairs media and file sockets (`server.go` 463-466).
- **Host registration:** bearer `HOST_GATE_TOKEN` on the upgrade, then `token` equal to `TOKEN` in the `hello` (`server.go` 319-322, 433-440; `index.js` 81-84, 347-362). Both are 32+ character shared secrets, stored with CurrentUser DPAPI in `%LOCALAPPDATA%\BubblePlay\host.json` (`tokenDpapiHex`, `hostGateTokenDpapiHex`; `host/scripts/setup-tray.ps1` 203-208, 347-351; `host/src/tray/logic.rs` 258-266) and passed to the host as environment variables (`tray/supervisor.rs` 117-161).
- **Viewer login:** one shared application password per relay, no accounts. Go stores an Argon2id hash (`relay/auth.go` 57, 79) and offers TOTP, ntfy or host approval; Cloudflare compares a SHA-256 digest of `APP_PASSWORD` (`worker/src/gate.js` 380-382) and offers ntfy or host approval. The result is two `__Host-` cookies (12 h, or 30 days remembered; `auth.go` 38-39, 363-368), rechecked on every viewer socket ([browser approval](browser-approval.md)).
- **Android** signs in through a WebView and replays its cookies on `/ws`, `/media-ws` and file sockets (`android/.../ui/LoginActivity.kt` 21-27, `net/Session.kt` 306-316). It stores one server URL in plain `SharedPreferences` (`ui/Prefs.kt` 48-54) and has no key material and no list of PCs.
- **No binding.** Signaling forwards SDP after a size check (`relay/protocol.go` 100-102). Nothing ties the WebRTC DTLS fingerprints, or the WSS stream, to an authenticated party. A relay can impersonate either end, read and inject input and clipboard on `/ws`, and read WSS media ([security and privacy](../security-and-privacy.md)).
- **Permission gates today** are per PC, not per viewer: Admin mode in `HKLM\SOFTWARE\BubblePlay` (`host/src/admin_mode.rs` 93-102, 217-231), the secure-desktop input gate (`host/src/input.rs` 3556-3584), and file transfer enabled whenever the host gate token is present (`host/src/main.rs` 950-963). There is no view-only mode.

What changes: the endpoints authenticate each other with keys, and the relay only routes and keeps strangers out. Password login, TOTP and host approval, the session cookie and the Android WebView login are deleted. ntfy approval stays at the relay as a [site gate](#site-gate) in front of the browser page, unrelated to device keys. On the PC, ntfy becomes the second factor for pairing a device remotely and for pre-login sessions.

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
| PC user host | `%LOCALAPPDATA%\BubblePlay\device-key.bpkey`, CurrentUser DPAPI-wrapped, no passphrase, because the host starts unattended at sign-in. | None. | Tray **Export key** asks for an optional export passphrase; **Import key** keeps a PC's ID across a reinstall. |
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

- **Rotation:** a device creates a new key and signs a `rotate` statement with the old one, naming the new public key. Each PC it is paired with replaces the entry and keeps its name and permissions. A PC rotating its own key does the same towards its viewers; a viewer that has not seen the statement shows "PC key changed" and refuses, as SSH does. Manual only; no expiry.
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

### B. Remotely, confirmed through ntfy

For pairing while away from the PC. Off by default (**Allow remote pairing** in the tray); a device paired this way gets view and input only and is flagged "paired remotely" in the tray and in the event log.

1. The viewer asks the relay to pair with a PC ID, without a code. The relay forwards the request only if the PC has remote pairing on (it publishes that flag) and within rate limits: one pending request per PC, a few per client prefix per hour.
2. Viewer and PC exchange keys; both compute the comparison number. The viewer shows it.
3. **The PC itself** publishes to the owner's ntfy topic: "Pair 'Work laptop' (ID M3ZD-...) with Office-PC? Number 482 913", with Approve and Deny actions. The relay never sees the number.
4. The action URLs carry a one-time capability the PC generated for this exact request (bound to the viewer key). They point at the relay, which forwards the click to the PC; the PC accepts only the matching capability, once, before expiry.
5. The owner compares the number on the phone with the one on the new device and approves.

Why this holds against a relay in the middle: if the relay substitutes its own key, the PC's notification shows that key's number, which differs from the one on the owner's device. The relay cannot approve on its own either, because it does not know the capability until the owner clicks, and that click approves only the key in the notification. What it does not cover: an ntfy server and relay run by the same hostile operator could read the capability and approve before the owner. In the self-hosted setup both are the owner's.

ntfy settings (URL, topic, publish token) move from the relay to the PC: `host.json`, DPAPI-wrapped, set in setup and the tray. The logon host needs the same for its per-session second factor (below).

### Both relay types

Pairing runs end to end over the signaling socket, so Go and Cloudflare carry it the same way. Each relay needs: routing by PC ID, admission of unpaired devices only for an open pairing window (A) or a PC with remote pairing on (B), forwarding of the ntfy action clicks to the PC, and the new message types. Neither relay holds a password or TOTP seed any more. The relay's own ntfy settings serve only the site gate; the PC's ntfy settings for pairing are separate and may point at the same server and topic.

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
- **Viewers connect to a PC ID.** `hello` carries `target: <pc id>`, the viewer's ID and a signature over a relay nonce; the relay answers with a ticket for that epoch's media and file sockets. The session cookie is gone; browsers also need the site gate cookie.
- **Admission without accounts:** each PC publishes the list of viewer IDs it trusts (IDs only) and whether remote pairing is on. The relay admits a viewer to a PC's room only if its ID is listed, or for an open pairing window or a remote pairing request. Strangers never reach the host's parser beyond the pairing messages, and the PC still checks every key itself.
- **What the relay stores:** per PC: ID, public key, last seen, the published viewer-ID list, the remote-pairing flag, a hash of an open pairing code. Separately, the site gate's ntfy settings and hashed gate sessions, as today. No names, permissions, passwords, or second-factor secrets for pairing or pre-login. Go keeps it in `auth.json`, Cloudflare in the `GATE` Durable Object storage. Rooms are per PC ID: Go keys peers as `signal:host:<id>` and `signal:client:<id>`; Cloudflare names the `ROOM` object after the PC ID instead of `room-v3`.
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

Each trusted key has permissions, set at pairing and editable in the tray. The host enforces them after the handshake. Defaults are for pairing at the PC; remote pairing gets view and input only.

| Permission | Default | Existing gate it maps to |
| --- | --- | --- |
| View | on, always | The session itself. |
| Input (pointer, keyboard, text) | on | New check at the top of the input route (`host/src/input.rs` 3533). Off gives the view-only mode that is missing today. |
| Clipboard | on | New check in the clipboard control path. |
| Files | on | Replaces "host gate token present" (`host/src/main.rs` 950-963). |
| Elevated input | off | Admin mode stays the machine switch in HKLM, set through UAC. Elevated input needs Admin mode **and** this permission. The host enforces the device part, so the residual same-user risk in [security and privacy](../security-and-privacy.md) is unchanged. |
| Before sign-in | off | The logon host admits only keys with this permission, plus a fresh second factor per session: ntfy approval sent by the logon host, or an authenticator code checked by the logon host and sent inside the authenticated control channel. |
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
 WoL helper     Q4XD-8B2M-ZT7C-1KHR   now                Wake                    [Edit] [Revoke]

 [ ] Allow remote pairing (confirmed on your phone via ntfy)      ntfy: ntfy.example.com / bp-pc  [Change]
```

- Each row is one key. A key seen from more than one client kind or network in the last 30 days expands to its recent connections, so a shared key file is visible.
- **Edit** opens name and permission checkboxes; "Elevated input" and "Before sign-in" are greyed with a note while their machine switch is off, and changing "Before sign-in" asks for UAC.
- **Revoke** asks once, removes the key, ends its session and syncs the relay list.
- **Pair device** shows the code, the QR and the confirmation from pairing at the PC, inside this window.
- **Allow remote pairing** is greyed until ntfy is configured.
- Last seen comes from the host's own records, not the relay.
- Native window, like the existing approval dialog, following the BubblePlay product profile.

## Cutover and removal

No migration period: device keys are the only way to connect. Relay, host and clients are updated together; an old client or host stops working against a new relay. There is no public release yet, so only the owner's own setups are affected.

Deleted in phase 2:

- **Go relay:** application password (`passwordHash`, Argon2id login in `auth.go`), TOTP login and replay state, the `__Host-bp-session` cookie and its revocation routes, `/__auth/login`, `/__auth/methods`, the `host` notifier and `approvalMethods`, `hostGateToken`; `init` and `rotate` lose `--password`, `--totp` and `--methods`.
- **Cloudflare relay:** `APP_PASSWORD`, `HOST_GATE_TOKEN`, `APPROVAL_METHODS`, the password login and session cookie in `gate.js`, the host notifier.
- **Browser:** `login.html` and the password form; the approval wait page is reduced to the minimal site-gate page. The key unlock or create/import screen and the PC list replace the login.
- **Host:** the browser approval dialog (`host/src/browser_approval.rs`) and `browserApproval` capability, `HOST_GATE_TOKEN` handling in setup, tray and signaling.
- **Android:** `LoginActivity` WebView login, cookie storage and replay, "Forget browser".
- **Docs and fixtures:** [browser approval](browser-approval.md) rewritten as the site gate (ntfy only); the host-approval messages in `fixtures/browser-approval-contract.json` removed; setup guides rewritten for key pairing.

Kept: the ntfy site gate with the `__Host-gate` cookie and `/__gate/decision`, `TOKEN` as the first-registration ticket, the admin token for operator management, the per-prefix rate limiting (now also on pairing requests and handshakes), TURN credential issuance.

## Threats and mitigations

| Threat | Mitigation | Remaining |
| --- | --- | --- |
| Unauthenticated probing of the site and relay | Site gate: nothing of the page is served before ntfy approval; the wait page, errors and headers name no product; `/__auth/methods` removed, `/healthz` restricted; native sockets get only a nonce before a valid key signature. | The wait page itself and the TLS certificate show that something is hosted; a visitor can trigger notifications within the rate limits. |
| Stolen key file | Passphrase with Argon2id; on Android also the Keystore wrap; revoke in the tray. | An unprotected file works until revoked. |
| Key file shared across devices | Shown per connection in the tray. | One revocation covers all of them. |
| Malicious or compromised relay splicing a session | Handshake pinned to stored keys; DTLS fingerprints signed; control messages authenticated. | WSS media readable until the E2E phase. Browser clients run relay-served script. |
| Relay brute-forcing a pairing code | Comparison number confirmed on the PC; code single-use, 5 minutes. | None known. |
| Relay faking a remote pairing | Comparison number and a one-time capability, both created by the PC, sent through ntfy. | A hostile operator running both relay and ntfy server. |
| Strangers spamming remote pairing | Off by default; one pending request per PC; per-prefix rate limits at the relay and the PC. | Anyone who knows relay URL and PC ID can trigger a notification within those limits. |
| ID squatting at the relay | ID must match the key hash and a signature; first registration needs `TOKEN`. | Denial of service by a holder of `TOKEN`. |
| Other same-user process on the PC adding a device | Trusted list DPAPI-wrapped; pre-login list needs UAC. | Same residual risk as Admin mode today. |
| PC key copied by malware running as the user | DPAPI at rest. | A same-user process can read it; same boundary as today's host credentials. |
| Replay of a handshake or input | Fresh nonces and epoch in the transcript; per-message sequence numbers. | None known. |

Stays prohibited: accepting a key without the PC-side confirmation or the PC-sent ntfy approval; pairing before sign-in; any password or shared-secret login for viewers; treating the site gate as access to a PC; serving any page asset before the site gate; the relay storing names, permissions or the PC's second-factor secrets; writing an unlocked key to disk; any inbound listener on the PC; and everything already prohibited in [unattended access](unattended-access.md#recommendation).

## Phased plan

| Phase | Content | Size | Unblocks |
| --- | --- | --- | --- |
| 0 | `fixtures/device-keys-contract.json`: ID derivation, key file vectors (with and without passphrase), message shapes, transcript and signature vectors, for Rust, Go, JS and Kotlin. Check X25519 and Ed25519 in the minimum Chromium; pick and vendor the Argon2id WASM. | S | everything |
| 1 | Key files on host, browser and Android: create, import, export, passphrase, unlock screen. Hosts register by ID; both relays route rooms by PC ID; tickets for media and file sockets; operator PC-ID list. | M | multiple PCs; logon host and helper as separate IDs |
| 2 | Pairing at the PC, handshake with DTLS binding and the authenticated control channel, trusted list, tray Devices window, viewer PC lists, relay admission by published IDs. **Cutover:** delete the password, TOTP, host approval, session cookie and `HOST_GATE_TOKEN` paths in relay, worker, browser, host and Android (see above); reduce the site gate to ntfy only, with the minimal wait page, and confirm every asset sits behind it. | L | **Wake-on-LAN** (helper pairs, verifies signed wake requests; all paired keys may wake until phase 3) |
| 3 | Per-device permissions in the host gates, ntfy on the PC, remote pairing through ntfy, pre-login trusted list and logon host key in ProgramData, per-session second factor for pre-login. | M | **Unattended access** step 2, which needs "Before sign-in", the machine-scoped list and the PC-side second factor |
| 4 | Separate proposal: WSS media and file E2E with the phase 2 keys. | M-L | release-status future work |

Sizes: S under a week, M one to three weeks, L three to six weeks, each with tests against the shared fixture. Phase 2 is one release for all components, since the old login disappears with it.

## Open questions for the owner

1. **Authenticator codes for remote pairing are not sound against a relay in the middle.** A code typed on the new device either travels unbound (the relay forwards it with its own key) or bound by a MAC that a 6-digit code cannot protect (the relay tries all million codes offline). The design therefore uses ntfy only for remote pairing; an owner without ntfy pairs at the PC. Authenticator codes stay usable for the pre-login second factor, where they travel inside an already authenticated channel. Confirm, or accept authenticator pairing as a weaker option that only trusts the relay.
2. Remote pairing has no password gate any more: anyone who knows the relay URL and a PC ID can make the owner's phone show a pairing request, within the rate limits. Acceptable, or should remote pairing be limited further (for example only for a few hours after the owner turns it on)? For a browser this needs the site gate first; for the Android app it does not.
3. AGPL-3.0 asks the relay to offer its source to users who interact with it over a network. The minimal wait page shows no source link, so that it names nothing; the link is behind the gate. Is that acceptable, or should the wait page carry a plain unbranded "Source" link?

## Decisions

2026-10-13, owner:

1. **Portable key files, SSH-style.** Keys can be exported and imported; each key has an optional passphrase. Browser and Android ask for it on open, and offer **Create key** and **Import key** when there is none. No non-extractable WebCrypto key and no Keystore-bound identity; the Keystore may wrap the file at rest. This supersedes the earlier recommendation of a non-extractable browser key with optional passkeys. Passkeys remain a possible later option, not planned. Consequences above: Ed25519 instead of P-256, the `.bpkey` format with Argon2id and AES-256-GCM, unlocked keys in memory only, and one tray row per key with connections shown per place.
2. **Remote pairing:** allowed, off by default, view and input only, flagged in the tray. The comparison number goes to the owner's phone through ntfy, sent by the PC, so the relay cannot fake it.
3. **Android fingerprint or PIN per connection:** replaced by the passphrase-on-open rule.
4. **No legacy path:** the application password, its session login, TOTP login and host approval are deleted at once, with no compatibility mode, no "Require device keys" switch and no migration period. Device keys are the only way to connect. Kept: `TOKEN` for a PC's first registration, and ntfy (on the PC) as the authentication for remote pairing.
5. **Site gate stays:** ntfy approval in front of the browser page, before the relay serves anything of the site, independent of device keys and not part of the removal. Proposed details above: 30-day gate cookie per browser, a minimal unbranded wait page, Android exempt.
