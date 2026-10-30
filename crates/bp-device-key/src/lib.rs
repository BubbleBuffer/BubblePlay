//! BubblePlay's device-key crate: a thin layer over the shared `bubblebuffer-device-key` crate
//! (device IDs, Ed25519 keys, the `bubblebuffer-device-key` key file and relay proofs) plus what
//! stays BubblePlay's own: the relay label `bubbleplay-relay-v1`, the relay-origin canonical form,
//! the phase 2 handshake and, behind `keyfile`, the key file wrapper.
//!
//! Features:
//!
//! - default: device IDs, relay proof encoding, hashing, signing with an existing key and
//!   verification, and the relay-origin canonicaliser.
//! - `keygen`: `DeviceIdentity::generate` and, with `phase2`, the phase 2 generators.
//! - `keyfile`: [`KeyFile`] (the shared format).
//! - `phase2`: the [`phase2`] module: handshake transcript and signatures, X25519 + HKDF
//!   control keys, the sequenced AES-256-GCM `ctl` envelope, pairing codes, MACs and the
//!   comparison number (pulls `x25519-dalek`, `hkdf`, `hmac` and `aes-gcm`).

#![forbid(unsafe_code)]

use std::fmt;

mod proof;

#[cfg(feature = "keyfile")]
mod keyfile;

#[cfg(feature = "phase2")]
pub mod phase2;

pub use bubblebuffer_device_key::DeviceId;
pub use proof::{
    relay_origin, relay_proof_encoding, relay_proof_hash, verify_relay_proof, DeviceIdentity, Role,
    RELAY_LABEL,
};

#[cfg(feature = "keyfile")]
pub use keyfile::KeyFile;

/// Failures are distinct: `AuthenticationFailed` means the AEAD tag did not verify (wrong
/// passphrase or altered authenticated metadata); the other variants are validation failures
/// found before or after decryption.
#[derive(Debug)]
pub enum DeviceKeyError {
    InvalidId,
    /// The Ed25519 key is malformed, noncanonical or outside the prime-order subgroup.
    InvalidPublicKey,
    /// A proof role other than the wire strings `host` and `viewer`.
    UnknownRole,
    /// JSON syntax, duplicate keys, unsupported JSON value kinds or nesting depth.
    Malformed(&'static str),
    MissingField(&'static str),
    UnknownField(String),
    /// Wrong JSON type, or a value outside what the format allows.
    InvalidField(&'static str),
    InvalidEncoding(&'static str),
    InvalidLength(&'static str),
    UnsupportedFormat,
    UnsupportedVersion,
    UnsupportedType,
    UnsupportedKdf,
    UnsupportedAead,
    /// `kdf` and `aead` must both be null (plain) or both be objects (protected).
    IncompleteProtection,
    IdMismatch,
    PublicKeyMismatch,
    PassphraseRequired,
    PassphraseNotAccepted,
    AuthenticationFailed,
    Entropy,
    /// The proof hash did not verify under the supplied key, or the fields do not match the
    /// signature that was made over them.
    InvalidSignature,
    Internal(&'static str),
    /// Phase 2: a handshake signature (auth-host, auth-finish or auth-bind) did not verify.
    InvalidHandshakeSignature,
    /// Phase 2: X25519 produced the all-zero shared secret (low-order peer key).
    WeakSharedSecret,
    /// Phase 2: a ctl kind outside `[a-z0-9-]{1,32}`.
    InvalidCtlKind,
    /// Phase 2: a ctl sequence number other than the expected next one (gap or repeat).
    CtlSequence,
    /// Phase 2: the ctl sequence would pass 2^53 - 1.
    CtlSequenceExhausted,
    /// Phase 2: a ctl AEAD tag did not verify.
    CtlAuthenticationFailed,
    /// Phase 2: an earlier failure ended this ctl channel.
    CtlChannelClosed,
    /// Phase 2: a pairing code that is not 10 Crockford base32 characters.
    InvalidPairingCode,
    /// Phase 2: a pairing MAC did not match.
    PairingMacMismatch,
    /// Phase 2: a DTLS fingerprint that is not `sha-256` plus 32 uppercase hex pairs.
    InvalidFingerprint,
}

impl fmt::Display for DeviceKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId => write!(f, "device ID must be 16 Crockford base32 characters"),
            Self::InvalidPublicKey => write!(f, "invalid Ed25519 public key"),
            Self::UnknownRole => write!(
                f,
                "unknown role (only \"host\" and \"viewer\" are accepted)"
            ),
            Self::Malformed(reason) => write!(f, "malformed key file JSON: {reason}"),
            Self::MissingField(field) => write!(f, "key file is missing required field `{field}`"),
            Self::UnknownField(field) => write!(f, "key file has unknown field `{field}`"),
            Self::InvalidField(field) => {
                write!(f, "key file field `{field}` has an invalid type or value")
            }
            Self::InvalidEncoding(field) => write!(
                f,
                "key file field `{field}` is not canonical unpadded base64url"
            ),
            Self::InvalidLength(field) => {
                write!(f, "key file field `{field}` has the wrong length")
            }
            Self::UnsupportedFormat => {
                write!(f, "not a BubblePlay device key file (unknown format)")
            }
            Self::UnsupportedVersion => write!(
                f,
                "unsupported device key file version (this build reads version 1)"
            ),
            Self::UnsupportedType => {
                write!(f, "unsupported device key type (this build reads ed25519)")
            }
            Self::UnsupportedKdf => write!(
                f,
                "unsupported key derivation (only argon2id, m=65536, t=3, p=1)"
            ),
            Self::UnsupportedAead => write!(f, "unsupported cipher (only aes-256-gcm)"),
            Self::IncompleteProtection => write!(
                f,
                "key file protection is incomplete: kdf and aead must both be null or both present"
            ),
            Self::IdMismatch => write!(f, "device ID does not match the public key"),
            Self::PublicKeyMismatch => write!(f, "public key does not match the private seed"),
            Self::PassphraseRequired => write!(f, "this device key is passphrase-protected"),
            Self::PassphraseNotAccepted => write!(f, "this device key is not passphrase-protected"),
            Self::AuthenticationFailed => {
                write!(f, "wrong passphrase or the key file was modified")
            }
            Self::Entropy => write!(f, "operating system randomness is unavailable"),
            Self::InvalidSignature => write!(f, "relay proof signature did not verify"),
            Self::Internal(what) => write!(f, "internal cryptography failure: {what}"),
            Self::InvalidHandshakeSignature => write!(f, "handshake signature did not verify"),
            Self::WeakSharedSecret => write!(f, "X25519 shared secret is all zero"),
            Self::InvalidCtlKind => write!(f, "control message kind must match [a-z0-9-]{{1,32}}"),
            Self::CtlSequence => write!(f, "control message sequence number out of order"),
            Self::CtlSequenceExhausted => {
                write!(f, "control message sequence number limit reached")
            }
            Self::CtlAuthenticationFailed => {
                write!(f, "control message did not authenticate")
            }
            Self::CtlChannelClosed => write!(f, "control channel ended after an earlier failure"),
            Self::InvalidPairingCode => {
                write!(f, "pairing code must be 10 Crockford base32 characters")
            }
            Self::PairingMacMismatch => write!(f, "pairing proof did not match"),
            Self::InvalidFingerprint => write!(
                f,
                "DTLS fingerprint must be sha-256 followed by 32 uppercase hex pairs"
            ),
        }
    }
}

impl std::error::Error for DeviceKeyError {}

impl From<bubblebuffer_device_key::DeviceKeyError> for DeviceKeyError {
    fn from(error: bubblebuffer_device_key::DeviceKeyError) -> Self {
        use bubblebuffer_device_key::DeviceKeyError as Shared;
        match error {
            Shared::InvalidId => Self::InvalidId,
            Shared::InvalidPublicKey => Self::InvalidPublicKey,
            Shared::UnknownRole => Self::UnknownRole,
            Shared::Malformed(reason) => Self::Malformed(reason),
            Shared::MissingField(field) => Self::MissingField(field),
            Shared::UnknownField(field) => Self::UnknownField(field),
            Shared::InvalidField(field) => Self::InvalidField(field),
            Shared::InvalidEncoding(field) => Self::InvalidEncoding(field),
            Shared::InvalidLength(field) => Self::InvalidLength(field),
            Shared::UnsupportedFormat => Self::UnsupportedFormat,
            Shared::UnsupportedVersion => Self::UnsupportedVersion,
            Shared::UnsupportedType => Self::UnsupportedType,
            Shared::UnsupportedKdf => Self::UnsupportedKdf,
            Shared::UnsupportedAead => Self::UnsupportedAead,
            Shared::IncompleteProtection => Self::IncompleteProtection,
            Shared::IdMismatch => Self::IdMismatch,
            Shared::PublicKeyMismatch => Self::PublicKeyMismatch,
            Shared::PassphraseRequired => Self::PassphraseRequired,
            Shared::PassphraseNotAccepted => Self::PassphraseNotAccepted,
            Shared::AuthenticationFailed => Self::AuthenticationFailed,
            Shared::Entropy => Self::Entropy,
            Shared::InvalidSignature => Self::InvalidSignature,
            Shared::Internal(what) => Self::Internal(what),
        }
    }
}
