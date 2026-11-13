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

mod ctl;
mod pairing;

pub use ctl::*;
pub use pairing::*;

const TRANSCRIPT_CONTEXT: &str = "bubbleplay-auth-v1";
const AUTH_HOST_CONTEXT: &[u8] = b"bubbleplay-auth-host-v1";
const AUTH_VIEWER_CONTEXT: &[u8] = b"bubbleplay-auth-viewer-v1";
const DTLS_CONTEXT: &[u8] = b"bubbleplay-dtls-v1";
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

#[cfg(test)]
mod tests;
