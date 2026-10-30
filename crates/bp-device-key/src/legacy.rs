//! Reader for the old BubblePlay key file format (`bubbleplay-device-key`, version 1).
//!
//! Key files are now written only in the shared `bubblebuffer-device-key` format (see
//! [`crate::LegacyKeyFile`]). This module keeps the old strict reader so stored and imported files in
//! the old format can be opened once and converted: parse, unlock and the AAD, which carries the
//! old format name. It cannot create or serialise a file. Argon2id m=64 MiB t=3 p=1,
//! AES-256-GCM and the field layout are the same in both formats.
//!
//! `fixtures/device-keys-contract.json` (`legacyKeyFiles`) pins the old bytes.

use std::fmt;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::{Algorithm, Argon2, Block, Params, Version};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::{SigningKey, VerifyingKey};
use zeroize::{Zeroize, Zeroizing};

use crate::{DeviceId, DeviceKeyError};

/// The old format name, also authenticated in the AAD of protected files.
pub const FORMAT: &str = "bubbleplay-device-key";
const KEY_TYPE: &str = "ed25519";
const KDF_ALG: &str = "argon2id";
const AEAD_ALG: &str = "aes-256-gcm";
/// Argon2id parameters fixed by format version 1: 64 MiB, 3 passes, 1 lane, Argon2 v1.3.
const KDF_MEMORY_KIB: u32 = 65_536;
const KDF_PASSES: u32 = 3;
const KDF_LANES: u32 = 1;
const SEED_BYTES: usize = 32;
const SALT_BYTES: usize = 16;
const NONCE_BYTES: usize = 12;
/// Encrypted seed: ciphertext plus the 16-byte AES-GCM tag.
const SEALED_SEED_BYTES: usize = SEED_BYTES + 16;
const MAX_JSON_DEPTH: usize = 4;
const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// A parsed or created key file. Immutable; the seed is only ever unlocked into a caller-owned
/// `SigningKey`. A plain file holds the seed itself.
pub struct LegacyKeyFile {
    id: DeviceId,
    // Preserve imported spelling: JCS authenticates string values without normalizing IDs.
    id_text: String,
    label: String,
    created: String,
    public: [u8; 32],
    protection: Protection,
}

enum Protection {
    Plain {
        seed: Zeroizing<[u8; SEED_BYTES]>,
    },
    Protected {
        salt: [u8; SALT_BYTES],
        nonce: [u8; NONCE_BYTES],
        /// Ciphertext and tag of the seed, 48 bytes.
        sealed: Vec<u8>,
    },
}

impl LegacyKeyFile {
    /// here; `unlock` adds decryption and seed-to-public-to-ID validation.
    pub fn parse(text: &str) -> Result<Self, DeviceKeyError> {
        let mut entries = match parse_document(text)? {
            Json::Object(entries) => entries,
            _ => return Err(DeviceKeyError::Malformed("key file must be a JSON object")),
        };
        if take_str(&mut entries, "format", "format")?.as_str() != FORMAT {
            return Err(DeviceKeyError::UnsupportedFormat);
        }
        match take(&mut entries, "version", "version")? {
            Json::Int(1) => {}
            Json::Int(_) => return Err(DeviceKeyError::UnsupportedVersion),
            _ => return Err(DeviceKeyError::InvalidField("version")),
        }
        if take_str(&mut entries, "type", "type")?.as_str() != KEY_TYPE {
            return Err(DeviceKeyError::UnsupportedType);
        }
        let id_text = take_str(&mut entries, "id", "id")?;
        let label = take_str(&mut entries, "label", "label")?;
        let created = take_str(&mut entries, "created", "created")?;
        let public = decode_fixed::<32>(&take_str(&mut entries, "public", "public")?, "public")?;
        VerifyingKey::from_bytes(&public).map_err(|_| DeviceKeyError::InvalidEncoding("public"))?;

        let kdf = take(&mut entries, "kdf", "kdf")?;
        let aead = take(&mut entries, "aead", "aead")?;
        let mut private = take_str(&mut entries, "private", "private")?;
        let decoded = decode_bytes(&private, "private");
        private.zeroize();
        let private_bytes = decoded?;
        let protection = match (kdf, aead) {
            (Json::Null, Json::Null) => Protection::Plain {
                seed: fixed_secret(&private_bytes, "private")?,
            },
            (Json::Object(kdf), Json::Object(aead)) => {
                let salt = parse_kdf(kdf)?;
                let nonce = parse_aead(aead)?;
                if private_bytes.len() != SEALED_SEED_BYTES {
                    return Err(DeviceKeyError::InvalidLength("private"));
                }
                Protection::Protected {
                    salt,
                    nonce,
                    sealed: private_bytes.to_vec(),
                }
            }
            _ => return Err(DeviceKeyError::IncompleteProtection),
        };

        let id = DeviceId::parse(&id_text)?;
        reject_unknown(&entries, "")?;
        if let Protection::Plain { seed } = &protection {
            if SigningKey::from_bytes(seed).verifying_key().to_bytes() != public {
                return Err(DeviceKeyError::PublicKeyMismatch);
            }
        }
        if id != DeviceId::from_public_key(&public) {
            return Err(DeviceKeyError::IdMismatch);
        }
        Ok(Self {
            id,
            id_text: id_text.to_string(),
            label: label.to_string(),
            created: created.to_string(),
            public,
            protection,
        })
    }

    /// Recovers the signing key. Plain files take `None`; protected files take the passphrase.
    /// A wrong passphrase or altered metadata yields `AuthenticationFailed`.
    pub fn unlock(&self, passphrase: Option<&str>) -> Result<SigningKey, DeviceKeyError> {
        let seed = self.open_seed(passphrase)?;
        let signing = SigningKey::from_bytes(&seed);
        if signing.verifying_key().to_bytes() != self.public {
            return Err(DeviceKeyError::PublicKeyMismatch);
        }
        if DeviceId::from_public_key(&self.public) != self.id {
            return Err(DeviceKeyError::IdMismatch);
        }
        Ok(signing)
    }

    pub fn id(&self) -> DeviceId {
        self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn created(&self) -> &str {
        &self.created
    }

    pub fn public_key(&self) -> &[u8; 32] {
        &self.public
    }

    pub fn is_protected(&self) -> bool {
        matches!(self.protection, Protection::Protected { .. })
    }

    /// Returns the decrypted seed for a plain or protected file. Only the passphrase is checked
    /// here; the caller validates the seed against the public key and ID.
    fn open_seed(
        &self,
        passphrase: Option<&str>,
    ) -> Result<Zeroizing<[u8; SEED_BYTES]>, DeviceKeyError> {
        match (&self.protection, passphrase) {
            (Protection::Plain { seed }, None) => Ok(seed.clone()),
            (Protection::Plain { .. }, Some(_)) => Err(DeviceKeyError::PassphraseNotAccepted),
            (Protection::Protected { .. }, None) => Err(DeviceKeyError::PassphraseRequired),
            (
                Protection::Protected {
                    salt,
                    nonce,
                    sealed,
                },
                Some(passphrase),
            ) => {
                let kdf_key = derive_kdf_key(passphrase, salt)?;
                let aad = aad_bytes(
                    &self.id_text,
                    &self.label,
                    &self.created,
                    &self.public,
                    salt,
                );
                let cipher = aes_cipher(&kdf_key)?;
                let plain = Zeroizing::new(
                    cipher
                        .decrypt(
                            Nonce::from_slice(nonce),
                            Payload {
                                msg: sealed,
                                aad: &aad,
                            },
                        )
                        .map_err(|_| DeviceKeyError::AuthenticationFailed)?,
                );
                fixed_secret(&plain, "private")
            }
        }
    }
}

#[doc(hidden)]
impl LegacyKeyFile {
    /// The authenticated metadata (AAD) this file is sealed with, or None for a plain file.
    /// Exposed only so the contract tests can compare it with the shared fixture.
    pub fn aad_for_contract(&self) -> Option<Vec<u8>> {
        match &self.protection {
            Protection::Plain { .. } => None,
            Protection::Protected { salt, .. } => Some(aad_bytes(
                &self.id_text,
                &self.label,
                &self.created,
                &self.public,
                salt,
            )),
        }
    }
}

impl fmt::Debug for LegacyKeyFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LegacyKeyFile")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("created", &self.created)
            .field("protected", &self.is_protected())
            .finish_non_exhaustive()
    }
}

/// Key derivation: Argon2id v1.3 over the UTF-8 passphrase, 64 MiB, 3 passes, 1 lane, 32 bytes.
fn derive_kdf_key(
    passphrase: &str,
    salt: &[u8; SALT_BYTES],
) -> Result<Zeroizing<[u8; SEED_BYTES]>, DeviceKeyError> {
    let params = Params::new(KDF_MEMORY_KIB, KDF_PASSES, KDF_LANES, Some(SEED_BYTES))
        .map_err(|_| DeviceKeyError::Internal("argon2 parameters"))?;
    let mut key = Zeroizing::new([0u8; SEED_BYTES]);
    let mut memory = Zeroizing::new(vec![Block::default(); params.block_count()]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into_with_memory(passphrase.as_bytes(), salt, &mut key[..], &mut memory[..])
        .map_err(|_| DeviceKeyError::Internal("argon2 derivation"))?;
    Ok(key)
}

fn aes_cipher(key: &[u8; SEED_BYTES]) -> Result<Aes256Gcm, DeviceKeyError> {
    Aes256Gcm::new_from_slice(key).map_err(|_| DeviceKeyError::Internal("aes-256-gcm key"))
}

#[cfg(test)]
fn seal_seed(
    kdf_key: &[u8; SEED_BYTES],
    nonce: &[u8; NONCE_BYTES],
    aad: &[u8],
    seed: &[u8; SEED_BYTES],
) -> Result<Vec<u8>, DeviceKeyError> {
    aes_cipher(kdf_key)?
        .encrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: &seed[..],
                aad,
            },
        )
        .map_err(|_| DeviceKeyError::Internal("aes-256-gcm seal"))
}

/// Authenticated metadata: canonical JSON of every header field except `private` and `aead.nonce`,
/// keeping `aead.alg` and the KDF salt. Field values are used exactly as stored.
fn aad_bytes(
    id: &str,
    label: &str,
    created: &str,
    public: &[u8; 32],
    salt: &[u8; SALT_BYTES],
) -> Vec<u8> {
    let root = object([
        ("format", jstr(FORMAT)),
        ("version", Json::Int(1)),
        ("type", jstr(KEY_TYPE)),
        ("id", jstr(id)),
        ("label", jstr(label)),
        ("created", jstr(created)),
        ("public", jstr(&URL_SAFE_NO_PAD.encode(public))),
        (
            "kdf",
            object([
                ("alg", jstr(KDF_ALG)),
                ("m", Json::Int(i64::from(KDF_MEMORY_KIB))),
                ("t", Json::Int(i64::from(KDF_PASSES))),
                ("p", Json::Int(i64::from(KDF_LANES))),
                ("salt", jstr(&URL_SAFE_NO_PAD.encode(salt))),
            ]),
        ),
        ("aead", object([("alg", jstr(AEAD_ALG))])),
    ]);
    canonical(&root).into_bytes()
}

/// Strict unpadded base64url: the alphabet is checked, then the bytes must re-encode to the
/// same text, which rejects padding and non-zero trailing bits.
fn decode_bytes(value: &str, field: &'static str) -> Result<Zeroizing<Vec<u8>>, DeviceKeyError> {
    if !value.bytes().all(|b| BASE64URL.contains(&b)) {
        return Err(DeviceKeyError::InvalidEncoding(field));
    }
    let decoded = Zeroizing::new(
        URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| DeviceKeyError::InvalidEncoding(field))?,
    );
    if Zeroizing::new(URL_SAFE_NO_PAD.encode(decoded.as_slice())).as_str() != value {
        return Err(DeviceKeyError::InvalidEncoding(field));
    }
    Ok(decoded)
}

fn decode_fixed<const N: usize>(
    value: &str,
    field: &'static str,
) -> Result<[u8; N], DeviceKeyError> {
    let decoded = decode_bytes(value, field)?;
    <[u8; N]>::try_from(decoded.as_slice()).map_err(|_| DeviceKeyError::InvalidLength(field))
}

fn fixed_secret(
    bytes: &[u8],
    field: &'static str,
) -> Result<Zeroizing<[u8; SEED_BYTES]>, DeviceKeyError> {
    if bytes.len() != SEED_BYTES {
        return Err(DeviceKeyError::InvalidLength(field));
    }
    let mut out = Zeroizing::new([0u8; SEED_BYTES]);
    out.copy_from_slice(bytes);
    Ok(out)
}

fn parse_kdf(mut entries: Vec<(String, Json)>) -> Result<[u8; SALT_BYTES], DeviceKeyError> {
    if take_str(&mut entries, "alg", "kdf.alg")?.as_str() != KDF_ALG {
        return Err(DeviceKeyError::UnsupportedKdf);
    }
    let memory = take_int(&mut entries, "m", "kdf.m")?;
    let passes = take_int(&mut entries, "t", "kdf.t")?;
    let lanes = take_int(&mut entries, "p", "kdf.p")?;
    if memory != i64::from(KDF_MEMORY_KIB)
        || passes != i64::from(KDF_PASSES)
        || lanes != i64::from(KDF_LANES)
    {
        return Err(DeviceKeyError::UnsupportedKdf);
    }
    let salt = take_str(&mut entries, "salt", "kdf.salt")?;
    reject_unknown(&entries, "kdf")?;
    decode_fixed::<SALT_BYTES>(&salt, "kdf.salt")
}

fn parse_aead(mut entries: Vec<(String, Json)>) -> Result<[u8; NONCE_BYTES], DeviceKeyError> {
    if take_str(&mut entries, "alg", "aead.alg")?.as_str() != AEAD_ALG {
        return Err(DeviceKeyError::UnsupportedAead);
    }
    let nonce = take_str(&mut entries, "nonce", "aead.nonce")?;
    reject_unknown(&entries, "aead")?;
    decode_fixed::<NONCE_BYTES>(&nonce, "aead.nonce")
}

fn take(
    entries: &mut Vec<(String, Json)>,
    key: &str,
    path: &'static str,
) -> Result<Json, DeviceKeyError> {
    let index = entries
        .iter()
        .position(|(k, _)| k.as_str() == key)
        .ok_or(DeviceKeyError::MissingField(path))?;
    Ok(entries.swap_remove(index).1)
}

fn take_str(
    entries: &mut Vec<(String, Json)>,
    key: &str,
    path: &'static str,
) -> Result<Zeroizing<String>, DeviceKeyError> {
    match take(entries, key, path)? {
        Json::Str(value) => Ok(value),
        _ => Err(DeviceKeyError::InvalidField(path)),
    }
}

fn take_int(
    entries: &mut Vec<(String, Json)>,
    key: &str,
    path: &'static str,
) -> Result<i64, DeviceKeyError> {
    match take(entries, key, path)? {
        Json::Int(value) => Ok(value),
        _ => Err(DeviceKeyError::InvalidField(path)),
    }
}

fn reject_unknown(entries: &[(String, Json)], scope: &str) -> Result<(), DeviceKeyError> {
    match entries.first() {
        None => Ok(()),
        Some((key, _)) if scope.is_empty() => Err(DeviceKeyError::UnknownField(key.clone())),
        Some((key, _)) => Err(DeviceKeyError::UnknownField(format!("{scope}.{key}"))),
    }
}

// ---------------------------------------------------------------------------
// Strict JSON for the key-file domain: objects, strings, integers and null only.
// Duplicate keys, floats, booleans, arrays, lone surrogates and control characters are refused.
// Canonical output follows RFC 8785 (JCS): UTF-16 code-unit key order and minimal escapes.

#[derive(Debug, Clone, PartialEq, Eq)]
enum Json {
    Null,
    Int(i64),
    Str(Zeroizing<String>),
    Object(Vec<(String, Json)>),
}

fn jstr(value: &str) -> Json {
    Json::Str(Zeroizing::new(value.to_owned()))
}

fn object<const N: usize>(entries: [(&str, Json); N]) -> Json {
    Json::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

fn parse_document(text: &str) -> Result<Json, DeviceKeyError> {
    let mut reader = Reader { text, pos: 0 };
    reader.skip_ws();
    let value = reader.value(0)?;
    reader.skip_ws();
    if reader.pos != text.len() {
        return Err(DeviceKeyError::Malformed(
            "trailing data after the document",
        ));
    }
    Ok(value)
}

struct Reader<'a> {
    text: &'a str,
    pos: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.pos).copied()
    }

    fn rest(&self) -> &str {
        &self.text[self.pos..]
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// `depth` is the nesting level of the enclosing object (0 at the top level).
    fn value(&mut self, depth: usize) -> Result<Json, DeviceKeyError> {
        match self.peek() {
            Some(b'{') => self.object(depth + 1),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b'-' | b'0'..=b'9') => self.integer(),
            Some(b'n') => self.literal("null", Json::Null),
            Some(b't' | b'f') => Err(DeviceKeyError::Malformed(
                "boolean values are not supported",
            )),
            Some(b'[') => Err(DeviceKeyError::Malformed("arrays are not supported")),
            _ => Err(DeviceKeyError::Malformed("unexpected token")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, DeviceKeyError> {
        if depth > MAX_JSON_DEPTH {
            return Err(DeviceKeyError::Malformed("objects are nested too deeply"));
        }
        self.pos += 1;
        let mut entries: Vec<(String, Json)> = Vec::new();
        self.skip_ws();
        if self.eat(b'}') {
            return Ok(Json::Object(entries));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(DeviceKeyError::Malformed("expected an object key"));
            }
            let key = self.string()?.to_string();
            self.skip_ws();
            if !self.eat(b':') {
                return Err(DeviceKeyError::Malformed("expected ':' after a key"));
            }
            self.skip_ws();
            let value = self.value(depth)?;
            if entries.iter().any(|(existing, _)| *existing == key) {
                return Err(DeviceKeyError::Malformed("duplicate key"));
            }
            entries.push((key, value));
            self.skip_ws();
            if self.eat(b',') {
                continue;
            }
            if self.eat(b'}') {
                return Ok(Json::Object(entries));
            }
            return Err(DeviceKeyError::Malformed("expected ',' or '}'"));
        }
    }

    fn string(&mut self) -> Result<Zeroizing<String>, DeviceKeyError> {
        self.pos += 1;
        let mut out = Zeroizing::new(String::new());
        loop {
            let ch = self
                .rest()
                .chars()
                .next()
                .ok_or(DeviceKeyError::Malformed("unterminated string"))?;
            self.pos += ch.len_utf8();
            match ch {
                '"' => return Ok(out),
                '\\' => out.push(self.escape()?),
                control if (control as u32) < 0x20 => {
                    return Err(DeviceKeyError::Malformed("control character in string"));
                }
                plain => out.push(plain),
            }
        }
    }

    fn escape(&mut self) -> Result<char, DeviceKeyError> {
        let ch = self
            .rest()
            .chars()
            .next()
            .ok_or(DeviceKeyError::Malformed("unterminated escape"))?;
        self.pos += ch.len_utf8();
        Ok(match ch {
            '"' => '"',
            '\\' => '\\',
            '/' => '/',
            'b' => '\u{8}',
            'f' => '\u{c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'u' => {
                let high = self.hex4()?;
                if (0xD800..=0xDBFF).contains(&high) {
                    if !self.rest().starts_with("\\u") {
                        return Err(DeviceKeyError::Malformed("lone surrogate in string"));
                    }
                    self.pos += 2;
                    let low = self.hex4()?;
                    if !(0xDC00..=0xDFFF).contains(&low) {
                        return Err(DeviceKeyError::Malformed("lone surrogate in string"));
                    }
                    char::from_u32(0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00))
                        .ok_or(DeviceKeyError::Malformed("invalid escape"))?
                } else {
                    char::from_u32(high)
                        .ok_or(DeviceKeyError::Malformed("lone surrogate in string"))?
                }
            }
            _ => return Err(DeviceKeyError::Malformed("invalid escape")),
        })
    }

    fn hex4(&mut self) -> Result<u32, DeviceKeyError> {
        let digits = self
            .text
            .get(self.pos..self.pos + 4)
            .ok_or(DeviceKeyError::Malformed("invalid unicode escape"))?;
        if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(DeviceKeyError::Malformed("invalid unicode escape"));
        }
        self.pos += 4;
        u32::from_str_radix(digits, 16)
            .map_err(|_| DeviceKeyError::Malformed("invalid unicode escape"))
    }

    fn integer(&mut self) -> Result<Json, DeviceKeyError> {
        let start = self.pos;
        let negative = self.eat(b'-');
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(DeviceKeyError::Malformed("leading zero in number"));
                }
            }
            Some(b'1'..=b'9') => self.skip_digits(),
            _ => return Err(DeviceKeyError::Malformed("invalid number")),
        }
        if matches!(self.peek(), Some(b'.' | b'e' | b'E')) {
            return Err(DeviceKeyError::Malformed(
                "fractions and exponents are not supported",
            ));
        }
        let digits = &self.text[start..self.pos];
        if negative && digits == "-0" {
            return Err(DeviceKeyError::Malformed("negative zero is not supported"));
        }
        digits
            .parse::<i64>()
            .map(Json::Int)
            .map_err(|_| DeviceKeyError::Malformed("integer out of range"))
    }

    fn skip_digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Json, DeviceKeyError> {
        if self.rest().starts_with(word) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(DeviceKeyError::Malformed("unexpected token"))
        }
    }
}

/// RFC 8785 canonical form: keys sorted by UTF-16 code units, minimal string escapes.
fn canonical(value: &Json) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Json, out: &mut String) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Int(number) => out.push_str(&number.to_string()),
        Json::Str(text) => write_string(text, out),
        Json::Object(entries) => {
            let mut sorted: Vec<&(String, Json)> = entries.iter().collect();
            sorted.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
            out.push('{');
            for (index, (key, child)) in sorted.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_canonical(child, out);
            }
            out.push('}');
        }
    }
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32))
            }
            plain => out.push(plain),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::{json, Value};

    const FIXTURE: &str = include_str!("../../../fixtures/device-keys-contract.json");
    const PASSPHRASE: &str = "correct horse battery staple";
    const CREATED: &str = "2026-10-13T09:30:00Z";

    fn fixture() -> Value {
        serde_json::from_str(FIXTURE).expect("fixture parses")
    }

    fn unhex(value: &str) -> Vec<u8> {
        (0..value.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&value[i..i + 2], 16).expect("hex"))
            .collect()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn b64(bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    fn seed_of(seed_hex: &str) -> [u8; 32] {
        unhex(seed_hex).try_into().expect("32-byte seed")
    }

    fn str_of(value: &Value) -> &str {
        value.as_str().expect("string")
    }

    /// Fixture file with one top-level field replaced.
    fn with_field(file: &Value, key: &str, value: Value) -> String {
        let mut file = file.clone();
        file[key] = value;
        file.to_string()
    }

    fn without_field(file: &Value, key: &str) -> String {
        let mut file = file.clone();
        file.as_object_mut().expect("object").remove(key);
        file.to_string()
    }

    fn parse_error(text: &str) -> DeviceKeyError {
        match LegacyKeyFile::parse(text) {
            Ok(_) => panic!("parse should fail"),
            Err(error) => error,
        }
    }

    fn plain_file() -> Value {
        fixture()["legacyKeyFiles"]["plain"]["file"].clone()
    }

    fn protected_file() -> Value {
        fixture()["legacyKeyFiles"]["passphrase"]["file"].clone()
    }

    #[test]
    fn plain_fixture_parses_and_unlocks() {
        let file = plain_file();
        let key_file = LegacyKeyFile::parse(&file.to_string()).expect("plain fixture");
        assert!(!key_file.is_protected());
        assert_eq!(key_file.id().to_string(), str_of(&file["id"]));
        assert_eq!(key_file.label(), "PC test");
        assert_eq!(key_file.created(), CREATED);
        let seed = fixture()["legacyKeyFiles"]["plain"]["seedHex"].clone();
        let signing = key_file.unlock(None).expect("plain unlock");
        assert_eq!(signing.to_bytes(), seed_of(str_of(&seed)));
        assert!(matches!(
            key_file.unlock(Some(PASSPHRASE)),
            Err(DeviceKeyError::PassphraseNotAccepted)
        ));
    }

    #[test]
    fn plain_file_checks_seed_public_and_id_consistency() {
        let file = plain_file();
        let viewer_public = fixture()["ids"][0]["public"].clone();
        let viewer_id = fixture()["ids"][0]["id"].clone();
        assert!(matches!(
            parse_error(&with_field(&file, "public", viewer_public)),
            DeviceKeyError::PublicKeyMismatch
        ));
        assert!(matches!(
            parse_error(&with_field(&file, "id", viewer_id)),
            DeviceKeyError::IdMismatch
        ));
    }

    #[test]
    fn protected_fixture_kdf_and_aad_match_exact_bytes() {
        let vector = fixture()["legacyKeyFiles"]["passphrase"].clone();
        let key_file =
            LegacyKeyFile::parse(&vector["file"].to_string()).expect("protected fixture");
        assert!(key_file.is_protected());
        let Protection::Protected { salt, .. } = &key_file.protection else {
            panic!("expected protected file");
        };
        let kdf_key = derive_kdf_key(PASSPHRASE, salt).expect("argon2");
        assert_eq!(hex(&kdf_key[..]), str_of(&vector["kdfKeyHex"]));
        let aad = aad_bytes(
            &key_file.id_text,
            &key_file.label,
            &key_file.created,
            &key_file.public,
            salt,
        );
        assert_eq!(
            String::from_utf8(aad.clone()).expect("utf8"),
            str_of(&vector["aadUtf8"])
        );
        assert_eq!(hex(&aad), str_of(&vector["aadHex"]));
    }

    #[test]
    fn protected_fixture_unlocks_to_the_viewer_seed() {
        let vector = fixture()["legacyKeyFiles"]["passphrase"].clone();
        let key_file =
            LegacyKeyFile::parse(&vector["file"].to_string()).expect("protected fixture");
        let signing = key_file.unlock(Some(PASSPHRASE)).expect("unlock");
        assert_eq!(signing.to_bytes(), seed_of(str_of(&vector["seedHex"])));
        assert!(matches!(
            key_file.unlock(None),
            Err(DeviceKeyError::PassphraseRequired)
        ));
    }

    #[test]
    fn protected_failure_vectors_are_authentication_failures() {
        let failures = fixture()["legacyKeyFiles"]["failures"].clone();
        assert_eq!(failures[0]["name"], "wrong-passphrase");
        assert_eq!(failures[1]["name"], "tampered-aad");
        let wrong =
            LegacyKeyFile::parse(&failures[0]["file"].to_string()).expect("structurally valid");
        assert!(matches!(
            wrong.unlock(Some(str_of(&failures[0]["passphrase"]))),
            Err(DeviceKeyError::AuthenticationFailed)
        ));
        let tampered =
            LegacyKeyFile::parse(&failures[1]["file"].to_string()).expect("structurally valid");
        assert!(matches!(
            tampered.unlock(Some(str_of(&failures[1]["passphrase"]))),
            Err(DeviceKeyError::AuthenticationFailed)
        ));
    }

    #[test]
    fn unlock_rejects_a_seed_that_does_not_match_the_public_key() {
        let vector = fixture()["legacyKeyFiles"]["passphrase"].clone();
        let file = LegacyKeyFile::parse(&vector["file"].to_string()).expect("protected fixture");
        let Protection::Protected { salt, nonce, .. } = &file.protection else {
            panic!("expected protected file");
        };
        // A correctly encrypted, authenticated seed that belongs to the PC key, not this file's key.
        let pc_seed = Zeroizing::new(seed_of(str_of(&fixture()["ids"][1]["seedHex"])));
        let kdf_key = derive_kdf_key(PASSPHRASE, salt).expect("argon2");
        let aad = aad_bytes(
            &file.id_text,
            &file.label,
            &file.created,
            &file.public,
            salt,
        );
        let sealed = seal_seed(&kdf_key, nonce, &aad, &pc_seed).expect("seal");
        let forged = LegacyKeyFile {
            id: file.id,
            id_text: file.id_text.clone(),
            label: file.label.clone(),
            created: file.created.clone(),
            public: file.public,
            protection: Protection::Protected {
                salt: *salt,
                nonce: *nonce,
                sealed,
            },
        };
        assert!(matches!(
            forged.unlock(Some(PASSPHRASE)),
            Err(DeviceKeyError::PublicKeyMismatch)
        ));
    }

    #[test]
    fn refuses_unknown_format_version_and_type() {
        let file = plain_file();
        assert!(matches!(
            parse_error(&with_field(&file, "format", json!("other-format"))),
            DeviceKeyError::UnsupportedFormat
        ));
        assert!(matches!(
            parse_error(&with_field(&file, "version", json!(2))),
            DeviceKeyError::UnsupportedVersion
        ));
        assert!(matches!(
            parse_error(&with_field(&file, "version", json!("1"))),
            DeviceKeyError::InvalidField("version")
        ));
        assert!(matches!(
            parse_error(&with_field(&file, "type", json!("rsa"))),
            DeviceKeyError::UnsupportedType
        ));
        assert!(matches!(
            parse_error(&without_field(&file, "format")),
            DeviceKeyError::MissingField("format")
        ));
    }

    #[test]
    fn refuses_invalid_lengths_and_encodings() {
        let file = plain_file();
        let public = str_of(&file["public"]).to_owned();
        let private = str_of(&file["private"]).to_owned();
        assert!(matches!(
            parse_error(&with_field(&file, "public", json!(b64(&[0u8; 31])))),
            DeviceKeyError::InvalidLength("public")
        ));
        assert!(matches!(
            parse_error(&with_field(&file, "public", json!(format!("{public}=")))),
            DeviceKeyError::InvalidEncoding("public")
        ));
        assert!(matches!(
            parse_error(&with_field(
                &file,
                "public",
                json!(format!("+{}", &public[1..]))
            )),
            DeviceKeyError::InvalidEncoding("public")
        ));
        // Last character of 32 bytes carries two unused bits: changing bit 0 keeps the decoded bytes.
        let alphabet = std::str::from_utf8(BASE64URL).expect("ascii");
        let last = public.chars().last().expect("non-empty");
        let index = alphabet.find(last).expect("alphabet");
        let mut non_canonical = public.clone();
        non_canonical.pop();
        non_canonical.push(alphabet.as_bytes()[index ^ 1] as char);
        assert!(matches!(
            parse_error(&with_field(&file, "public", json!(non_canonical))),
            DeviceKeyError::InvalidEncoding("public")
        ));
        assert!(matches!(
            parse_error(&with_field(&file, "private", json!(b64(&[7u8; 31])))),
            DeviceKeyError::InvalidLength("private")
        ));
        assert!(matches!(
            parse_error(&with_field(&file, "private", json!(b64(&[7u8; 48])))),
            DeviceKeyError::InvalidLength("private")
        ));
        assert!(!private.is_empty());

        let protected = protected_file();
        assert!(matches!(
            parse_error(&with_field(&protected, "private", json!(b64(&[7u8; 32])))),
            DeviceKeyError::InvalidLength("private")
        ));
        let mut short_salt = protected.clone();
        short_salt["kdf"]["salt"] = json!(b64(&[1u8; 15]));
        assert!(matches!(
            parse_error(&short_salt.to_string()),
            DeviceKeyError::InvalidLength("kdf.salt")
        ));
        let mut short_nonce = protected.clone();
        short_nonce["aead"]["nonce"] = json!(b64(&[1u8; 11]));
        assert!(matches!(
            parse_error(&short_nonce.to_string()),
            DeviceKeyError::InvalidLength("aead.nonce")
        ));
        let mut padded_salt = protected.clone();
        padded_salt["kdf"]["salt"] = json!(format!("{}==", str_of(&protected["kdf"]["salt"])));
        assert!(matches!(
            parse_error(&padded_salt.to_string()),
            DeviceKeyError::InvalidEncoding("kdf.salt")
        ));
    }

    #[test]
    fn imported_id_spelling_is_preserved_for_authentication() {
        let plain = plain_file();
        let alternate = str_of(&plain["id"]).to_lowercase().replace('-', "");
        let parsed =
            LegacyKeyFile::parse(&with_field(&plain, "id", json!(alternate))).expect("ID aliases");
        assert_eq!(parsed.id().to_string(), str_of(&plain["id"]));
        assert_eq!(parsed.id_text, alternate);
        let protected = protected_file();
        let tampered = LegacyKeyFile::parse(&with_field(
            &protected,
            "id",
            json!(str_of(&protected["id"]).to_lowercase()),
        ))
        .unwrap();
        assert!(matches!(
            tampered.unlock(Some(PASSPHRASE)),
            Err(DeviceKeyError::AuthenticationFailed)
        ));
        assert!(matches!(
            parse_error(&with_field(&plain, "id", json!("bad"))),
            DeviceKeyError::InvalidId
        ));
    }

    #[test]
    fn refuses_incomplete_and_unsupported_protection() {
        let protected = protected_file();
        let mut kdf_only = protected.clone();
        kdf_only["aead"] = Value::Null;
        assert!(matches!(
            parse_error(&kdf_only.to_string()),
            DeviceKeyError::IncompleteProtection
        ));
        let mut aead_only = protected.clone();
        aead_only["kdf"] = Value::Null;
        assert!(matches!(
            parse_error(&aead_only.to_string()),
            DeviceKeyError::IncompleteProtection
        ));
        let mut missing_aead = protected.clone();
        missing_aead.as_object_mut().expect("object").remove("aead");
        assert!(matches!(
            parse_error(&missing_aead.to_string()),
            DeviceKeyError::MissingField("aead")
        ));

        let mut argon2i = protected.clone();
        argon2i["kdf"]["alg"] = json!("argon2i");
        assert!(matches!(
            parse_error(&argon2i.to_string()),
            DeviceKeyError::UnsupportedKdf
        ));
        let mut weaker = protected.clone();
        weaker["kdf"]["m"] = json!(32768);
        assert!(matches!(
            parse_error(&weaker.to_string()),
            DeviceKeyError::UnsupportedKdf
        ));
        let mut fewer_passes = protected.clone();
        fewer_passes["kdf"]["t"] = json!(2);
        assert!(matches!(
            parse_error(&fewer_passes.to_string()),
            DeviceKeyError::UnsupportedKdf
        ));
        let mut more_lanes = protected.clone();
        more_lanes["kdf"]["p"] = json!(2);
        assert!(matches!(
            parse_error(&more_lanes.to_string()),
            DeviceKeyError::UnsupportedKdf
        ));
        let mut aes128 = protected.clone();
        aes128["aead"]["alg"] = json!("aes-128-gcm");
        assert!(matches!(
            parse_error(&aes128.to_string()),
            DeviceKeyError::UnsupportedAead
        ));
        let mut no_salt = protected.clone();
        no_salt["kdf"]
            .as_object_mut()
            .expect("object")
            .remove("salt");
        assert!(matches!(
            parse_error(&no_salt.to_string()),
            DeviceKeyError::MissingField("kdf.salt")
        ));
        let mut extra_kdf = protected.clone();
        extra_kdf["kdf"]["extra"] = json!(1);
        assert!(
            matches!(parse_error(&extra_kdf.to_string()), DeviceKeyError::UnknownField(ref f) if f == "kdf.extra")
        );
        assert!(matches!(
            parse_error(&with_field(&protected, "extra", json!("x"))),
            DeviceKeyError::UnknownField(ref f) if f == "extra"
        ));
    }

    #[test]
    fn refuses_duplicate_keys_and_non_canonical_json() {
        let text = plain_file().to_string();
        let duplicate_top = format!("{{\"label\":\"first\",{}", &text[1..]);
        assert!(matches!(
            parse_error(&duplicate_top),
            DeviceKeyError::Malformed("duplicate key")
        ));
        let protected = protected_file().to_string();
        let duplicate_nested = protected.replacen("\"m\":65536", "\"m\":65536,\"m\":65536", 1);
        assert_ne!(duplicate_nested, protected);
        assert!(matches!(
            parse_error(&duplicate_nested),
            DeviceKeyError::Malformed("duplicate key")
        ));
        assert!(matches!(
            parse_error(&format!("{text} x")),
            DeviceKeyError::Malformed(_)
        ));
        assert!(matches!(
            parse_error(&format!("{text}{text}")),
            DeviceKeyError::Malformed(_)
        ));
        assert!(matches!(
            parse_error(&with_field(&plain_file(), "extra", json!(true))),
            DeviceKeyError::Malformed(_)
        ));
        assert!(matches!(
            parse_error(&with_field(&plain_file(), "extra", json!([1]))),
            DeviceKeyError::Malformed(_)
        ));
        assert!(matches!(
            parse_error(&text.replace("\"version\":1", "\"version\":1.0")),
            DeviceKeyError::Malformed(_)
        ));
        assert!(matches!(
            parse_error(&text.replace("\"version\":1", "\"version\":01")),
            DeviceKeyError::Malformed(_)
        ));
        assert!(matches!(
            parse_error(&text.replace("\"version\":1", "\"version\":-0")),
            DeviceKeyError::Malformed(_)
        ));
        assert!(matches!(
            parse_error(&text.replace("\"version\":1", "\"version\":1e0")),
            DeviceKeyError::Malformed(_)
        ));
        assert!(matches!(parse_error("[1]"), DeviceKeyError::Malformed(_)));
        assert!(matches!(parse_error(""), DeviceKeyError::Malformed(_)));
        assert!(matches!(
            parse_error(&text.replace("\"PC test\"", "\"PC\\ud800test\"")),
            DeviceKeyError::Malformed(_)
        ));
        assert!(matches!(
            parse_error(&text.replace("\"PC test\"", "\"PC\u{1}test\"")),
            DeviceKeyError::Malformed(_)
        ));
        assert!(matches!(
            parse_error("{\"a\":{\"b\":{\"c\":{\"d\":{\"e\":1}}}}}"),
            DeviceKeyError::Malformed(_)
        ));
        // Insignificant whitespace is fine; escapes that denote the same text are fine too.
        let spaced = format!("\n\t{} \r\n", text);
        assert!(LegacyKeyFile::parse(&spaced).is_ok());
        let escaped_label = text.replace("\"PC test\"", "\"\\u0050C test\"");
        assert_eq!(
            LegacyKeyFile::parse(&escaped_label)
                .expect("escapes")
                .label(),
            "PC test"
        );
        let surrogate_label = text.replace("\"PC test\"", "\"\\ud83d\\ude00\"");
        assert_eq!(
            LegacyKeyFile::parse(&surrogate_label)
                .expect("pair")
                .label(),
            "\u{1F600}"
        );
    }

    #[test]
    fn json_canonical_form_sorts_utf16_and_escapes_strings() {
        let entries = vec![
            ("\u{FFFD}".to_owned(), Json::Int(5)),
            ("\u{1F600}".to_owned(), Json::Int(4)),
            ("é".to_owned(), Json::Int(3)),
            ("a".to_owned(), Json::Int(2)),
            ("B".to_owned(), Json::Int(1)),
        ];
        let expected = "{\"B\":1,\"a\":2,\"é\":3,\"\u{1F600}\":4,\"\u{FFFD}\":5}";
        assert_eq!(canonical(&Json::Object(entries.clone())), expected);
        let mut reversed = entries;
        reversed.reverse();
        assert_eq!(canonical(&Json::Object(reversed)), expected);

        let tricky = "\"\\\u{8}\u{c}\n\r\t\u{1}\u{1f}\u{7f}é\u{1F600}/";
        let escaped = r#""\"\\\b\f\n\r\t\u0001\u001f"#.to_owned() + "\u{7f}é\u{1F600}/\"";
        assert_eq!(canonical(&jstr(tricky)), escaped);
        assert_eq!(canonical(&Json::Null), "null");
        assert_eq!(canonical(&Json::Int(-5)), "-5");
        assert_eq!(canonical(&Json::Int(0)), "0");
        assert_eq!(canonical(&Json::Object(Vec::new())), "{}");

        let parsed = parse_document(r#"{"b":"é","a":{"x":-7}}"#).expect("parse");
        assert_eq!(canonical(&parsed), "{\"a\":{\"x\":-7},\"b\":\"é\"}");
    }

    #[test]
    fn aad_is_canonical_for_quotes_and_non_ascii_labels() {
        let vector = fixture()["legacyKeyFiles"]["passphrase"].clone();
        let file = LegacyKeyFile::parse(&vector["file"].to_string()).expect("protected fixture");
        assert_eq!(file.label(), "Viewer \"test\" café");
        let Protection::Protected { salt, .. } = &file.protection else {
            panic!("expected protected file");
        };
        let aad = aad_bytes(
            &file.id_text,
            &file.label,
            &file.created,
            &file.public,
            salt,
        );
        let text = String::from_utf8(aad).expect("utf8");
        assert!(text.contains(r#""label":"Viewer \"test\" café""#));
        assert!(text.starts_with(r#"{"aead":{"alg":"aes-256-gcm"},"created":"#));
    }

    #[test]
    fn debug_output_hides_the_seed() {
        let key = LegacyKeyFile::parse(&plain_file().to_string()).expect("plain fixture");
        let seed = key.unlock(None).expect("unlock").to_bytes();
        let debug = format!("{key:?}");
        assert!(debug.contains(&key.id().to_string()));
        assert!(!debug.contains(&hex(&seed)));
        assert!(!debug.contains(&b64(&seed)));
        let protected =
            LegacyKeyFile::parse(&protected_file().to_string()).expect("protected fixture");
        let debug = format!("{protected:?}");
        assert!(debug.contains("protected: true"));
        assert!(!debug.contains("salt"));
    }

    #[test]
    fn the_new_format_is_not_a_legacy_file() {
        let mut file = plain_file();
        file["format"] = json!(bubblebuffer_device_key::FORMAT);
        assert!(matches!(
            parse_error(&file.to_string()),
            DeviceKeyError::UnsupportedFormat
        ));
    }
}
