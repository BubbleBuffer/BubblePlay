//! The viewer's device key: create, import, export, show and unlock.
//!
//! At rest the key is `%LOCALAPPDATA%\BubblePlay\viewer\device-key.bpkey`: the portable
//! `.bpkey` JSON, wrapped as a CurrentUser DPAPI blob. A key with a passphrase is unlocked on
//! the console when a session opens it; a key that cannot be unlocked means no connection.
//! `--key <file.bpkey>` replaces the stored key with a plain (not DPAPI-wrapped) file for
//! development.
//!
//! Only the shared `bubblebuffer-device-key` format is read and written. A stored key in any
//! other format counts as no key: `exists` is false, `load` reports no key, and `create` or
//! `import` replace the file.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use bp_device_key::{DeviceId, DeviceIdentity, DeviceKeyError, KeyFile};
use zeroize::Zeroizing;

use super::dpapi;

pub const KEY_FILE_NAME: &str = "device-key.bpkey";

/// Supplies a passphrase when a protected key is unlocked.
pub type PassphraseSource<'a> = &'a mut dyn FnMut() -> Result<Zeroizing<String>>;

/// The DPAPI-wrapped key file in one directory.
pub struct KeyStore {
    path: PathBuf,
}

impl KeyStore {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            path: dir.join(KEY_FILE_NAME),
        }
    }

    /// `%LOCALAPPDATA%\BubblePlay\viewer\device-key.bpkey`.
    pub fn default_location() -> Result<Self> {
        Ok(Self::in_dir(&super::viewer_dir()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// True when a usable-format key is stored. A file in another format counts as no key (one
    /// line is logged); a corrupt file still counts as a key, so it is never replaced silently.
    pub fn exists(&self) -> bool {
        if !self.path.exists() {
            return false;
        }
        !self.is_other_format()
    }

    /// True (with one log line) when the stored key unwraps to a key file in another format.
    fn is_other_format(&self) -> bool {
        let other = self.read_text().is_ok_and(|text| {
            matches!(
                KeyFile::parse(&text),
                Err(DeviceKeyError::UnsupportedFormat)
            )
        });
        if other {
            log_other_format();
        }
        other
    }

    /// The unwrapped key file text.
    fn read_text(&self) -> Result<Zeroizing<String>> {
        let wrapped = self.read_wrapped()?;
        let plain = Zeroizing::new(dpapi::unprotect(&wrapped)?);
        let text = std::str::from_utf8(&plain).context("the stored key is not UTF-8")?;
        Ok(Zeroizing::new(text.to_owned()))
    }

    /// Reads, unwraps and parses the stored key. The key stays locked. A key in another format
    /// is reported as no key.
    pub fn load(&self) -> Result<KeyFile> {
        let text = self.read_text()?;
        match KeyFile::parse(&text) {
            Ok(file) => Ok(file),
            Err(DeviceKeyError::UnsupportedFormat) => {
                log_other_format();
                Err(self.no_key())
            }
            Err(error) => Err(anyhow!("the stored key is invalid: {error}")),
        }
    }

    /// Wraps and writes `key`. Refuses to replace an existing key: that would lose the
    /// viewer's identity, so the user exports and removes it on purpose first. A file in another
    /// format is not a key and is replaced.
    pub fn save_new(&self, key: &KeyFile) -> Result<()> {
        if self.exists() {
            bail!(
                "a viewer key already exists at {}; export it and remove the file first",
                self.path.display()
            );
        }
        let json = Zeroizing::new(key.to_json()?);
        let wrapped = dpapi::protect(json.as_bytes())?;
        super::write_replacing(&self.path, &wrapped)
    }

    /// Creates a new key, with a passphrase when one is given, and stores it.
    pub fn create(&self, label: &str, passphrase: Option<&str>) -> Result<KeyFile> {
        let key = KeyFile::create(label, &utc_now(), passphrase)?;
        self.save_new(&key)?;
        Ok(key)
    }

    /// Imports a `.bpkey` file and stores it. A protected key is unlocked once first, so a file
    /// whose seed does not match its public key and ID is refused before it is stored.
    pub fn import(&self, key: KeyFile, passphrase: PassphraseSource<'_>) -> Result<KeyFile> {
        let secret = passphrase_for(&key, passphrase)?;
        unlocked(key.id(), key.unlock(secret.as_ref().map(|s| s.as_str())))?;
        self.save_new(&key)?;
        Ok(key)
    }

    /// The portable `.bpkey` text of the stored key (DPAPI removed). The stored file is not
    /// changed.
    pub fn export(&self) -> Result<Zeroizing<String>> {
        Ok(Zeroizing::new(self.load()?.to_json()?))
    }

    /// The stored DPAPI blob as it is on disk.
    fn read_wrapped(&self) -> Result<Vec<u8>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(self.no_key()),
            Err(error) => Err(error).with_context(|| format!("read {}", self.path.display())),
        }
    }

    fn no_key(&self) -> anyhow::Error {
        anyhow!(
            "no viewer key at {}: run `bubbleplay-client key create` or `key import <file>`",
            self.path.display()
        )
    }
}

fn log_other_format() {
    eprintln!(
        "BubblePlay: the stored viewer key is in an unsupported format; treating it as no key"
    );
}

/// Maps a failed unlock to the console message. A wrong passphrase is named as such.
fn unlocked<T>(id: DeviceId, result: Result<T, DeviceKeyError>) -> Result<T> {
    result.map_err(|error| match error {
        DeviceKeyError::AuthenticationFailed => anyhow!("wrong passphrase for viewer key {id}"),
        other => anyhow!("viewer key {id} cannot be unlocked: {other}"),
    })
}

/// The passphrase for `key`, asked for only when the key is protected.
fn passphrase_for(
    key: &KeyFile,
    passphrase: PassphraseSource<'_>,
) -> Result<Option<Zeroizing<String>>> {
    if key.is_protected() {
        passphrase().map(Some)
    } else {
        Ok(None)
    }
}

fn parse(text: &str) -> Result<KeyFile> {
    KeyFile::parse(text).map_err(|error| anyhow!("not a usable .bpkey file: {error}"))
}

/// Unlocks `key`, asking `passphrase` only when the key is protected.
pub fn unlock(key: &KeyFile, passphrase: PassphraseSource<'_>) -> Result<DeviceIdentity> {
    let secret = passphrase_for(key, passphrase)?;
    let signing = unlocked(key.id(), key.unlock(secret.as_ref().map(|s| s.as_str())))?;
    Ok(DeviceIdentity::from_signing_key(signing))
}

/// Opens the viewer identity for a session: the stored key, or the plain file `--key` names.
/// A protected key asks for its passphrase on the console.
pub fn open_identity(key_override: Option<&Path>) -> Result<DeviceIdentity> {
    match key_override {
        Some(path) => {
            let key = read_plain(path)?;
            let prompt = format!("Passphrase for viewer key {}: ", key.id());
            unlock(&key, &mut || super::console::read_passphrase(&prompt))
        }
        None => {
            let store = KeyStore::default_location()?;
            let key = store.load()?;
            let prompt = format!("Passphrase for viewer key {}: ", key.id());
            unlock(&key, &mut || super::console::read_passphrase(&prompt))
        }
    }
}

/// Reads a portable (not DPAPI-wrapped) `.bpkey` file.
pub fn read_plain(path: &Path) -> Result<KeyFile> {
    let bytes =
        Zeroizing::new(std::fs::read(path).with_context(|| format!("read {}", path.display()))?);
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow!("{} is not a plain .bpkey file", path.display()))?;
    parse(text)
}

/// The current UTC time as `YYYY-MM-DDTHH:MM:SSZ`, for the key file's `created`.
fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    format_utc(secs)
}

fn format_utc(secs: u64) -> String {
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rest = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_dir::TempDir;
    use serde_json::Value;

    /// The shared format's vectors: `keyFiles` and `ids`.
    fn shared_fixture() -> Value {
        read_fixture("../fixtures/shared/device-keys-contract.json")
    }

    fn read_fixture(relative: &str) -> Value {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
        serde_json::from_str(&std::fs::read_to_string(path).expect("contract fixture")).unwrap()
    }

    fn fixed(text: &str) -> impl FnMut() -> Result<Zeroizing<String>> + '_ {
        move || Ok(Zeroizing::new(text.to_owned()))
    }

    fn never() -> Result<Zeroizing<String>> {
        panic!("a plain key must not ask for a passphrase")
    }

    /// A key file in another format, stored the way a key is: DPAPI-wrapped.
    fn store_other_format(store: &KeyStore) -> Vec<u8> {
        let mut file: Value = serde_json::from_str(
            &KeyFile::create("old", "2026-10-13T09:30:00Z", None)
                .unwrap()
                .to_json()
                .unwrap(),
        )
        .unwrap();
        file["format"] = Value::from("other-format");
        let wrapped = dpapi::protect(file.to_string().as_bytes()).unwrap();
        std::fs::write(store.path(), &wrapped).unwrap();
        wrapped
    }

    #[test]
    fn a_key_in_another_format_counts_as_no_key() {
        let dir = TempDir::new("other-format");
        let store = KeyStore::in_dir(dir.path());
        let before = store_other_format(&store);
        assert!(!store.exists());
        let error = store.load().unwrap_err();
        assert!(error.to_string().contains("no viewer key"), "{error}");
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
    }

    #[test]
    fn creating_a_key_replaces_a_key_in_another_format() {
        let dir = TempDir::new("other-format-create");
        let store = KeyStore::in_dir(dir.path());
        let before = store_other_format(&store);
        let created = store.create("New", None).unwrap();
        assert!(store.exists());
        assert_eq!(store.load().unwrap().id(), created.id());
        assert_ne!(std::fs::read(store.path()).unwrap(), before);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no backup is kept"
        );
    }

    #[test]
    fn importing_replaces_a_key_in_another_format_and_refuses_such_a_file() {
        let fixture = shared_fixture();
        let plain = &fixture["keyFiles"]["plain"]["file"];
        let dir = TempDir::new("other-format-import");
        let store = KeyStore::in_dir(dir.path());
        store_other_format(&store);
        let key = store
            .import(KeyFile::parse(&plain.to_string()).unwrap(), &mut never)
            .unwrap();
        assert_eq!(store.load().unwrap().id(), key.id());

        let mut foreign = plain.clone();
        foreign["format"] = Value::from("other-format");
        let file = dir.path().join("foreign.bpkey");
        std::fs::write(&file, foreign.to_string()).unwrap();
        assert!(read_plain(&file).is_err());
        assert!(open_identity(Some(&file)).is_err());
    }

    #[test]
    fn create_save_load_unlock_round_trip() {
        let dir = TempDir::new("roundtrip");
        let store = KeyStore::in_dir(dir.path());
        assert!(!store.exists());
        let created = store.create("Test viewer", None).unwrap();
        assert!(store.exists());
        assert_eq!(store.path(), dir.path().join(KEY_FILE_NAME));

        // On disk it is a DPAPI blob, not the JSON.
        let raw = std::fs::read(store.path()).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains(KeyFile::FORMAT));

        let loaded = store.load().unwrap();
        assert_eq!(loaded.id(), created.id());
        assert_eq!(loaded.label(), "Test viewer");
        assert!(!loaded.is_protected());
        let identity = unlock(&loaded, &mut never).unwrap();
        assert_eq!(identity.id(), created.id());
        assert_eq!(&identity.public_key(), created.public_key());

        // A second create would replace the identity and is refused.
        assert!(store.create("Again", None).is_err());
        assert_eq!(store.load().unwrap().id(), created.id());
    }

    #[test]
    fn protected_key_needs_the_right_passphrase() {
        let dir = TempDir::new("protected");
        let store = KeyStore::in_dir(dir.path());
        let created = store.create("Locked", Some("right horse")).unwrap();
        let loaded = store.load().unwrap();
        assert!(loaded.is_protected());
        let error = unlock(&loaded, &mut fixed("wrong horse")).unwrap_err();
        assert!(error.to_string().contains("wrong passphrase"), "{error}");
        let identity = unlock(&loaded, &mut fixed("right horse")).unwrap();
        assert_eq!(identity.id(), created.id());
    }

    #[test]
    fn export_is_the_portable_file_and_imports_elsewhere() {
        let first = TempDir::new("export");
        let store = KeyStore::in_dir(first.path());
        let created = store.create("Portable", None).unwrap();
        let exported = store.export().unwrap();
        let value: Value = serde_json::from_str(&exported).unwrap();
        assert_eq!(value["format"], KeyFile::FORMAT);
        assert_eq!(value["id"], created.id().to_string());

        let second = TempDir::new("import");
        let other = KeyStore::in_dir(second.path());
        let imported = other
            .import(KeyFile::parse(&exported).unwrap(), &mut never)
            .unwrap();
        assert_eq!(imported.id(), created.id());
        assert_eq!(other.load().unwrap().id(), created.id());
    }

    #[test]
    fn fixture_key_files_yield_the_fixture_ids() {
        let fixture = shared_fixture();
        let plain = &fixture["keyFiles"]["plain"]["file"];
        let protected = &fixture["keyFiles"]["passphrase"];

        let dir = TempDir::new("fixture-plain");
        let store = KeyStore::in_dir(dir.path());
        let key = store
            .import(KeyFile::parse(&plain.to_string()).unwrap(), &mut never)
            .unwrap();
        assert_eq!(key.id().to_string(), plain["id"].as_str().unwrap());
        assert_eq!(
            store.load().unwrap().id().to_string(),
            plain["id"].as_str().unwrap()
        );

        let dir = TempDir::new("fixture-protected");
        let store = KeyStore::in_dir(dir.path());
        let passphrase = protected["passphrase"].as_str().unwrap();
        let key = store
            .import(
                KeyFile::parse(&protected["file"].to_string()).unwrap(),
                &mut fixed(passphrase),
            )
            .unwrap();
        let expected = protected["file"]["id"].as_str().unwrap();
        assert_eq!(key.id().to_string(), expected);
        assert_eq!(expected, fixture["ids"][0]["id"].as_str().unwrap());
        let identity = unlock(&store.load().unwrap(), &mut fixed(passphrase)).unwrap();
        assert_eq!(identity.id().to_string(), expected);

        // A wrong passphrase at import stores nothing.
        let dir = TempDir::new("fixture-wrong");
        let store = KeyStore::in_dir(dir.path());
        assert!(store
            .import(
                KeyFile::parse(&protected["file"].to_string()).unwrap(),
                &mut fixed("incorrect horse battery staple")
            )
            .is_err());
        assert!(!store.exists());
    }

    #[test]
    fn key_override_reads_a_plain_file() {
        let fixture = shared_fixture();
        let plain = &fixture["keyFiles"]["plain"]["file"];
        let dir = TempDir::new("override");
        let path = dir.path().join("dev.bpkey");
        std::fs::write(&path, plain.to_string()).unwrap();
        let identity = open_identity(Some(&path)).unwrap();
        assert_eq!(identity.id().to_string(), plain["id"].as_str().unwrap());

        // A DPAPI-wrapped file is not a plain key file.
        let store = KeyStore::in_dir(dir.path());
        store
            .import(KeyFile::parse(&plain.to_string()).unwrap(), &mut never)
            .unwrap();
        assert!(open_identity(Some(store.path())).is_err());
    }

    #[test]
    #[test]
    #[test]
    #[test]
    #[test]
    #[test]
    #[test]
    #[test]
    #[test]
    #[test]
    #[test]
    fn export_of_a_current_protected_key_asks_for_nothing() {
        let dir = TempDir::new("export-current-protected");
        let store = KeyStore::in_dir(dir.path());
        store.create("Locked", Some("right horse")).unwrap();
        let exported = store.export().unwrap();
        assert!(KeyFile::parse(&exported).unwrap().is_protected());
    }

    #[test]
    fn utc_dates() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(11_016 * 86_400 + 3_661), "2000-02-29T01:01:01Z");
        assert_eq!(format_utc(20_739 * 86_400 + 34_200), "2026-10-13T09:30:00Z");
    }
}
