//! The PCs this viewer knows: `%LOCALAPPDATA%\BubblePlay\viewer\pcs.json`.
//!
//! A JSON array of `{name, id, public, relay}`. `id` is the PC's canonical device ID, `public`
//! its pinned public key (base64url, 32 bytes; empty until paired) and `relay` the relay URL.
//! Pairing adds entries through [`PcStore::add`].

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use bp_device_key::DeviceId;
use serde::{Deserialize, Serialize};

pub const PCS_FILE_NAME: &str = "pcs.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PcEntry {
    pub name: String,
    pub id: String,
    /// The pinned PC public key, base64url without padding; empty until paired.
    #[serde(default)]
    pub public: String,
    pub relay: String,
}

impl PcEntry {
    /// Checks the entry and returns it with the ID in canonical form.
    fn validated(mut self) -> Result<Self> {
        let id = DeviceId::parse(&self.id).map_err(|_| anyhow!("invalid PC ID {:?}", self.id))?;
        self.id = id.to_string();
        if !self.public.is_empty() {
            let public: [u8; 32] = URL_SAFE_NO_PAD
                .decode(&self.public)
                .ok()
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(|| anyhow!("PC {}: public key is not 32 bytes of base64url", id))?;
            if DeviceId::from_public_key(&public) != id {
                bail!("PC {id}: public key does not match the ID");
            }
        }
        let relay = url::Url::parse(&self.relay)
            .map_err(|_| anyhow!("PC {id}: invalid relay URL {:?}", self.relay))?;
        if !matches!(relay.scheme(), "https" | "http" | "wss" | "ws") {
            bail!("PC {id}: relay URL must be https, http, wss or ws");
        }
        Ok(self)
    }
}

/// `pcs.json` in one directory.
pub struct PcStore {
    path: PathBuf,
}

impl PcStore {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            path: dir.join(PCS_FILE_NAME),
        }
    }

    /// `%LOCALAPPDATA%\BubblePlay\viewer\pcs.json`.
    pub fn default_location() -> Result<Self> {
        Ok(Self::in_dir(&super::viewer_dir()?))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// All entries. A missing file is an empty list.
    pub fn load(&self) -> Result<Vec<PcEntry>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", self.path.display()))
            }
        };
        let entries: Vec<PcEntry> = serde_json::from_str(&text)
            .with_context(|| format!("{} is not a PC list", self.path.display()))?;
        entries.into_iter().map(PcEntry::validated).collect()
    }

    /// Replaces the file with `entries`.
    pub fn save(&self, entries: &[PcEntry]) -> Result<()> {
        let mut text = serde_json::to_string_pretty(entries)?;
        text.push('\n');
        super::write_replacing(&self.path, text.as_bytes())
    }

    /// Adds a PC and returns the stored (canonical) entry. A PC that is already listed is
    /// refused: replacing it could silently change its pinned key.
    // Stage B pairing (`--pair <code>`) is the first caller outside the tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn add(&self, entry: PcEntry) -> Result<PcEntry> {
        let entry = entry.validated()?;
        let mut entries = self.load()?;
        if entries.iter().any(|known| known.id == entry.id) {
            bail!("PC {} is already in the list; remove it first", entry.id);
        }
        entries.push(entry.clone());
        self.save(&entries)?;
        Ok(entry)
    }

    /// Removes a PC by ID (any accepted spelling). Returns whether it was listed.
    pub fn remove(&self, id: &str) -> Result<bool> {
        let id = DeviceId::parse(id)
            .map_err(|_| anyhow!("invalid PC ID {id:?}"))?
            .to_string();
        let mut entries = self.load()?;
        let before = entries.len();
        entries.retain(|entry| entry.id != id);
        if entries.len() == before {
            return Ok(false);
        }
        self.save(&entries)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_dir::TempDir;

    // From fixtures/device-keys-contract.json (the "PC test" key).
    const PC_ID: &str = "77VH-7M56-8GJK-Y12J";
    const PC_PUBLIC: &str = "PUAXw-hDiVqStwqnTRt-vJyYLM8uxJaMwM1V8Sr0Zgw";
    const OTHER_ID: &str = "47Z3-3QX1-AJH6-2RKB";

    fn entry(id: &str, public: &str) -> PcEntry {
        PcEntry {
            name: "Desk".into(),
            id: id.into(),
            public: public.into(),
            relay: "https://relay.example".into(),
        }
    }

    #[test]
    fn missing_file_is_an_empty_list() {
        let dir = TempDir::new("pcs-empty");
        let store = PcStore::in_dir(dir.path());
        assert!(store.load().unwrap().is_empty());
        assert!(!store.remove(PC_ID).unwrap());
    }

    #[test]
    fn add_save_load_remove() {
        let dir = TempDir::new("pcs-roundtrip");
        let store = PcStore::in_dir(dir.path());
        // Lower case and no hyphens are accepted and stored canonical.
        let added = store
            .add(entry(&PC_ID.to_lowercase().replace('-', ""), PC_PUBLIC))
            .unwrap();
        assert_eq!(added.id, PC_ID);
        store.add(entry(OTHER_ID, "")).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded, vec![entry(PC_ID, PC_PUBLIC), entry(OTHER_ID, "")]);
        let text = std::fs::read_to_string(store.path()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let keys: Vec<&str> = value[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys.len(), 4);
        for key in ["name", "id", "public", "relay"] {
            assert!(keys.contains(&key), "{keys:?}");
        }

        assert!(
            store.add(entry(PC_ID, "")).is_err(),
            "duplicates are refused"
        );
        assert!(store.remove(&PC_ID.to_lowercase()).unwrap());
        assert!(!store.remove(PC_ID).unwrap());
        assert_eq!(store.load().unwrap(), vec![entry(OTHER_ID, "")]);
    }

    #[test]
    fn bad_entries_are_refused() {
        let dir = TempDir::new("pcs-bad");
        let store = PcStore::in_dir(dir.path());
        assert!(store.add(entry("not-an-id", "")).is_err());
        // The public key of a different device.
        assert!(store.add(entry(OTHER_ID, PC_PUBLIC)).is_err());
        assert!(store.add(entry(PC_ID, "c2hvcnQ")).is_err());
        let mut bad_relay = entry(PC_ID, "");
        bad_relay.relay = "ftp://relay.example".into();
        assert!(store.add(bad_relay).is_err());
        assert!(store.load().unwrap().is_empty());

        std::fs::write(
            store.path(),
            r#"[{"name":"x","id":"77VH-7M56-8GJK-Y12J","relay":"https://r.example","extra":1}]"#,
        )
        .unwrap();
        assert!(store.load().is_err(), "unknown fields are refused");
    }
}
