//! Device keys phase 2: the authenticated handshake transcript, the auth-host, auth-finish and
//! auth-bind signatures, the X25519 + HKDF control-channel keys, the sequenced AES-256-GCM
//! control envelope (`ctl`), the pairing code with its relay admit hash, pair key and MACs, and
//! the six-digit comparison number (SAS).
//!
//! The encodings are fixed by `docs/architecture/device-keys.md` ("Encodings for implementers"
//! and the 2026-10-25 phase 2 paragraph) and by `fixtures/device-keys-contract.json`
//! (`handshake` and `phase2`). Where phase 0 defined an encoding, phase 2 keeps it.
//!
//! The API uses only std types, byte arrays and this crate's own types, so the Windows host,
//! the Windows client and the relay can share it, and it builds for `wasm32-unknown-unknown`.
//! Nothing here needs OS randomness unless the `keygen` feature is also on.
//!
//! Secrets (ephemeral X25519 secrets, the shared secret, control keys, the pairing code and the
//! pair key) are zeroized on drop and never printed by `Debug`.

use std::fmt;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use ed25519_dalek::{Signature, VerifyingKey};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
#[cfg(feature = "keygen")]
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::{DeviceId, DeviceIdentity, DeviceKeyError};

/// Crockford base32 without I, L, O and U: the pairing code alphabet (same as device IDs).
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

const TRANSCRIPT_CONTEXT: &str = "bubbleplay-auth-v1";
const AUTH_HOST_CONTEXT: &[u8] = b"bubbleplay-auth-host-v1";
const AUTH_VIEWER_CONTEXT: &[u8] = b"bubbleplay-auth-viewer-v1";
const DTLS_CONTEXT: &[u8] = b"bubbleplay-dtls-v1";
const CTL_INFO_VIEWER_TO_PC: &[u8] = b"bubbleplay-ctl-v1 viewer-to-pc";
const CTL_INFO_PC_TO_VIEWER: &[u8] = b"bubbleplay-ctl-v1 pc-to-viewer";
const PAIR_ADMIT_CONTEXT: &[u8] = b"bubbleplay-pair-admit-v1";
const PAIR_KEY_INFO: &[u8] = b"bubbleplay-pair-v1";
const PAIR_SAS_CONTEXT: &[u8] = b"bubbleplay-pair-sas-v1";
const FINGERPRINT_PREFIX: &str = "sha-256 ";

/// The highest control sequence number: 2^53 - 1, so JSON numbers stay exact in every client.
pub const MAX_CTL_SEQ: u64 = (1 << 53) - 1;

/// Number of characters in a pairing code (50 bits of Crockford base32).
pub const PAIRING_CODE_LEN: usize = 10;

/// The two ends of a phase 2 session. Wire labels: `viewer` and `pc`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Side {
    Viewer,
    Pc,
}

impl Side {
    /// The label used in the pairing MAC (`viewer` or `pc`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Pc => "pc",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Transcript

/// The phase 0 handshake transcript. Every field is written as a uint32 big-endian length
/// followed by its bytes, in this order: the context label `bubbleplay-auth-v1`, the relay
/// origin, the decimal session epoch string, the viewer's canonical hyphenated ID, the viewer's
/// raw Ed25519 public key (32), the PC's canonical ID, the PC's public key (32), the viewer
/// nonce (32), the PC nonce (32), the viewer's ephemeral X25519 public key (32) and the PC's
/// ephemeral public key (32). `H` is SHA-256 of that encoding.
///
/// `epoch` is used byte for byte as received; it is never parsed as a number (epochs may exceed
/// 2^53). Checking that each ID belongs to its public key is the caller's job, as for relay
/// proofs ([`DeviceId::from_public_key`]).
#[derive(Clone, Copy, Debug)]
pub struct Transcript<'a> {
    pub relay_origin: &'a str,
    pub epoch: &'a str,
    pub viewer_id: DeviceId,
    pub viewer_public: [u8; 32],
    pub pc_id: DeviceId,
    pub pc_public: [u8; 32],
    pub viewer_nonce: [u8; 32],
    pub pc_nonce: [u8; 32],
    pub viewer_ephemeral: [u8; 32],
    pub pc_ephemeral: [u8; 32],
}

impl Transcript<'_> {
    /// The length-prefixed transcript encoding.
    pub fn encode(&self) -> Vec<u8> {
        let viewer_id = self.viewer_id.to_string();
        let pc_id = self.pc_id.to_string();
        let fields: [&[u8]; 11] = [
            TRANSCRIPT_CONTEXT.as_bytes(),
            self.relay_origin.as_bytes(),
            self.epoch.as_bytes(),
            viewer_id.as_bytes(),
            &self.viewer_public,
            pc_id.as_bytes(),
            &self.pc_public,
            &self.viewer_nonce,
            &self.pc_nonce,
            &self.viewer_ephemeral,
            &self.pc_ephemeral,
        ];
        let mut out = Vec::new();
        for field in fields {
            out.extend_from_slice(&(field.len() as u32).to_be_bytes());
            out.extend_from_slice(field);
        }
        out
    }

    /// `H` = SHA-256 of [`Transcript::encode`].
    pub fn hash(&self) -> [u8; 32] {
        Sha256::digest(self.encode()).into()
    }
}

// ---------------------------------------------------------------------------------------------
// Handshake signatures

/// The bytes the PC signs in `auth-host`: `"bubbleplay-auth-host-v1" || H`.
pub fn auth_host_signed_bytes(h: &[u8; 32]) -> Vec<u8> {
    [AUTH_HOST_CONTEXT, h.as_slice()].concat()
}

/// The bytes the viewer signs in `auth-finish`:
/// `"bubbleplay-auth-viewer-v1" || H || the PC's 64-byte auth-host signature`.
pub fn auth_finish_signed_bytes(h: &[u8; 32], pc_signature: &[u8; 64]) -> Vec<u8> {
    [AUTH_VIEWER_CONTEXT, h.as_slice(), pc_signature.as_slice()].concat()
}

/// The bytes each side signs in `auth-bind` (phase 0 encoding):
/// `"bubbleplay-dtls-v1" || H || local fingerprint || "\n" || remote fingerprint`, where both
/// fingerprints are the UTF-8 `a=fingerprint` values from the signer's point of view
/// (`sha-256 AB:CD:...`, 32 uppercase hex pairs). Any other fingerprint form is refused: run the
/// SDP through [`sdp_sha256_fingerprint`] (or a single value through [`normalize_fingerprint`])
/// first, on both the signing and the verifying side.
pub fn auth_bind_signed_bytes(
    h: &[u8; 32],
    local_fingerprint: &str,
    remote_fingerprint: &str,
) -> Result<Vec<u8>, DeviceKeyError> {
    check_fingerprint(local_fingerprint)?;
    check_fingerprint(remote_fingerprint)?;
    Ok([
        DTLS_CONTEXT,
        h.as_slice(),
        local_fingerprint.as_bytes(),
        b"\n",
        remote_fingerprint.as_bytes(),
    ]
    .concat())
}

/// Signs `auth-host` (PC side): Ed25519 over [`auth_host_signed_bytes`].
pub fn sign_auth_host(identity: &DeviceIdentity, h: &[u8; 32]) -> [u8; 64] {
    identity.sign_bytes(&auth_host_signed_bytes(h))
}

/// Verifies `auth-host` with `verify_strict` under the PC's raw public key.
pub fn verify_auth_host(
    pc_public: &[u8],
    h: &[u8; 32],
    signature: &[u8],
) -> Result<(), DeviceKeyError> {
    verify_strict(pc_public, &auth_host_signed_bytes(h), signature)
}

/// Signs `auth-finish` (viewer side): Ed25519 over [`auth_finish_signed_bytes`].
pub fn sign_auth_finish(
    identity: &DeviceIdentity,
    h: &[u8; 32],
    pc_signature: &[u8; 64],
) -> [u8; 64] {
    identity.sign_bytes(&auth_finish_signed_bytes(h, pc_signature))
}

/// Verifies `auth-finish` with `verify_strict` under the viewer's raw public key.
/// `pc_signature` is the auth-host signature the PC sent.
pub fn verify_auth_finish(
    viewer_public: &[u8],
    h: &[u8; 32],
    pc_signature: &[u8; 64],
    signature: &[u8],
) -> Result<(), DeviceKeyError> {
    verify_strict(
        viewer_public,
        &auth_finish_signed_bytes(h, pc_signature),
        signature,
    )
}

/// Signs `auth-bind` with the signer's own local and remote DTLS fingerprints.
pub fn sign_auth_bind(
    identity: &DeviceIdentity,
    h: &[u8; 32],
    local_fingerprint: &str,
    remote_fingerprint: &str,
) -> Result<[u8; 64], DeviceKeyError> {
    let signed = auth_bind_signed_bytes(h, local_fingerprint, remote_fingerprint)?;
    Ok(identity.sign_bytes(&signed))
}

/// Verifies a peer's `auth-bind` with `verify_strict`. The fingerprints are in the signer's
/// order: `signer_local` is the peer's own fingerprint (our remote), `signer_remote` is ours.
pub fn verify_auth_bind(
    signer_public: &[u8],
    h: &[u8; 32],
    signer_local_fingerprint: &str,
    signer_remote_fingerprint: &str,
    signature: &[u8],
) -> Result<(), DeviceKeyError> {
    let signed = auth_bind_signed_bytes(h, signer_local_fingerprint, signer_remote_fingerprint)?;
    verify_strict(signer_public, &signed, signature)
}

/// Checks an `a=fingerprint` value: `sha-256 ` followed by 32 uppercase hex pairs joined by `:`.
pub fn check_fingerprint(value: &str) -> Result<(), DeviceKeyError> {
    let rest = value
        .strip_prefix(FINGERPRINT_PREFIX)
        .ok_or(DeviceKeyError::InvalidFingerprint)?
        .as_bytes();
    if rest.len() != 32 * 3 - 1 {
        return Err(DeviceKeyError::InvalidFingerprint);
    }
    for (index, &byte) in rest.iter().enumerate() {
        let ok = if index % 3 == 2 {
            byte == b':'
        } else {
            byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte)
        };
        if !ok {
            return Err(DeviceKeyError::InvalidFingerprint);
        }
    }
    Ok(())
}

/// Normalises one `a=fingerprint` value to the form auth-bind signs: algorithm lowercase, exactly
/// one space, hex uppercase in colon-separated pairs (`sha-256 A0:A1:...`). Only sha-256 is
/// accepted; anything else, or a hash that is not 32 colon-separated hex pairs, is
/// [`DeviceKeyError::InvalidFingerprint`].
pub fn normalize_fingerprint(value: &str) -> Result<String, DeviceKeyError> {
    let (algorithm, hash) = value
        .trim()
        .split_once([' ', '\t'])
        .ok_or(DeviceKeyError::InvalidFingerprint)?;
    if !algorithm.eq_ignore_ascii_case("sha-256") {
        return Err(DeviceKeyError::InvalidFingerprint);
    }
    let normalized = format!(
        "{FINGERPRINT_PREFIX}{}",
        hash.trim_start_matches([' ', '\t']).to_ascii_uppercase()
    );
    check_fingerprint(&normalized)?;
    Ok(normalized)
}

/// The normalised sha-256 DTLS fingerprint of an SDP (see [`normalize_fingerprint`]), read from
/// its `a=fingerprint:` lines. Lines for other algorithms are skipped. Several sha-256 lines
/// (session and media level) must carry the same fingerprint. No sha-256 line, a malformed
/// sha-256 line or two different sha-256 fingerprints is [`DeviceKeyError::InvalidFingerprint`].
pub fn sdp_sha256_fingerprint(sdp: &str) -> Result<String, DeviceKeyError> {
    let mut found: Option<String> = None;
    for line in sdp.lines() {
        let Some(value) = line.trim().strip_prefix("a=fingerprint:") else {
            continue;
        };
        let is_sha256 = value
            .split_whitespace()
            .next()
            .is_some_and(|algorithm| algorithm.eq_ignore_ascii_case("sha-256"));
        if !is_sha256 {
            continue;
        }
        let normalized = normalize_fingerprint(value)?;
        match &found {
            Some(existing) if *existing != normalized => {
                return Err(DeviceKeyError::InvalidFingerprint);
            }
            Some(_) => {}
            None => found = Some(normalized),
        }
    }
    found.ok_or(DeviceKeyError::InvalidFingerprint)
}

fn verify_strict(public: &[u8], message: &[u8], signature: &[u8]) -> Result<(), DeviceKeyError> {
    let public: &[u8; 32] = public
        .try_into()
        .map_err(|_| DeviceKeyError::InvalidLength("public"))?;
    let signature: &[u8; 64] = signature
        .try_into()
        .map_err(|_| DeviceKeyError::InvalidLength("signature"))?;
    let verifying =
        VerifyingKey::from_bytes(public).map_err(|_| DeviceKeyError::InvalidEncoding("public"))?;
    verifying
        .verify_strict(message, &Signature::from_bytes(signature))
        .map_err(|_| DeviceKeyError::InvalidHandshakeSignature)
}

// ---------------------------------------------------------------------------------------------
// X25519 and control keys

/// An ephemeral X25519 secret for one handshake. Zeroized on drop; consumed by
/// [`EphemeralSecret::diffie_hellman`] so it is used once.
pub struct EphemeralSecret(x25519_dalek::StaticSecret);

impl EphemeralSecret {
    /// Wraps 32 caller-supplied random bytes (clamping happens inside X25519).
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        let bytes = Zeroizing::new(bytes);
        Self(x25519_dalek::StaticSecret::from(*bytes))
    }

    /// Generates a secret from OS randomness.
    #[cfg(feature = "keygen")]
    pub fn generate() -> Result<Self, DeviceKeyError> {
        let mut bytes = Zeroizing::new([0u8; 32]);
        OsRng
            .try_fill_bytes(&mut bytes[..])
            .map_err(|_| DeviceKeyError::Entropy)?;
        Ok(Self::from_bytes(*bytes))
    }

    /// The raw 32-byte X25519 public key that goes into `auth-hello` / `auth-host`.
    pub fn public_key(&self) -> [u8; 32] {
        x25519_dalek::PublicKey::from(&self.0).to_bytes()
    }

    /// `X25519(own secret, peer public)`. An all-zero result (a low-order peer key) is refused.
    pub fn diffie_hellman(self, peer_public: &[u8; 32]) -> Result<SharedSecret, DeviceKeyError> {
        let shared = self
            .0
            .diffie_hellman(&x25519_dalek::PublicKey::from(*peer_public));
        if !shared.was_contributory() {
            return Err(DeviceKeyError::WeakSharedSecret);
        }
        Ok(SharedSecret(Zeroizing::new(shared.to_bytes())))
    }
}

impl fmt::Debug for EphemeralSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EphemeralSecret")
            .field("public", &self.public_key())
            .finish_non_exhaustive()
    }
}

/// The X25519 shared secret. Zeroized on drop.
pub struct SharedSecret(Zeroizing<[u8; 32]>);

impl SharedSecret {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for SharedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SharedSecret(..)")
    }
}

/// The two control-channel keys:
/// `k_v2p = HKDF-SHA256(ikm = shared, salt = H, info = "bubbleplay-ctl-v1 viewer-to-pc", 32)`
/// and `k_p2v` with info `"bubbleplay-ctl-v1 pc-to-viewer"`. Zeroized on drop.
pub struct CtlKeys {
    viewer_to_pc: Zeroizing<[u8; 32]>,
    pc_to_viewer: Zeroizing<[u8; 32]>,
}

impl CtlKeys {
    pub fn derive(shared: &SharedSecret, h: &[u8; 32]) -> Self {
        let hkdf = Hkdf::<Sha256>::new(Some(&h[..]), shared.as_bytes());
        let mut viewer_to_pc = Zeroizing::new([0u8; 32]);
        let mut pc_to_viewer = Zeroizing::new([0u8; 32]);
        hkdf.expand(CTL_INFO_VIEWER_TO_PC, &mut viewer_to_pc[..])
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        hkdf.expand(CTL_INFO_PC_TO_VIEWER, &mut pc_to_viewer[..])
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        Self {
            viewer_to_pc,
            pc_to_viewer,
        }
    }

    pub fn viewer_to_pc(&self) -> &[u8; 32] {
        &self.viewer_to_pc
    }

    pub fn pc_to_viewer(&self) -> &[u8; 32] {
        &self.pc_to_viewer
    }

    /// The sealer and opener for `side`: the viewer seals with `k_v2p` and opens with `k_p2v`,
    /// the PC the other way round. Both counters start at 0.
    pub fn channel(&self, side: Side) -> (CtlSealer, CtlOpener) {
        match side {
            Side::Viewer => (
                CtlSealer::new(&self.viewer_to_pc),
                CtlOpener::new(&self.pc_to_viewer),
            ),
            Side::Pc => (
                CtlSealer::new(&self.pc_to_viewer),
                CtlOpener::new(&self.viewer_to_pc),
            ),
        }
    }
}

impl fmt::Debug for CtlKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CtlKeys(..)")
    }
}

// ---------------------------------------------------------------------------------------------
// ctl envelope

/// Checks a ctl `kind`: 1 to 32 characters from `[a-z0-9-]`.
pub fn check_ctl_kind(kind: &str) -> Result<(), DeviceKeyError> {
    let ok = (1..=32).contains(&kind.len())
        && kind
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if ok {
        Ok(())
    } else {
        Err(DeviceKeyError::InvalidCtlKind)
    }
}

/// The 12-byte AES-GCM nonce for a control sequence number: 4 zero bytes || seq as u64 BE.
pub fn ctl_nonce(seq: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&seq.to_be_bytes());
    nonce
}

/// The sending half of a control channel: AES-256-GCM, nonce [`ctl_nonce`]`(seq)`, AAD = the
/// UTF-8 kind, output `ciphertext || 16-byte tag`. Its own counter starts at 0 and stops after
/// [`MAX_CTL_SEQ`].
pub struct CtlSealer {
    cipher: Aes256Gcm,
    next_seq: u64,
    dead: bool,
}

impl CtlSealer {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: Aes256Gcm::new(key.into()),
            next_seq: 0,
            dead: false,
        }
    }

    /// The sequence number the next [`CtlSealer::seal`] will use.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Seals one message and returns `(seq, data)`. An invalid kind is refused without using a
    /// sequence number; running past [`MAX_CTL_SEQ`] ends the sealer.
    pub fn seal(&mut self, kind: &str, plaintext: &[u8]) -> Result<(u64, Vec<u8>), DeviceKeyError> {
        if self.dead {
            return Err(DeviceKeyError::CtlChannelClosed);
        }
        check_ctl_kind(kind)?;
        let seq = self.next_seq;
        if seq > MAX_CTL_SEQ {
            self.dead = true;
            return Err(DeviceKeyError::CtlSequenceExhausted);
        }
        let nonce = ctl_nonce(seq);
        let data = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: kind.as_bytes(),
                },
            )
            .map_err(|_| {
                self.dead = true;
                DeviceKeyError::Internal("ctl seal")
            })?;
        self.next_seq = seq + 1;
        Ok((seq, data))
    }
}

impl fmt::Debug for CtlSealer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CtlSealer")
            .field("next_seq", &self.next_seq)
            .field("dead", &self.dead)
            .finish_non_exhaustive()
    }
}

/// The receiving half of a control channel. It accepts only the exact next sequence number
/// (starting at 0). Any gap, repeat, sequence past [`MAX_CTL_SEQ`], kind outside
/// `[a-z0-9-]{1,32}` or tag failure is an error and ends the channel: every later call fails
/// with [`DeviceKeyError::CtlChannelClosed`].
///
/// For JSON payloads the receiver must also check that the plaintext's `type` equals `kind`.
/// That needs a JSON parser, which this crate does not carry, so it is left to the caller.
pub struct CtlOpener {
    cipher: Aes256Gcm,
    expected_seq: u64,
    dead: bool,
}

impl CtlOpener {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: Aes256Gcm::new(key.into()),
            expected_seq: 0,
            dead: false,
        }
    }

    /// The only sequence number the next [`CtlOpener::open`] accepts.
    pub fn expected_seq(&self) -> u64 {
        self.expected_seq
    }

    /// Whether an earlier failure ended the channel.
    pub fn is_dead(&self) -> bool {
        self.dead
    }

    /// Opens one message. On success the expected sequence number advances by one.
    pub fn open(&mut self, kind: &str, seq: u64, data: &[u8]) -> Result<Vec<u8>, DeviceKeyError> {
        if self.dead {
            return Err(DeviceKeyError::CtlChannelClosed);
        }
        let result = self.open_inner(kind, seq, data);
        match result {
            Ok(_) => self.expected_seq += 1,
            Err(_) => self.dead = true,
        }
        result
    }

    fn open_inner(&self, kind: &str, seq: u64, data: &[u8]) -> Result<Vec<u8>, DeviceKeyError> {
        check_ctl_kind(kind)?;
        if self.expected_seq > MAX_CTL_SEQ {
            return Err(DeviceKeyError::CtlSequenceExhausted);
        }
        if seq != self.expected_seq {
            return Err(DeviceKeyError::CtlSequence);
        }
        let nonce = ctl_nonce(seq);
        self.cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: data,
                    aad: kind.as_bytes(),
                },
            )
            .map_err(|_| DeviceKeyError::CtlAuthenticationFailed)
    }
}

impl fmt::Debug for CtlOpener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CtlOpener")
            .field("expected_seq", &self.expected_seq)
            .field("dead", &self.dead)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------------------
// Pairing

/// A pairing code: 10 Crockford base32 characters, stored in canonical form (uppercase, no
/// separators) and zeroized on drop. Shown as `XXXXX-XXXXX`.
pub struct PairingCode(Zeroizing<[u8; PAIRING_CODE_LEN]>);

impl PairingCode {
    /// Normalises like device IDs (ASCII uppercase, O reads as 0, I and L as 1, `-` and
    /// whitespace dropped) and refuses anything that is not then exactly 10 Crockford
    /// characters. Non-ASCII letters are never folded.
    pub fn parse(input: &str) -> Result<Self, DeviceKeyError> {
        let mut code = Zeroizing::new([0u8; PAIRING_CODE_LEN]);
        let mut count = 0usize;
        for ch in input.chars() {
            if ch == '-' || ch.is_whitespace() {
                continue;
            }
            let symbol = match ch.to_ascii_uppercase() {
                'O' => '0',
                'I' | 'L' => '1',
                other => other,
            };
            if !symbol.is_ascii() || !CROCKFORD.contains(&(symbol as u8)) {
                return Err(DeviceKeyError::InvalidPairingCode);
            }
            if count == PAIRING_CODE_LEN {
                return Err(DeviceKeyError::InvalidPairingCode);
            }
            code[count] = symbol as u8;
            count += 1;
        }
        if count != PAIRING_CODE_LEN {
            return Err(DeviceKeyError::InvalidPairingCode);
        }
        Ok(Self(code))
    }

    /// Builds a code from 10 caller-supplied random bytes: the low 5 bits of each byte pick one
    /// character, so uniform bytes give a uniform code (no modulo bias).
    pub fn from_random_bytes(bytes: &[u8; PAIRING_CODE_LEN]) -> Self {
        let mut code = Zeroizing::new([0u8; PAIRING_CODE_LEN]);
        for (out, byte) in code.iter_mut().zip(bytes) {
            *out = CROCKFORD[usize::from(byte & 0x1f)];
        }
        Self(code)
    }

    /// Generates a code from OS randomness.
    #[cfg(feature = "keygen")]
    pub fn generate() -> Result<Self, DeviceKeyError> {
        let mut bytes = Zeroizing::new([0u8; PAIRING_CODE_LEN]);
        OsRng
            .try_fill_bytes(&mut bytes[..])
            .map_err(|_| DeviceKeyError::Entropy)?;
        Ok(Self::from_random_bytes(&bytes))
    }

    /// The canonical form: 10 characters, no separator.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0[..]).expect("pairing codes are ASCII")
    }

    /// The display form `XXXXX-XXXXX`.
    pub fn display(&self) -> Zeroizing<String> {
        let canonical = self.as_str();
        Zeroizing::new(format!("{}-{}", &canonical[..5], &canonical[5..]))
    }

    /// The relay admit hash:
    /// `SHA-256("bubbleplay-pair-admit-v1" || canonical hyphenated PC ID || canonical code)`,
    /// raw concatenation.
    pub fn admit_hash(&self, pc_id: &DeviceId) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(PAIR_ADMIT_CONTEXT);
        hasher.update(pc_id.to_string().as_bytes());
        hasher.update(&self.0[..]);
        hasher.finalize().into()
    }

    /// The pair key `K = HKDF-SHA256(ikm = canonical code ASCII, salt = empty,
    /// info = "bubbleplay-pair-v1", 32)`.
    pub fn pair_key(&self) -> PairKey {
        let hkdf = Hkdf::<Sha256>::new(Some(&[][..]), &self.0[..]);
        let mut key = Zeroizing::new([0u8; 32]);
        hkdf.expand(PAIR_KEY_INFO, &mut key[..])
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        PairKey(key)
    }
}

impl fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingCode(..)")
    }
}

/// The pair key derived from a pairing code. Zeroized on drop.
pub struct PairKey(Zeroizing<[u8; 32]>);

impl PairKey {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// `HMAC-SHA256(K, side label || H)` with the label `viewer` or `pc`.
    pub fn mac(&self, side: Side, h: &[u8; 32]) -> [u8; 32] {
        self.hmac(side, h).finalize().into_bytes().into()
    }

    /// Checks a peer's pair MAC in constant time. Wrong lengths fail as well.
    pub fn verify_mac(&self, side: Side, h: &[u8; 32], mac: &[u8]) -> Result<(), DeviceKeyError> {
        self.hmac(side, h)
            .verify_slice(mac)
            .map_err(|_| DeviceKeyError::PairingMacMismatch)
    }

    fn hmac(&self, side: Side, h: &[u8; 32]) -> Hmac<Sha256> {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.0[..])
            .expect("HMAC takes keys of any length");
        mac.update(side.as_str().as_bytes());
        mac.update(h);
        mac
    }
}

impl fmt::Debug for PairKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairKey(..)")
    }
}

/// The comparison number shown on both screens while pairing: the first 4 bytes of
/// `SHA-256("bubbleplay-pair-sas-v1" || H)` as a u32 big-endian, mod 1 000 000.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sas(u32);

impl Sas {
    pub fn from_transcript_hash(h: &[u8; 32]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(PAIR_SAS_CONTEXT);
        hasher.update(h);
        let digest = hasher.finalize();
        Self(u32::from_be_bytes([
            digest[0], digest[1], digest[2], digest[3],
        ]))
    }

    /// The raw u32 before the modulo.
    pub fn raw(self) -> u32 {
        self.0
    }

    /// Six digits, zero padded (`"830687"`).
    pub fn number(self) -> String {
        format!("{:06}", self.0 % 1_000_000)
    }

    /// The display form with a space after three digits (`"830 687"`).
    pub fn display(self) -> String {
        let number = self.number();
        format!("{} {}", &number[..3], &number[3..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealer_and_opener() -> (CtlSealer, CtlOpener) {
        let key = [7u8; 32];
        (CtlSealer::new(&key), CtlOpener::new(&key))
    }

    #[test]
    fn channel_is_dead_after_any_failure() {
        // Tag failure.
        let (mut sealer, mut opener) = sealer_and_opener();
        let (seq, mut data) = sealer.seal("input", b"abc").expect("seal");
        data[0] ^= 1;
        assert!(matches!(
            opener.open("input", seq, &data),
            Err(DeviceKeyError::CtlAuthenticationFailed)
        ));
        assert!(opener.is_dead());
        data[0] ^= 1;
        assert!(matches!(
            opener.open("input", seq, &data),
            Err(DeviceKeyError::CtlChannelClosed)
        ));

        // Sequence gap, then the correct message is still refused.
        let (mut sealer, mut opener) = sealer_and_opener();
        let (_, first) = sealer.seal("input", b"a").expect("seal");
        let (_, second) = sealer.seal("input", b"b").expect("seal");
        assert!(matches!(
            opener.open("input", 1, &second),
            Err(DeviceKeyError::CtlSequence)
        ));
        assert!(matches!(
            opener.open("input", 0, &first),
            Err(DeviceKeyError::CtlChannelClosed)
        ));

        // Invalid kind.
        let (mut sealer, mut opener) = sealer_and_opener();
        let (_, data) = sealer.seal("input", b"a").expect("seal");
        let long = "a".repeat(33);
        for bad in ["", "Input", "in put", long.as_str(), "ïnput"] {
            assert!(matches!(
                sealer.seal(bad, b"a"),
                Err(DeviceKeyError::InvalidCtlKind)
            ));
        }
        assert!(matches!(
            opener.open("Input", 0, &data),
            Err(DeviceKeyError::InvalidCtlKind)
        ));
        assert!(matches!(
            opener.open("input", 0, &data),
            Err(DeviceKeyError::CtlChannelClosed)
        ));
    }

    #[test]
    fn sequence_overflow_is_refused() {
        let (mut sealer, mut opener) = sealer_and_opener();
        sealer.next_seq = MAX_CTL_SEQ;
        opener.expected_seq = MAX_CTL_SEQ;
        let (seq, data) = sealer
            .seal("input", b"last")
            .expect("seq 2^53-1 is allowed");
        assert_eq!(seq, MAX_CTL_SEQ);
        assert_eq!(opener.open("input", seq, &data).expect("open"), b"last");
        assert!(matches!(
            sealer.seal("input", b"over"),
            Err(DeviceKeyError::CtlSequenceExhausted)
        ));
        assert!(matches!(
            sealer.seal("input", b"over"),
            Err(DeviceKeyError::CtlChannelClosed)
        ));
        assert!(matches!(
            opener.open("input", MAX_CTL_SEQ + 1, &data),
            Err(DeviceKeyError::CtlSequenceExhausted)
        ));
        assert!(opener.is_dead());

        // A sequence number beyond the limit on a fresh opener is a gap and ends it too.
        let (_, mut fresh) = sealer_and_opener();
        assert!(fresh.open("input", u64::MAX, &data).is_err());
        assert!(fresh.is_dead());
    }

    #[test]
    fn generated_codes_are_canonical() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut seen_all = [false; 32];
        for round in 0..2000u32 {
            let mut bytes = [0u8; PAIRING_CODE_LEN];
            for byte in &mut bytes {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = (state >> 24) as u8;
            }
            if round == 0 {
                bytes = [0xff; PAIRING_CODE_LEN];
            }
            let code = PairingCode::from_random_bytes(&bytes);
            let text = code.as_str().to_owned();
            assert_eq!(text.len(), PAIRING_CODE_LEN);
            for byte in text.bytes() {
                let index = CROCKFORD.iter().position(|&c| c == byte).expect("alphabet");
                seen_all[index] = true;
            }
            let reparsed = PairingCode::parse(&text).expect("canonical parses");
            assert_eq!(reparsed.as_str(), text);
            let display = code.display();
            assert_eq!(display.len(), 11);
            assert_eq!(&display[5..6], "-");
            assert_eq!(
                PairingCode::parse(&display)
                    .expect("display parses")
                    .as_str(),
                text
            );
        }
        assert!(seen_all.iter().all(|&seen| seen), "every symbol reachable");
        // Each low-5-bit value maps to its own symbol, so the mapping is uniform.
        let all: [u8; PAIRING_CODE_LEN] = [0, 1, 2, 3, 4, 27, 28, 29, 30, 31];
        assert_eq!(PairingCode::from_random_bytes(&all).as_str(), "01234VWXYZ");
    }

    #[cfg(feature = "keygen")]
    #[test]
    fn os_generated_codes_are_canonical() {
        for _ in 0..64 {
            let code = PairingCode::generate().expect("entropy");
            assert_eq!(
                PairingCode::parse(code.as_str()).expect("parses").as_str(),
                code.as_str()
            );
        }
    }

    #[test]
    fn pairing_code_rejects_bad_input() {
        for input in [
            "",
            "70Q1K9MXR",
            "70Q1K9MXR12",
            "70Q1K9MXRU",
            "70Q1K9MXR!",
            "70Q1K9MXRé",
            "70Q1K_9MXR1",
        ] {
            assert!(
                matches!(
                    PairingCode::parse(input),
                    Err(DeviceKeyError::InvalidPairingCode)
                ),
                "accepted {input:?}"
            );
        }
    }

    #[test]
    fn fingerprints_must_be_canonical_sha256() {
        let good = format!("sha-256 {}", ["AB"; 32].join(":"));
        assert!(check_fingerprint(&good).is_ok());
        for bad in [
            format!("sha-256 {}", ["ab"; 32].join(":")),
            format!("SHA-256 {}", ["AB"; 32].join(":")),
            format!("sha-1 {}", ["AB"; 20].join(":")),
            format!("sha-256 {}", ["AB"; 31].join(":")),
            format!("sha-256 {}:", ["AB"; 32].join(":")),
            format!("sha-256  {}", ["AB"; 32].join(":")),
            format!("sha-256 {}", ["AB"; 32].join("-")),
            format!("sha-256 {}", ["AG"; 32].join(":")),
        ] {
            assert!(
                matches!(
                    check_fingerprint(&bad),
                    Err(DeviceKeyError::InvalidFingerprint)
                ),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn fingerprints_normalise_to_the_signed_form() {
        let canonical = format!("sha-256 {}", ["AB"; 32].join(":"));
        for input in [
            canonical.clone(),
            format!("sha-256 {}", ["ab"; 32].join(":")),
            format!("SHA-256 {}", ["aB"; 32].join(":")),
            format!("  Sha-256 \t  {}  ", ["Ab"; 32].join(":")),
        ] {
            assert_eq!(
                normalize_fingerprint(&input).expect("normalises"),
                canonical,
                "{input:?}"
            );
        }
        for bad in [
            String::new(),
            "sha-256".to_owned(),
            format!("sha-1 {}", ["ab"; 20].join(":")),
            format!("sha-256 {}", ["ab"; 31].join(":")),
            format!("sha-256 {}", ["ab"; 32].join("")),
            format!("sha-256 {}", ["ag"; 32].join(":")),
        ] {
            assert!(
                matches!(
                    normalize_fingerprint(&bad),
                    Err(DeviceKeyError::InvalidFingerprint)
                ),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn sdp_fingerprint_picks_sha256() {
        let sha1 = format!("sha-1 {}", ["0f"; 20].join(":"));
        let sha256 = format!("sha-256 {}", ["cd"; 32].join(":"));
        let canonical = format!("sha-256 {}", ["CD"; 32].join(":"));
        // sha-1 first, then sha-256 at session and media level (CRLF and LF lines).
        let sdp = format!(
            "v=0\r\na=fingerprint:{sha1}\r\na=fingerprint:{sha256}\r\nm=application 9 UDP/DTLS/SCTP\na=fingerprint:{canonical}\n"
        );
        assert_eq!(sdp_sha256_fingerprint(&sdp).expect("sha-256"), canonical);
        // sha-256 first, sha-1 after.
        let sdp = format!("a=fingerprint:{sha256}\na=fingerprint:{sha1}\n");
        assert_eq!(sdp_sha256_fingerprint(&sdp).expect("sha-256"), canonical);
    }

    #[test]
    fn sdp_without_a_usable_sha256_fingerprint_is_an_error() {
        let sha1 = format!("sha-1 {}", ["0F"; 20].join(":"));
        let one = format!("sha-256 {}", ["AB"; 32].join(":"));
        let other = format!("sha-256 {}", ["CD"; 32].join(":"));
        for sdp in [
            String::new(),
            "v=0\r\na=setup:actpass\r\n".to_owned(),
            format!("v=0\r\na=fingerprint:{sha1}\r\n"),
            format!("a=fingerprint:sha-256 {}\n", ["AB"; 31].join(":")),
            format!("a=fingerprint:{one}\na=fingerprint:{other}\n"),
        ] {
            assert!(
                matches!(
                    sdp_sha256_fingerprint(&sdp),
                    Err(DeviceKeyError::InvalidFingerprint)
                ),
                "accepted {sdp:?}"
            );
        }
    }

    #[test]
    fn low_order_peer_key_is_refused() {
        let secret = EphemeralSecret::from_bytes([9u8; 32]);
        assert!(matches!(
            secret.diffie_hellman(&[0u8; 32]),
            Err(DeviceKeyError::WeakSharedSecret)
        ));
    }

    #[test]
    fn debug_output_hides_secrets() {
        let code = PairingCode::parse("70Q1K9MXR1").expect("code");
        let key = code.pair_key();
        let text = format!("{code:?} {key:?}");
        assert!(!text.contains("70Q1K"));
        assert_eq!(text, "PairingCode(..) PairKey(..)");
    }
}
