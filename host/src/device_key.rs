//! The PC device key on the host: the shared `bubblebuffer-device-key` types through
//! `bp-device-key`, plus the one host-only piece, the key the tray hands over in an environment
//! variable.
//!
//! Key files, IDs and relay proofs live in the shared crate. Nothing here reads a clock, the
//! network or storage.

use bp_device_key::KeyFile;
use zeroize::Zeroizing;

pub use bp_device_key::{relay_origin, DeviceId, DeviceIdentity, DeviceKeyError};

/// Environment variable that carries the PC key from the tray to the host: key file JSON in the
/// `bubblebuffer-device-key` format. The tray sets it on the child command only; the host removes
/// it before anything else.
pub const HANDOFF_ENV: &str = "BUBBLEPLAY_DEVICE_KEY";

/// Takes the handoff variable out of this process's environment. Call it first in `main`, so no
/// child process can inherit it. The value is returned even when it is later found invalid.
pub fn take_handoff_env() -> Option<Zeroizing<String>> {
    let value = std::env::var_os(HANDOFF_ENV);
    std::env::remove_var(HANDOFF_ENV);
    value
        .and_then(|value| value.into_string().ok())
        .map(Zeroizing::new)
}

/// Unlocks the handed-over key when there is one. `Ok(None)` means no key was handed over. A
/// passphrase-protected file is refused: nothing at the host can supply the passphrase.
pub fn identity_from_handoff(
    handoff: Option<&str>,
) -> Result<Option<DeviceIdentity>, DeviceKeyError> {
    handoff
        .map(|text| {
            let file = KeyFile::parse(text)?;
            if file.is_protected() {
                return Err(DeviceKeyError::PassphraseRequired);
            }
            Ok(DeviceIdentity::from_signing_key(file.unlock(None)?))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const SHARED: &str = include_str!("../../fixtures/shared/device-keys-contract.json");

    /// The `file` object of one key file in the shared fixture, as key file JSON text.
    fn key_file(name: &str) -> (String, String) {
        let fixture: Value = serde_json::from_str(SHARED).unwrap();
        let file = &fixture["keyFiles"][name]["file"];
        let id = file["id"].as_str().unwrap().to_owned();
        (file.to_string(), id)
    }

    #[test]
    fn handoff_env_is_removed_on_read_even_when_the_value_is_invalid() {
        std::env::set_var(HANDOFF_ENV, "{not a key");
        let value = take_handoff_env();
        assert_eq!(value.as_deref().map(String::as_str), Some("{not a key"));
        assert!(std::env::var_os(HANDOFF_ENV).is_none());
        assert!(take_handoff_env().is_none());
    }

    #[test]
    fn handoff_identity_accepts_the_current_format_only() {
        let (current, current_id) = key_file("plain");
        let identity = identity_from_handoff(Some(&current)).unwrap().unwrap();
        assert_eq!(identity.id().to_string(), current_id);

        let mut other: Value = serde_json::from_str(&current).unwrap();
        other["format"] = Value::from("other-format");
        assert!(matches!(
            identity_from_handoff(Some(&other.to_string())),
            Err(DeviceKeyError::UnsupportedFormat)
        ));

        assert!(identity_from_handoff(None).unwrap().is_none());
    }

    #[test]
    fn handoff_identity_refuses_protected_and_malformed_files() {
        let (protected, _) = key_file("passphrase");
        assert!(matches!(
            identity_from_handoff(Some(&protected)),
            Err(DeviceKeyError::PassphraseRequired)
        ));
        assert!(identity_from_handoff(Some("{not a key")).is_err());
    }

    #[test]
    fn handoff_identity_debug_shows_the_id_alone() {
        let (current, id) = key_file("plain");
        let identity = identity_from_handoff(Some(&current)).unwrap().unwrap();
        let debug = format!("{identity:?}");
        assert!(debug.contains(&id));
        assert!(!debug.contains(current.as_str()));
    }
}
