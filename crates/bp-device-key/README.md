# bp-device-key

BubblePlay's device-key primitives, split out of `host/src/device_key.rs` so the Rust relay and
later the host can share one implementation. No Windows dependencies; it builds for
`wasm32-unknown-unknown`.

## What is in it

- `DeviceId`: the 80-bit device ID (first 10 bytes of SHA-256 over the raw Ed25519 public key),
  printed as 16 Crockford base32 characters in four groups, `B7QK-2M9X-H4PD-R8TN`. Parsing and
  case folding are ASCII only, so 'ß', 'ı', 'ſ', 'ﬁ' and 'é' are rejected, not folded.
- `Role` (`Host`, `Viewer`) with the wire strings `host` and `viewer`.
- The relay proof: `relay_proof_encoding`, `relay_proof_hash`, `DeviceIdentity::sign_relay_proof`
  and `verify_relay_proof` (checks the claimed ID against the key, recomputes the hash and
  verifies with `verify_strict`).
- `relay_origin`: the relay-origin canonical form (lowercase scheme and host, default port
  dropped, `ws`->`http`, `wss`->`https`).
- `KeyFile`: the `.bpkey` file, plain or passphrase-protected (Argon2id m=64 MiB t=3 p=1,
  AES-256-GCM, AAD = JCS of the header).
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

Docs: `docs/architecture/device-keys.md`. Contract fixture: `fixtures/device-keys-contract.json`,
checked by `tests/contract.rs` (the Go relay and the host check the same file).

## Features

| feature | default | adds |
| --- | --- | --- |
| (none) | yes | IDs, proof encoding/hash/sign/verify, origin canonicaliser, `DeviceIdentity` from an existing seed |
| `keygen` | no | `DeviceIdentity::generate` from OS randomness (`getrandom`); with `keyfile` also `KeyFile::create` and `rewrap` |
| `keyfile` | no | `KeyFile` parsing, unlocking, serialising and creating (`argon2`, `aes-gcm`) |
| `phase2` | no | the `phase2` module (`x25519-dalek`, `hkdf`, `hmac`, `aes-gcm`); with `keygen` also `EphemeralSecret::generate` and `PairingCode::generate` |

With default features the dependency tree has no `windows`, `tungstenite`, `argon2`, `aes-gcm`
or `getrandom`.

## Status

The host still has its own copy in `host/src/device_key.rs`, including the pieces this crate
deliberately does not carry (the tray handoff environment variable and the DPAPI-wrapped file
store). A follow-up switches the host onto this crate and deletes the duplicate. Nothing else in
this repository depends on the crate yet.
