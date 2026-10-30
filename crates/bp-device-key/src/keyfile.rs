//! The device key file in the shared `bubblebuffer-device-key` format (`.bpkey`).
//!
//! [`KeyFile`] wraps the shared crate's key file so callers keep this crate's error type. It
//! reads and writes only the shared format; any other format is `UnsupportedFormat`.
//!
//! Nothing here reads a clock, the network or storage. Callers supply the label and the created
//! timestamp and decide where the file lives. Unlocked seeds are never cached on a `KeyFile`.

use std::fmt;

use ed25519_dalek::SigningKey;

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

    /// Parses and validates a key file in the shared format. A file in any other format is
    /// `UnsupportedFormat`.
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
