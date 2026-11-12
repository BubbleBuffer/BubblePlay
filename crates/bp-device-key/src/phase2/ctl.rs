//! X25519 key agreement, the control keys and the sequenced control envelope.

use std::fmt;

use super::*;

const CTL_INFO_VIEWER_TO_PC: &[u8] = b"bubbleplay-ctl-v1 viewer-to-pc";
const CTL_INFO_PC_TO_VIEWER: &[u8] = b"bubbleplay-ctl-v1 pc-to-viewer";

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
    pub(super) next_seq: u64,
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
    pub(super) expected_seq: u64,
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
