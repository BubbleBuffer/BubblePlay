//! The device key file in the shared `bubblebuffer-device-key` format (`.bpkey`), and the
//! one-time conversion of files in the old `bubbleplay-device-key` format.
//!
//! [`KeyFile`] wraps the shared crate's key file so callers keep this crate's error type. It
//! reads and writes only the shared format. [`read_key_file`] also accepts the old format, and
//! [`convert_legacy`] turns an old file into a new one with the same key, label and timestamp.
//!
//! Nothing here reads a clock, the network or storage. Callers supply the label and the created
//! timestamp and decide where the file lives. Unlocked seeds are never cached on a `KeyFile`.

use std::fmt;

use ed25519_dalek::SigningKey;

use crate::legacy::LegacyKeyFile;
use crate::{DeviceId, DeviceIdentity, DeviceKeyError};

/// A parsed or created key file in the shared format. Immutable; the seed is only ever unlocked
/// into a caller-owned `SigningKey`. A plain file stores the seed itself, which is why `to_json`
/// on one is secret.
pub struct KeyFile(bubblebuffer_device_key::KeyFile);

impl KeyFile {
    /// The format name every file written by this crate carries.
    pub const FORMAT: &'static str = bubblebuffer_device_key::FORMAT;

    /// Creates a new key from OS randomness. Without a passphrase the file stores the seed.
    pub fn create(
        label: &str,
        created: &str,
        passphrase: Option<&str>,
    ) -> Result<Self, DeviceKeyError> {
        Ok(Self(bubblebuffer_device_key::KeyFile::create(
            label, created, passphrase,
        )?))
    }

    /// Wraps an existing identity, protected with fresh salt and nonce when a passphrase is given.
    pub fn from_identity(
        identity: &DeviceIdentity,
        label: &str,
        created: &str,
        passphrase: Option<&str>,
    ) -> Result<Self, DeviceKeyError> {
        Ok(Self(bubblebuffer_device_key::KeyFile::from_device(
            &identity.key(),
            label,
            created,
            passphrase,
        )?))
    }

    /// Parses and validates a key file in the shared format. An old-format file is
    /// `UnsupportedFormat` here; [`read_key_file`] accepts both.
    pub fn parse(text: &str) -> Result<Self, DeviceKeyError> {
        Ok(Self(bubblebuffer_device_key::KeyFile::parse(text)?))
    }

    /// Recovers the signing key. Plain files take `None`; protected files take the passphrase.
    /// A wrong passphrase or altered metadata yields `AuthenticationFailed`.
    pub fn unlock(&self, passphrase: Option<&str>) -> Result<SigningKey, DeviceKeyError> {
        Ok(self.0.unlock(passphrase)?)
    }

    /// Unlocks with the current passphrase, then returns the same identity protected by
    /// `new_passphrase` (or plain when `None`). Protected output always has a fresh salt and nonce.
    pub fn rewrap(
        &self,
        current_passphrase: Option<&str>,
        new_passphrase: Option<&str>,
    ) -> Result<Self, DeviceKeyError> {
        Ok(Self(self.0.rewrap(current_passphrase, new_passphrase)?))
    }

    /// Stable serialization: fixed field order, two-space indentation, no trailing newline.
    /// The output contains `private` as stored: the seed for plain files.
    pub fn to_json(&self) -> Result<String, DeviceKeyError> {
        Ok(self.0.to_json()?)
    }

    pub fn id(&self) -> DeviceId {
        self.0.id()
    }

    pub fn label(&self) -> &str {
        self.0.label()
    }

    pub fn created(&self) -> &str {
        self.0.created()
    }

    pub fn public_key(&self) -> &[u8; 32] {
        self.0.public_key()
    }

    pub fn is_protected(&self) -> bool {
        self.0.is_protected()
    }

    /// The authenticated metadata (AAD) this file is sealed with, or None for a plain file.
    /// Exposed only so the contract tests can compare it with the fixture.
    #[doc(hidden)]
    pub fn aad_for_contract(&self) -> Option<Vec<u8>> {
        self.0.aad_for_contract()
    }
}

impl fmt::Debug for KeyFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A stored or imported key file in either format.
#[derive(Debug)]
pub enum StoredKeyFile {
    /// The shared `bubblebuffer-device-key` format.
    Current(KeyFile),
    /// The old `bubbleplay-device-key` format, to be converted with [`convert_legacy`].
    Legacy(LegacyKeyFile),
}

impl StoredKeyFile {
    pub fn id(&self) -> DeviceId {
        match self {
            Self::Current(file) => file.id(),
            Self::Legacy(file) => file.id(),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Current(file) => file.label(),
            Self::Legacy(file) => file.label(),
        }
    }

    pub fn created(&self) -> &str {
        match self {
            Self::Current(file) => file.created(),
            Self::Legacy(file) => file.created(),
        }
    }

    pub fn public_key(&self) -> &[u8; 32] {
        match self {
            Self::Current(file) => file.public_key(),
            Self::Legacy(file) => file.public_key(),
        }
    }

    pub fn is_protected(&self) -> bool {
        match self {
            Self::Current(file) => file.is_protected(),
            Self::Legacy(file) => file.is_protected(),
        }
    }

    pub fn is_legacy(&self) -> bool {
        matches!(self, Self::Legacy(_))
    }

    /// Recovers the signing key from either format.
    pub fn unlock(&self, passphrase: Option<&str>) -> Result<SigningKey, DeviceKeyError> {
        match self {
            Self::Current(file) => file.unlock(passphrase),
            Self::Legacy(file) => file.unlock(passphrase),
        }
    }
}

/// Parses a key file in the shared format, or else in the old format. Any other failure is the
/// shared reader's error.
pub fn read_key_file(text: &str) -> Result<StoredKeyFile, DeviceKeyError> {
    match KeyFile::parse(text) {
        Ok(file) => Ok(StoredKeyFile::Current(file)),
        Err(DeviceKeyError::UnsupportedFormat) => {
            LegacyKeyFile::parse(text).map(StoredKeyFile::Legacy)
        }
        Err(error) => Err(error),
    }
}

/// Converts an old-format file to the shared format: the same key, label and created timestamp.
/// A plain file stays plain; a protected file stays protected under the same passphrase, with a
/// fresh salt and nonce. The new JSON is parsed back and unlocked before it is returned, and must
/// yield the old file's ID and public key.
///
/// Returns the converted file and its JSON (secret for a plain file).
pub fn convert_legacy(
    legacy: &LegacyKeyFile,
    passphrase: Option<&str>,
) -> Result<(KeyFile, String), DeviceKeyError> {
    let identity = DeviceIdentity::from_signing_key(legacy.unlock(passphrase)?);
    let converted =
        KeyFile::from_identity(&identity, legacy.label(), legacy.created(), passphrase)?;
    drop(identity);
    let json = converted.to_json()?;
    let reopened = KeyFile::parse(&json)?;
    let check = reopened.unlock(passphrase)?;
    if reopened.id() != legacy.id()
        || reopened.public_key() != legacy.public_key()
        || check.verifying_key().to_bytes() != *legacy.public_key()
        || reopened.is_protected() != legacy.is_protected()
    {
        return Err(DeviceKeyError::Internal(
            "converted key file does not match",
        ));
    }
    Ok((reopened, json))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const LOCAL: &str = include_str!("../../../fixtures/device-keys-contract.json");

    fn local() -> Value {
        serde_json::from_str(LOCAL).expect("fixture parses")
    }

    fn legacy(name: &str) -> (String, Value) {
        let vector = local()["legacyKeyFiles"][name].clone();
        (vector["file"].to_string(), vector)
    }

    #[test]
    fn read_key_file_tells_the_formats_apart() {
        let (plain, _) = legacy("plain");
        let stored = read_key_file(&plain).expect("legacy plain");
        assert!(stored.is_legacy() && !stored.is_protected());
        let StoredKeyFile::Legacy(old) = &stored else {
            panic!("expected legacy");
        };
        let (_, json) = convert_legacy(old, None).expect("convert");
        let stored = read_key_file(&json).expect("current");
        assert!(!stored.is_legacy());
        assert!(matches!(
            read_key_file(&plain.replace("bubbleplay-device-key", "other-format")),
            Err(DeviceKeyError::UnsupportedFormat)
        ));
        assert!(read_key_file("{").is_err());
    }

    #[test]
    fn plain_legacy_file_converts_to_the_same_identity() {
        let (text, vector) = legacy("plain");
        let old = LegacyKeyFile::parse(&text).expect("legacy plain");
        let (converted, json) = convert_legacy(&old, None).expect("convert");
        assert!(json.contains(&format!("\"format\": \"{}\"", KeyFile::FORMAT)));
        assert!(!json.contains("bubbleplay-device-key"));
        assert!(!converted.is_protected());
        assert_eq!(converted.id(), old.id());
        assert_eq!(converted.public_key(), old.public_key());
        assert_eq!(converted.label(), old.label());
        assert_eq!(converted.created(), old.created());
        let reopened = KeyFile::parse(&json).expect("new reader");
        assert_eq!(
            hex(&reopened.unlock(None).expect("unlock").to_bytes()),
            vector["seedHex"].as_str().expect("seed")
        );
        assert!(matches!(
            convert_legacy(&old, Some("x")),
            Err(DeviceKeyError::PassphraseNotAccepted)
        ));
    }

    #[test]
    fn protected_legacy_file_converts_under_the_same_passphrase() {
        let (text, vector) = legacy("passphrase");
        let passphrase = vector["passphrase"].as_str().expect("passphrase");
        let old = LegacyKeyFile::parse(&text).expect("legacy protected");
        assert!(matches!(
            convert_legacy(&old, None),
            Err(DeviceKeyError::PassphraseRequired)
        ));
        assert!(matches!(
            convert_legacy(&old, Some("wrong")),
            Err(DeviceKeyError::AuthenticationFailed)
        ));
        let (converted, json) = convert_legacy(&old, Some(passphrase)).expect("convert");
        assert!(converted.is_protected());
        assert_eq!(converted.id(), old.id());
        assert_eq!(converted.label(), old.label());
        let file: Value = serde_json::from_str(&json).expect("json");
        // Fresh protection, not the old salt and nonce.
        assert_ne!(file["kdf"]["salt"], vector["file"]["kdf"]["salt"]);
        assert_ne!(file["aead"]["nonce"], vector["file"]["aead"]["nonce"]);
        let reopened = KeyFile::parse(&json).expect("new reader");
        assert_eq!(
            hex(&reopened
                .unlock(Some(passphrase))
                .expect("unlock")
                .to_bytes()),
            vector["seedHex"].as_str().expect("seed")
        );
        assert!(matches!(
            reopened.unlock(Some("wrong")),
            Err(DeviceKeyError::AuthenticationFailed)
        ));
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}
