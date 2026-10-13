# Device IDs and keys

Status: proposed 2026-10-13, for owner approval. Roadmap entry: [Device IDs and keys](../roadmap.md#device-ids-and-keys). [Wake-on-LAN](../roadmap.md#wake-on-lan) and [unattended access](unattended-access.md) depend on it.

Goal, in the owner's words: "a relay / ID and key based system, kinda like ssh keys but with IDs instead of IPs; we'd need a little GUI for management." Every device has a key pair and an ID derived from its public key. The relay routes by ID. The PC keeps its own list of trusted device keys, like `authorized_keys`, and manages it in the tray. Settled before this proposal: the trusted-device list lives locally on the PC.

## Current state

Code reads on 2026-10-13; no live tests.

- **The relay is the authenticator; the endpoints trust it.** The host checks nothing about the viewer: its `hello` carries no viewer identity (`host/src/signaling.rs` 1298-1302).
- **One room per relay.** Go keys peers as `signal:host` and `signal:client` (`relay/server.go` 403-408); Cloudflare maps every socket to the Durable Object `room-v3` (`worker/src/index.js` ~127). There is no room ID. A viewer `hello` mints the session epoch that pairs media and file sockets (`server.go` 463-466).
- **Host registration:** bearer `HOST_GATE_TOKEN` on the upgrade, then `token` equal to `TOKEN` in the `hello` (`server.go` 319-322, 433-440; `index.js` 81-84, 347-362). Both are 32+ character shared secrets, stored with CurrentUser DPAPI in `%LOCALAPPDATA%\BubblePlay\host.json` (`tokenDpapiHex`, `hostGateTokenDpapiHex`; `host/scripts/setup-tray.ps1` 203-208, 347-351; `host/src/tray/logic.rs` 258-266) and passed to the host as environment variables (`tray/supervisor.rs` 117-161).
- **Viewer login:** one shared application password per relay, no accounts. Go stores an Argon2id hash (`relay/auth.go` 57, 79) and offers TOTP, ntfy or host approval; Cloudflare compares a SHA-256 digest of `APP_PASSWORD` (`worker/src/gate.js` 380-382) and offers ntfy or host approval. The result is two `__Host-` cookies (12 h, or 30 days remembered; `auth.go` 38-39, 363-368). Every viewer socket rechecks them ([browser approval](browser-approval.md)).
- **Android** signs in through a WebView and replays its cookies on `/ws`, `/media-ws` and file sockets (`android/.../ui/LoginActivity.kt` 21-27, `net/Session.kt` 306-316). It stores one server URL in plain `SharedPreferences` (`ui/Prefs.kt` 48-54) and has no key material and no list of PCs.
- **Browser** keeps no credential in script reach (HttpOnly cookies), no IndexedDB.
- **No binding.** Signaling forwards SDP after a size check (`relay/protocol.go` 100-102). Nothing ties the WebRTC DTLS fingerprints, or the WSS stream, to an authenticated party. A relay, or anyone holding its admin and host tokens, can impersonate either end, read and inject input and clipboard on `/ws`, and read WSS media ([security and privacy](../security-and-privacy.md)).
- **Permission gates today** are per PC, not per viewer: Admin mode in `HKLM\SOFTWARE\BubblePlay` (`host/src/admin_mode.rs` 93-102, 217-231), the secure-desktop input gate (`host/src/input.rs` 3556-3584), and file transfer enabled whenever the host gate token is present (`host/src/main.rs` 950-963). There is no view-only toggle.

What changes: the endpoints authenticate each other with keys. The relay keeps routing and abuse control but stops being the thing that grants access to the desktop. The password and second factor remain, for pairing and as a fallback.

## Identity

- **Key type: ECDSA P-256 (ES256) everywhere.** It is the one curve every key store here supports non-exportably: Windows CNG with the TPM (Microsoft Platform Crypto Provider) and the software provider, Android Keystore including StrongBox, WebCrypto in all current browsers, and WebAuthn passkeys. Ed25519 is nicer but has no TPM or StrongBox support. Signatures use a fixed-length raw `r||s` encoding.
- **Device ID:** the first 80 bits of `SHA-256(SubjectPublicKeyInfo DER)`, written as 16 Crockford base32 characters in four groups: `B7QK-2M9X-H4PD-R8TN`. Case-insensitive, no 0/O or 1/I/L confusion. 80 bits makes finding a second key with the same ID infeasible (about 2^80 work), and the full key is pinned at pairing anyway. The ID is both the routing address and the fingerprint people compare; there is no separate fingerprint format.
- **Where keys live:**

  | Device | Key store | Notes |
  | --- | --- | --- |
  | PC user host | CNG, TPM provider if present, else software provider; per user, non-exportable | Replaces nothing at first; `TOKEN` stays for relay registration until phase 3. |
  | Pre-login logon host | Machine-scoped CNG key owned by `NT SERVICE\BubblePlayLogon`, ACL SYSTEM and that SID | Its own ID, as already decided in [unattended access](unattended-access.md#decisions). |
  | Android | Android Keystore, StrongBox when available, `setUserAuthenticationRequired` off by default | One key per install; all paired PCs see the same ID. |
  | Browser | Non-extractable WebCrypto key in IndexedDB (recommended), passkey optional; see below | One key per browser profile and relay origin. |
  | Wake-on-LAN helper | Private key file, mode 600, in its state volume | A device like any other, with only the `wake` role. |

- **Rotation:** a device signs a `rotate` statement with its old key naming the new public key; each PC it is paired with replaces the entry and keeps its name and permissions. A PC rotating its own key does the same towards its viewers; until a viewer has seen the statement it shows "PC key changed" and refuses, as SSH does. Rotation is manual (tray button, client setting); no expiry by default.
- **Revocation** is local and immediate: removing a device from the PC's list ends its open session and refuses it from then on. Nothing else needs to be told; the relay's admission list (below) updates on the next sync.
- **Lost device:** revoke it in the tray. A lost PC is the reverse case: the viewer deletes the PC entry; the key is useless without the PC. If the PC itself is gone, nothing on the relay needs cleanup beyond its registration.
- **Lost browser key** (site data cleared, new profile): the browser just looks like a new device and pairs again. The old entry stays in the tray until revoked, shown as "not seen for N days".

### Browser keys: passkey or WebCrypto key

| | Non-extractable WebCrypto key | Passkey (WebAuthn) |
| --- | --- | --- |
| What it binds to | One browser profile on one device | Often synced across all the user's devices (iCloud, Google, password manager), so one "device" may be many |
| Per-connection friction | None | A system prompt (touch, PIN or face) on every connect |
| Protection of the key | Software, in the browser profile; script on the origin can use it but cannot read it | Hardware or the platform authenticator; user presence on each use |
| Survives "clear site data" | No; pair again | Yes |
| Relay domains | Separate key per relay origin | Bound to the relay's domain as rpId; same limit |
| Host verification | A plain signature over the transcript | Verify `authenticatorData`, `clientDataJSON` origin and challenge, then the signature; more code, well specified |
| Against a malicious relay | Weak, see below | Weak, see below |

Recommendation, a deviation from the default in the roadmap: **the WebCrypto key is the browser's device key**, and passkeys are an optional later add-on ("require a passkey to connect from this browser", phase 4). Reasons: a device list only works if one entry is one device, and synced passkeys break that; a prompt on every connect is friction for the everyday case the keys are meant to make easier; and passkeys do not protect against the main browser weakness either.

That weakness applies to both: **the relay serves the browser's JavaScript.** A relay that turns hostile can ship script that uses the key (or asks for a passkey assertion the user will approve as usual) while the page is open. Device keys protect a browser against a relay that is compromised in routing only, and against everyone else; they do not protect it against the relay operator. The Android app and the PC do not have this problem because their code does not come from the relay. Owners who self-host on their own server are both parties, which is the expected setup today.

## Pairing

Pairing adds a viewer's public key to a PC's trusted list, with a name and permissions, and gives the viewer the PC's ID and key. Two ways in:

### A. At the PC (recommended path)

1. Owner clicks **Pair device** in the tray. The tray shows for 5 minutes: the PC's ID, a one-time 10-character code, and a QR code with `bubbleplay://pair?relay=<url>&pc=<id>&code=<code>` (the browser uses the `https://<relay>/pair#...` form; the fragment never reaches the relay).
2. The viewer scans the QR or types PC ID and code. It connects to the relay as an unpaired device asking for that PC.
3. Viewer and PC exchange public keys and a pairing proof keyed by the code (HMAC over the transcript), so the PC accepts only requests from someone who saw its screen.
4. Both sides show a 6-digit comparison number derived from the transcript. The tray shows "Pair 'Pixel 8' (ID B7QK-...)? Number on the device: 482 913" with the default permissions and **Allow** / **Deny**. The relay cannot fake the number without changing the keys, so this defeats a relay in the middle. The code alone would not: a short code under HMAC could be brute-forced by a relay within its lifetime.
5. The PC adds the entry; the viewer stores the PC (ID, key, relay URL, name).

### B. Remotely, with password and second factor

For pairing a new device while away from the PC, which is today's login. The viewer signs in at the relay with the application password and a second factor as today, then asks the PC to pair. The PC accepts when **Allow remote pairing** is on, gives the device the remote-pairing default permissions (view and input), shows a tray notification and an entry marked "paired remotely", and writes an event log entry.

This path trusts the relay: whoever controls the relay could pair a key of its own. That is the same trust as today, limited to pairing instead of every connection. A later option closes it: the PC itself posts the comparison number to the owner's ntfy topic, so the relay never carries it (open question 3).

### First run on a fresh install

`setup-tray.ps1` creates the PC key and shows its ID next to the relay settings. The relay registration still uses `TOKEN` and `HOST_GATE_TOKEN` as today (phase 1-2). The tray's first-run screen ends with **Pair your first device**, which opens flow A. Pairing before sign-in is not possible, as decided for unattended access; the logon host gets its trusted list from the user host (see permissions).

### Both relay types

Pairing runs end to end over the signaling socket, so Go and Cloudflare carry it the same way. Each relay needs the same three additions: route by PC ID, admit unpaired devices only for an active pairing (flow A) or after login (flow B), and forward the new message types. Flow B uses each relay's existing login: Go with TOTP, ntfy or host approval, Cloudflare with ntfy or host approval. Host approval during flow B is redundant with the pairing confirmation, so a relay offering only `host` effectively supports only flow A.

## Routing and the relay

- **Hosts register by ID.** On `/host-ws` the host sends its ID and public key and signs a relay nonce. The relay checks that the ID matches the key's hash and the signature. One relay can then serve several PCs, and the user host and logon host of the same PC are just two IDs.
- **Who may register:** the operator's registration secret (`TOKEN`) stays as the admission ticket for a host's first registration; after that the relay remembers the admitted host ID and its key. The operator can list and remove host IDs with the admin token. This is a list of PC IDs, not accounts.
- **Viewers connect to a PC ID.** `hello` gains `target: <pc id>` and the viewer's ID. With no `target`, a relay with exactly one registered host routes to it, so old clients keep working.
- **Admission without accounts:** each PC publishes to the relay the list of viewer IDs it trusts (IDs only, no names or permissions). The relay admits a key-holding viewer to that PC's room only if its ID is listed and it signs the relay's nonce. Unpaired viewers reach a PC only through an open pairing window or a password login. This keeps internet strangers away from the host's parser without the relay deciding anything security-relevant: the PC still checks the key itself.
- **What the relay stores:** per admitted PC: ID, public key, last-seen time, the published viewer-ID list. No names, no permissions, no pairing codes in clear (only a hash to match a pairing request to its PC). Go keeps it in `auth.json`; Cloudflare in the `GATE` Durable Object storage. Rooms become per PC ID: Go keys peers as `signal:host:<id>` and `signal:client:<id>`; Cloudflare uses a `ROOM` object named after the PC ID instead of `room-v3`.
- **What the relay learns:** which device IDs talk to which PC and when, and their IP prefixes, as it already sees today. It no longer needs the password for everyday connections.
- **Does the relay need accounts?** No. One operator, one password for pairing, a list of admitted PC IDs. Multi-user accounts stay out of scope ([release status](../release-status.md)).
- **One room still pairs one host with one viewer**; a second viewer for the same PC gets "PC busy" as today. Multi-viewer is separate work.
- **Multi-PC on the viewer:** the browser and Android keep a list of paired PCs (name, ID, relay URL, online state from the relay). Android gains the list the app lacks today; the browser's list lives in IndexedDB beside the key, per relay origin.

## Handshake

After the relay joins viewer and PC, and before the host starts capture or accepts any input, they run a mutual authentication over the existing signaling socket. Messages, all JSON on `/ws` and `/host-ws`:

1. Viewer → PC `auth-hello`: viewer ID, public key, 32-byte nonce, ephemeral P-256 ECDH key, capabilities.
2. PC → viewer `auth-host`: PC ID, public key, nonce, ephemeral ECDH key, signature over the transcript so far.
3. Viewer → PC `auth-finish`: signature over the full transcript.

The transcript is `"bubbleplay-auth-v1"`, the relay origin, the session epoch, both IDs and public keys, both nonces and both ephemeral keys. The viewer checks the PC key against its pinned key; the PC checks the viewer key against its trusted list and loads that device's permissions. Either side closes on a mismatch, with a reason the user sees ("PC key changed", "This device is not paired").

**Binding to the transport, so a relay cannot splice:**

- **WebRTC:** each side adds a second pair of signatures once DTLS is up, over the local and remote certificate fingerprints taken from the local and remote SDP (`a=fingerprint`). DTLS already checks the remote certificate against the remote SDP, so a relay that rewrote SDP to sit in the middle produces fingerprints the peer did not generate; the signatures fail and the host refuses. Media and input wait for this step.
- **WSS and the control channel:** there is nothing end to end to bind to, because TLS ends at the relay. Input and clipboard travel on `/ws` in both transport modes. The ephemeral ECDH keys give both ends a shared secret no relay knows; HKDF derives per-direction keys. **Without using those keys, the handshake proves who connected but a relay can still inject input after it.** So phase 3 authenticates every control and input message with AES-GCM under these keys (sequence number as nonce, replay refused). Input packets are small; the cost is negligible.

**End-to-end encryption for WSS media** (future work in [release status](../release-status.md)) becomes possible with the same keys: encrypt each WSS v4 access unit with AES-GCM, the 28-byte header as associated data, generation and sequence as the nonce. In scope here: the key schedule and a capability flag. Out of scope: the media framing change, browser decode-path performance of per-frame WebCrypto, and file-socket encryption. Those are a separate proposal (phase 5). The browser caveat above still applies: E2E protects against a relay that only forwards, not against one that serves hostile script.

## Permissions

Each trusted device has permissions, set at pairing and editable in the tray. The host enforces them after the handshake. Defaults are for flow A; flow B gets view and input only.

| Permission | Default | Existing gate it maps to |
| --- | --- | --- |
| View | on, always | The session itself; a device without view is not paired. |
| Input (pointer, keyboard, text) | on | New check at the top of the input route (`host/src/input.rs` 3533). Off gives a view-only session, which also adds the missing view-only mode. |
| Clipboard | on | New check in the clipboard control path. |
| Files | on | Today's gate is "host gate token present" (`host/src/main.rs` 950-963); becomes "device has Files". |
| Elevated input | off | Admin mode stays the machine switch in HKLM, set through UAC. Elevated input needs Admin mode **and** this permission. The host enforces the device part, so the residual same-user risk in [security and privacy](../security-and-privacy.md) is unchanged. |
| Before sign-in | off | The logon host admits only devices with this permission, plus the fresh second factor already decided in [unattended access](unattended-access.md#decisions). |
| Wake | on | The Wake-on-LAN helper accepts wake requests only from devices the PC marked so. |

**Lists for the logon host and the helper.** The user host is the only place the owner edits. When the owner gives a device "Before sign-in", the tray writes that device's ID and key to `ProgramData\BubblePlay\prelogin\trusted.json` through the same UAC-elevated step that turns on pre-login (ACL: SYSTEM, the service SID, Administrators). A same-user process can therefore not add pre-login devices. The Wake-on-LAN helper is paired with the PC like a viewer (flow A); while awake, the PC sends it, end to end, its MAC address and the IDs and keys of devices with "Wake". The helper verifies a viewer's signed wake request against that list.

## Tray management window

A small window opened from the tray menu (**Devices…**), next to the existing Admin mode, Start at login and Logs items. It replaces nothing in the menu.

```
BubblePlay — Devices                                        [Pair device]
This PC   Office-PC   ID 7HQ2-K9MX-4RTD-P0ZB   Relay play.example.com (online)

 Name           ID                    Last seen          Permissions             
 Pixel 8        B7QK-2M9X-H4PD-R8TN   now (connected)    View Input Clip Files   [Edit] [Revoke]
 Laptop Chrome  M3ZD-71QF-XK5A-0H2C   yesterday 22:14    View Input Clip Files   [Edit] [Revoke]
 Work Edge      9PWE-RT4K-2NV8-JJ6Q   34 days ago        View Input  (remote)    [Edit] [Revoke]
 WoL helper     Q4XD-8B2M-ZT7C-1KHR   now                Wake                    [Edit] [Revoke]

 [ ] Allow remote pairing (password + second factor)       [ ] Require device keys
```

- **Edit** opens name and permission checkboxes; "Elevated input" and "Before sign-in" are greyed with a note while their machine switch is off, and changing "Before sign-in" asks for UAC.
- **Revoke** asks once, removes the entry, ends its session and syncs the relay list.
- **Pair device** shows the code, the QR and then the confirmation from flow A, inside this window.
- **Require device keys** turns off password-only connections (migration step 3). Greyed out until at least one device is paired, so the owner cannot lock themselves out.
- Last seen comes from the host's own records, not the relay. Connected devices are marked; revoking them disconnects.
- Design follows the BubblePlay product profile; the window is native, like the existing approval dialog.

## Migration and compatibility

1. **Phases 1-2, keys optional.** Hosts and clients announce `deviceKeys` in their capabilities. Old clients keep logging in with the password and second factor and reach the only registered host as today. A new client that has signed in this way is offered "Pair this browser / phone": that is flow B, done once.
2. **Upgrade order:** relay first (it routes both kinds), then the host, then clients. A new client against an old relay falls back to today's login; a new host against an old relay stays in single-room mode and accepts password sessions.
3. **Require device keys** (tray switch, default on for fresh installs once phase 3 ships, off for upgrades): the host refuses sessions without a valid handshake. Password login then only opens pairing.
4. `TOKEN` and `HOST_GATE_TOKEN` stay as the relay's admission secrets for hosts. A later cleanup can replace `HOST_GATE_TOKEN` with the host's key signature; not needed for any phase here.
5. `host.json` gains no secrets; the key stays in CNG. The trusted list is `%LOCALAPPDATA%\BubblePlay\devices.json`, DPAPI-wrapped for integrity, plus the pre-login copy above.

## Threats and mitigations

| Threat | Mitigation | Remaining |
| --- | --- | --- |
| Stolen password | Password alone no longer reaches the desktop once keys are required; it can only start remote pairing, which needs the second factor and is flagged in the tray. | Password + second factor still pair a device if remote pairing is on. |
| Malicious or compromised relay splicing a session | Handshake pinned to stored keys; WebRTC DTLS fingerprints signed; control messages authenticated with handshake keys (phase 3). | WSS media readable until phase 5. Browser clients run relay-served script. Flow B pairing trusts the relay. |
| Relay brute-forcing the pairing code | Comparison number confirmed on the PC; code expires after 5 minutes and one attempt. | None known for flow A. |
| ID squatting at the relay | ID must match the key hash and a signature; first admission needs `TOKEN`. | Denial of service by a holder of `TOKEN`. |
| Lost or stolen phone or laptop | Revoke in the tray. Android key is non-exportable. | Until revoked, the device connects with its permissions; an optional per-connect biometric on Android is open question 4. |
| Other same-user process on the PC adding a device | Trusted list is DPAPI-wrapped, so it is no stronger than the user account; pre-login list needs UAC. | Same residual risk as Admin mode today. |
| Key cloned from a PC | Non-exportable CNG key, TPM when present. | Software provider on PCs without a TPM. |
| Replay of a handshake or input | Fresh nonces and epoch in the transcript; per-message sequence numbers. | None known. |

Stays prohibited: accepting a device key without either the PC-side confirmation or the password and second factor; pairing before sign-in; password-only login; storing private keys outside the platform key stores (except the Wake-on-LAN helper's file); the relay storing names, permissions or anything that grants access; any inbound listener on the PC; and everything already prohibited in [unattended access](unattended-access.md#recommendation).

## Phased plan

| Phase | Content | Size | Unblocks |
| --- | --- | --- | --- |
| 0 | `fixtures/device-keys-contract.json`: ID derivation vectors, message shapes, transcript and signature test vectors, used by Rust, Go, JS and Kotlin. | S | everything |
| 1 | Keys and IDs on host (CNG), browser (WebCrypto), Android (Keystore); hosts register by ID; both relays route rooms by PC ID with single-host fallback; operator host-ID list. | M | multiple PCs, logon host and helper as separate IDs |
| 2 | Pairing flows A and B, handshake with DTLS binding, trusted list on the PC, viewer PC lists, tray Devices window, relay admission by published ID list. | L | **Wake-on-LAN** (helper pairs and verifies wake requests) and the device side of **unattended access** |
| 3 | Per-device permissions in the host gates, authenticated control channel, **Require device keys**, pre-login trusted list in ProgramData. | M | **Unattended access** step 2 (needs "Before sign-in" and the machine-scoped list) |
| 4 | Optional: passkey-per-browser, Android biometric per connect, key rotation UI, ntfy-carried comparison number for flow B. | S-M | — |
| 5 | Separate proposal: WSS media and file E2E with the phase 2 keys. | M-L | release-status future work |

Sizes: S under a week, M one to three weeks, L three to six weeks of focused work, each including tests against the shared fixture. Each phase ships behind capability flags and leaves old clients working.

## Open questions for the owner

1. Browser key: accept the non-extractable WebCrypto key as the default and passkeys as an optional later layer, instead of passkeys first?
2. Remote pairing (flow B): allowed at all? Recommended: yes, off by default, with view and input only and a tray flag.
3. For flow B, is the relay's trust acceptable, or should the PC send the comparison number through ntfy directly in phase 2 rather than phase 4?
4. Android: require a fingerprint or PIN for each connection, or only to open the app's PC list, or never (the default here)?
5. Should **Require device keys** default to on for upgrades too once phase 3 ships, or only for fresh installs?

## Decisions

None yet beyond the ones in the roadmap entry.
