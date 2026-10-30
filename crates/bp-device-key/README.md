# bp-device-key

BubblePlay's thin layer over the shared device identity crate
[`bubblebuffer-device-key`](https://github.com/BubbleBuffer/bubblebuffer-identity) (git tag
v0.1.0, commit a539c7e). The shared crate owns device IDs, keys, the key file format
`bubblebuffer-device-key` and the relay proof encoding. This crate adds what is BubblePlay's
own: the relay label, the relay-origin canonical form, the reader for the old key file format,
the one-time conversion, and the phase 2 primitives. No Windows dependencies; it builds for
`wasm32-unknown-unknown`.

## What is in it

- `DeviceId`: re-exported from the shared crate. The 80-bit device ID (first 10 bytes of
  SHA-256 over the raw Ed25519 public key), printed as 16 Crockford base32 characters in four
  groups, `B7QK-2M9X-H4PD-R8TN`. IDs are unchanged by the move.
- `Role` (`Host`, `Viewer`) with the wire strings `host` and `viewer`, and `DeviceKeyError`
  (the shared crate's errors plus the phase 2 ones).
- The relay proof: `RELAY_LABEL` (`bubbleplay-relay-v1`, the label the live relay verifies),
  `relay_proof_encoding`, `relay_proof_hash`, `DeviceIdentity::sign_relay_proof` and
  `verify_relay_proof`. They call the shared crate with `RELAY_LABEL`; the bytes are the same
  as before.
- `relay_origin`: the relay-origin canonical form (lowercase scheme and host, default port
  dropped, `ws`->`http`, `wss`->`https`).
- `KeyFile` (feature `keyfile`): the `.bpkey` file in the shared format, plain or
  passphrase-protected (Argon2id m=64 MiB t=3 p=1, AES-256-GCM, AAD = JCS of the header).
- `legacy::LegacyKeyFile` (feature `keyfile`): the reader for the old `bubbleplay-device-key`
  format. Used only by migration and import; nothing writes the old format.
- `read_key_file` accepts either format; `convert_legacy` turns an old file into a shared-format
  file with the same key, label and created time (same passphrase, fresh salt and nonce) and
  checks the result opens to the same ID and public key.
- `phase2` (feature `phase2`): the device-keys phase 2 primitives. `Transcript` (the phase 0
  length-prefixed handshake transcript and its hash `H`), the auth-host, auth-finish and
  auth-bind signatures (sign and `verify_strict`; auth-bind keeps the phase 0 encoding and only
  accepts `sha-256` fingerprints with 32 uppercase hex pairs), `EphemeralSecret` and `CtlKeys`
  (X25519, all-zero shared secret refused, HKDF-SHA256 with salt `H`), `CtlSealer` and
  `CtlOpener` (AES-256-GCM, nonce = 4 zero bytes || seq u64 BE, AAD = kind; the opener takes
  only the exact next seq and is dead after any failure; seq stops at 2^53 - 1),
  `PairingCode` (10 Crockford characters, admit hash, pair key), `PairKey` (the viewer and pc
  MACs, constant-time check) and `Sas` (the six-digit comparison number). Checked against the
  fixture by `tests/phase2_contract.rs`. Checking that a JSON ctl plaintext's `type` equals its
  kind is left to the caller (the crate has no JSON parser).

Docs: `docs/architecture/device-keys.md`. Fixtures: `fixtures/shared/device-keys-contract.json`
(vendored from the shared crate, never edited here) and `fixtures/device-keys-contract.json`
(BubblePlay's relay label vectors, old-format key files, handshake and phase 2). Both are
checked by `tests/contract.rs` and by the Node test in `tools/tests/`.

## Features

| feature | default | adds |
| --- | --- | --- |
| (none) | yes | IDs, proof encoding/hash/sign/verify, origin canonicaliser, `DeviceIdentity` from an existing seed |
| `keygen` | no | `DeviceIdentity::generate`; with `phase2` also `EphemeralSecret::generate` and `PairingCode::generate` |
| `keyfile` | no | `KeyFile`, the old-format reader and the conversion (`argon2`, `aes-gcm`) |
| `phase2` | no | the `phase2` module (`x25519-dalek`, `hkdf`, `hmac`, `aes-gcm`) |

The shared crate has no features of its own: it always links `argon2`, `aes-gcm` and
`getrandom` 0.2. On `wasm32-unknown-unknown` this crate turns on getrandom's `js` backend so
the Cloudflare Worker relay builds; the relay only verifies proofs and never generates keys.
