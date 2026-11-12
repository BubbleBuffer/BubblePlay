//! The pairing code, its admit hash, the pair key and the comparison number (SAS).

use std::fmt;

use super::*;

/// Crockford base32 without I, L, O and U: the pairing code alphabet (same as device IDs).
pub(super) const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const PAIR_ADMIT_CONTEXT: &[u8] = b"bubbleplay-pair-admit-v1";
const PAIR_KEY_INFO: &[u8] = b"bubbleplay-pair-v1";
const PAIR_SAS_CONTEXT: &[u8] = b"bubbleplay-pair-sas-v1";

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
