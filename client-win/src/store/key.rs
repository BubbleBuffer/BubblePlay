//! The viewer's device key: create, import, export, show and unlock.
//!
//! At rest the key is `%LOCALAPPDATA%\BubblePlay\viewer\device-key.bpkey`: the portable
//! `.bpkey` JSON, wrapped as a CurrentUser DPAPI blob. A key with a passphrase is unlocked on
//! the console when a session opens it; a key that cannot be unlocked means no connection.
//! `--key <file.bpkey>` replaces the stored key with a plain (not DPAPI-wrapped) file for
//! development.
//!
//! Only the shared `bubblebuffer-device-key` format is written. A key stored in the old
//! `bubbleplay-device-key` format is converted the first time it is unlocked. The old blob is
//! kept as `device-key.bpkey.legacy-backup` until the new blob has been read back and checked.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use bp_device_key::legacy::LegacyKeyFile;
use bp_device_key::{
    convert_legacy, read_key_file, DeviceId, DeviceIdentity, DeviceKeyError, KeyFile, StoredKeyFile,
};
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

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Reads, unwraps and parses the stored key in either format. The key stays locked.
    pub fn load(&self) -> Result<StoredKeyFile> {
        let wrapped = self.read_wrapped()?;
        let plain = Zeroizing::new(dpapi::unprotect(&wrapped)?);
        let text = std::str::from_utf8(&plain).context("the stored key is not UTF-8")?;
        read_key_file(text).map_err(|error| anyhow!("the stored key is invalid: {error}"))
    }

    /// Wraps and writes `key`. Refuses to replace an existing key: that would lose the
    /// viewer's identity, so the user exports and removes it on purpose first.
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
        if self.exists() {
            bail!("a viewer key already exists at {}", self.path.display());
        }
        let key = KeyFile::create(label, &utc_now(), passphrase)?;
        self.save_new(&key)?;
        Ok(key)
    }

    /// Imports a `.bpkey` file in either format and stores it in the shared format. A protected
    /// key is unlocked once first (a key in the old format is converted, which checks it the same
    /// way), so a file whose seed does not match its public key and ID is refused before it is
    /// stored.
    pub fn import(
        &self,
        stored: StoredKeyFile,
        passphrase: PassphraseSource<'_>,
    ) -> Result<KeyFile> {
        if self.exists() {
            bail!("a viewer key already exists at {}", self.path.display());
        }
        let secret = passphrase_for(&stored, passphrase)?;
        let secret = secret.as_ref().map(|s| s.as_str());
        let key = match stored {
            StoredKeyFile::Current(file) => {
                unlocked(file.id(), file.unlock(secret))?;
                file
            }
            StoredKeyFile::Legacy(legacy) => {
                let (converted, json) = unlocked(legacy.id(), convert_legacy(&legacy, secret))?;
                let _json = Zeroizing::new(json);
                converted
            }
        };
        self.save_new(&key)?;
        Ok(key)
    }

    /// The portable `.bpkey` text of the stored key, always in the shared format (DPAPI removed).
    /// A protected key in the old format asks for its passphrase, through `passphrase` with the
    /// key ID, and is converted in memory. The stored file is not changed.
    pub fn export(
        &self,
        passphrase: &mut dyn FnMut(DeviceId) -> Result<Zeroizing<String>>,
    ) -> Result<Zeroizing<String>> {
        match self.load()? {
            StoredKeyFile::Current(file) => Ok(Zeroizing::new(file.to_json()?)),
            StoredKeyFile::Legacy(legacy) => {
                let secret = if legacy.is_protected() {
                    Some(passphrase(legacy.id())?)
                } else {
                    None
                };
                let secret = secret.as_ref().map(|s| s.as_str());
                let (_, json) = unlocked(legacy.id(), convert_legacy(&legacy, secret))?;
                Ok(Zeroizing::new(json))
            }
        }
    }

    /// Unlocks the stored key for a session. A key in the old format is converted once, right
    /// after it has unlocked (a protected one after its passphrase was accepted). A failed
    /// conversion is logged and the session goes on with the old key.
    pub fn unlock_stored(
        &self,
        key: &StoredKeyFile,
        passphrase: PassphraseSource<'_>,
    ) -> Result<DeviceIdentity> {
        let secret = passphrase_for(key, passphrase)?;
        let secret = secret.as_ref().map(|s| s.as_str());
        let signing = unlocked(key.id(), key.unlock(secret))?;
        if let StoredKeyFile::Legacy(legacy) = key {
            self.migrate(legacy, secret);
        }
        Ok(DeviceIdentity::from_signing_key(signing))
    }

    /// Converts the stored old-format key. Logs a failure on one line, without secrets.
    fn migrate(&self, legacy: &LegacyKeyFile, passphrase: Option<&str>) {
        if let Err(error) = self.migrate_with(legacy, passphrase, dpapi::protect) {
            eprintln!(
                "BubblePlay: viewer key {} stays in the old format: {error:#}",
                legacy.id()
            );
        }
    }

    /// Writes the converted key in place of the old blob, after keeping the old blob as a
    /// backup. The backup is removed once the new blob reads back and matches the old key. On
    /// any failure after the backup exists, the old blob is written back first.
    fn migrate_with(
        &self,
        legacy: &LegacyKeyFile,
        passphrase: Option<&str>,
        wrap: fn(&[u8]) -> Result<Vec<u8>>,
    ) -> Result<()> {
        let old = self.read_wrapped()?;
        let (_, json) = convert_legacy(legacy, passphrase)?;
        let json = Zeroizing::new(json);
        let backup = self.backup_path();
        super::write_replacing(&backup, &old).context("write the legacy backup")?;

        let outcome = self
            .write_converted(&json, wrap)
            .and_then(|()| self.check_converted(legacy, passphrase));
        if let Err(error) = outcome {
            if let Err(restore) = super::write_replacing(&self.path, &old) {
                return Err(error.context(format!(
                    "the old blob could not be restored ({restore:#}); it is kept at {}",
                    backup.display()
                )));
            }
            drop_backup(&backup);
            return Err(error);
        }
        drop_backup(&backup);
        Ok(())
    }

    fn write_converted(&self, json: &str, wrap: fn(&[u8]) -> Result<Vec<u8>>) -> Result<()> {
        let wrapped = wrap(json.as_bytes())?;
        super::write_replacing(&self.path, &wrapped)
    }

    /// Reads the new blob back. It must be in the shared format with the old key's ID and public
    /// key, still protected when the old key was, and unlock with the same passphrase.
    fn check_converted(&self, legacy: &LegacyKeyFile, passphrase: Option<&str>) -> Result<()> {
        let StoredKeyFile::Current(file) = self.load()? else {
            bail!("the converted key is not in the shared format");
        };
        if file.id() != legacy.id()
            || file.public_key() != legacy.public_key()
            || file.is_protected() != legacy.is_protected()
        {
            bail!("the converted key has a different identity");
        }
        let signing = unlocked(file.id(), file.unlock(passphrase))?;
        if signing.verifying_key().to_bytes() != *legacy.public_key() {
            bail!("the converted key has a different public key");
        }
        Ok(())
    }

    /// The stored DPAPI blob as it is on disk.
    fn read_wrapped(&self) -> Result<Vec<u8>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => bail!(
                "no viewer key at {}: run `bubbleplay-client key create` or `key import <file>`",
                self.path.display()
            ),
            Err(error) => Err(error).with_context(|| format!("read {}", self.path.display())),
        }
    }

    /// Where the old blob is kept during a conversion: `device-key.bpkey.legacy-backup`.
    fn backup_path(&self) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(".legacy-backup");
        PathBuf::from(name)
    }
}

/// Removes a conversion backup. A failure is logged: the key itself is already in place.
fn drop_backup(backup: &Path) {
    if let Err(error) = std::fs::remove_file(backup) {
        if error.kind() != std::io::ErrorKind::NotFound {
            eprintln!("BubblePlay: could not remove {}: {error}", backup.display());
        }
    }
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
    key: &StoredKeyFile,
    passphrase: PassphraseSource<'_>,
) -> Result<Option<Zeroizing<String>>> {
    if key.is_protected() {
        passphrase().map(Some)
    } else {
        Ok(None)
    }
}

fn parse(text: &str) -> Result<StoredKeyFile> {
    read_key_file(text).map_err(|error| anyhow!("not a usable .bpkey file: {error}"))
}

/// Unlocks `key` in either format, asking `passphrase` only when the key is protected.
pub fn unlock(key: &StoredKeyFile, passphrase: PassphraseSource<'_>) -> Result<DeviceIdentity> {
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
            store.unlock_stored(&key, &mut || super::console::read_passphrase(&prompt))
        }
    }
}

/// Reads a portable (not DPAPI-wrapped) `.bpkey` file, in either format. Nothing is converted.
pub fn read_plain(path: &Path) -> Result<StoredKeyFile> {
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

    /// BubblePlay's own vectors: the old format under `legacyKeyFiles`.
    fn legacy_fixture() -> Value {
        read_fixture("../fixtures/device-keys-contract.json")
    }

    /// The shared format's vectors: `keyFiles` and `ids`.
    fn shared_fixture() -> Value {
        read_fixture("../fixtures/shared/device-keys-contract.json")
    }

    fn read_fixture(relative: &str) -> Value {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
        serde_json::from_str(&std::fs::read_to_string(path).expect("contract fixture")).unwrap()
    }

    /// An old-format vector: its file text and the whole vector (seed, passphrase, ...).
    fn legacy_text(name: &str) -> (String, Value) {
        let vector = legacy_fixture()["legacyKeyFiles"][name].clone();
        (vector["file"].to_string(), vector)
    }

    /// Stores `text` the way the old build did: in the old format, DPAPI-wrapped.
    fn store_legacy(store: &KeyStore, text: &str) {
        let wrapped = dpapi::protect(text.as_bytes()).unwrap();
        std::fs::write(store.path(), wrapped).unwrap();
    }

    fn fixed(text: &str) -> impl FnMut() -> Result<Zeroizing<String>> + '_ {
        move || Ok(Zeroizing::new(text.to_owned()))
    }

    fn never() -> Result<Zeroizing<String>> {
        panic!("a plain key must not ask for a passphrase")
    }

    fn never_id(_: DeviceId) -> Result<Zeroizing<String>> {
        panic!("this key must not ask for a passphrase")
    }

    fn not_dpapi(_: &[u8]) -> Result<Vec<u8>> {
        Ok(b"not a DPAPI blob".to_vec())
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
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
        assert!(matches!(loaded, StoredKeyFile::Current(_)));
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
        let exported = store.export(&mut never_id).unwrap();
        let value: Value = serde_json::from_str(&exported).unwrap();
        assert_eq!(value["format"], KeyFile::FORMAT);
        assert_eq!(value["id"], created.id().to_string());

        let second = TempDir::new("import");
        let other = KeyStore::in_dir(second.path());
        let imported = other
            .import(read_key_file(&exported).unwrap(), &mut never)
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
            .import(read_key_file(&plain.to_string()).unwrap(), &mut never)
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
                read_key_file(&protected["file"].to_string()).unwrap(),
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
                read_key_file(&protected["file"].to_string()).unwrap(),
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
            .import(read_key_file(&plain.to_string()).unwrap(), &mut never)
            .unwrap();
        assert!(open_identity(Some(store.path())).is_err());
    }

    #[test]
    fn key_override_reads_an_old_plain_file_without_converting_it() {
        let (text, _) = legacy_text("plain");
        let dir = TempDir::new("override-legacy");
        let path = dir.path().join("old.bpkey");
        std::fs::write(&path, &text).unwrap();
        let identity = open_identity(Some(&path)).unwrap();
        assert_eq!(identity.id(), read_key_file(&text).unwrap().id());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn legacy_plain_key_converts_when_it_is_unlocked() {
        let (text, vector) = legacy_text("plain");
        let dir = TempDir::new("legacy-plain");
        let store = KeyStore::in_dir(dir.path());
        store_legacy(&store, &text);
        let before = std::fs::read(store.path()).unwrap();

        let legacy = store.load().unwrap();
        assert!(legacy.is_legacy() && !legacy.is_protected());
        let identity = store.unlock_stored(&legacy, &mut never).unwrap();
        assert_eq!(identity.id(), legacy.id());
        assert!(!store.backup_path().exists());

        let StoredKeyFile::Current(file) = store.load().unwrap() else {
            panic!("the stored key was not converted");
        };
        assert_eq!(file.id(), legacy.id());
        assert_eq!(file.public_key(), legacy.public_key());
        assert!(!file.is_protected());
        let seed = file.unlock(None).unwrap().to_bytes();
        assert_eq!(hex(&seed), vector["seedHex"].as_str().unwrap());
        assert_ne!(std::fs::read(store.path()).unwrap(), before);

        // Once converted, a second unlock changes nothing.
        let converted = std::fs::read(store.path()).unwrap();
        let again = store.load().unwrap();
        store.unlock_stored(&again, &mut never).unwrap();
        assert_eq!(std::fs::read(store.path()).unwrap(), converted);
        assert!(!store.backup_path().exists());
    }

    #[test]
    fn legacy_protected_key_converts_once_its_passphrase_unlocks_it() {
        let (text, vector) = legacy_text("passphrase");
        let passphrase = vector["passphrase"].as_str().unwrap();
        let dir = TempDir::new("legacy-protected");
        let store = KeyStore::in_dir(dir.path());
        store_legacy(&store, &text);
        let before = std::fs::read(store.path()).unwrap();

        let legacy = store.load().unwrap();
        assert!(legacy.is_legacy() && legacy.is_protected());
        // A wrong passphrase leaves the old blob alone.
        assert!(store
            .unlock_stored(&legacy, &mut fixed("wrong horse"))
            .is_err());
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        assert!(!store.backup_path().exists());

        let identity = store
            .unlock_stored(&legacy, &mut fixed(passphrase))
            .unwrap();
        assert_eq!(identity.id(), legacy.id());
        assert!(!store.backup_path().exists());
        let StoredKeyFile::Current(file) = store.load().unwrap() else {
            panic!("the stored key was not converted");
        };
        assert!(file.is_protected());
        assert_eq!(file.id(), legacy.id());
        assert_eq!(file.public_key(), legacy.public_key());
        let seed = file.unlock(Some(passphrase)).unwrap().to_bytes();
        assert_eq!(hex(&seed), vector["seedHex"].as_str().unwrap());
    }

    #[test]
    fn failed_conversion_puts_the_old_blob_back_byte_for_byte() {
        let (text, _) = legacy_text("plain");
        let dir = TempDir::new("failed-convert");
        let store = KeyStore::in_dir(dir.path());
        store_legacy(&store, &text);
        let before = std::fs::read(store.path()).unwrap();
        let StoredKeyFile::Legacy(old) = store.load().unwrap() else {
            panic!("expected a key in the old format");
        };

        // The new blob is not DPAPI, so reading it back fails and the old blob returns.
        assert!(store.migrate_with(&old, None, not_dpapi).is_err());
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        assert!(!store.backup_path().exists());
        assert!(store.load().unwrap().is_legacy());
    }

    #[test]
    fn failed_backup_stops_before_the_old_blob_is_touched() {
        let (text, _) = legacy_text("plain");
        let dir = TempDir::new("failed-backup");
        let store = KeyStore::in_dir(dir.path());
        store_legacy(&store, &text);
        let before = std::fs::read(store.path()).unwrap();
        let StoredKeyFile::Legacy(old) = store.load().unwrap() else {
            panic!("expected a key in the old format");
        };

        // A directory in the backup's place: the backup cannot be written.
        std::fs::create_dir(store.backup_path()).unwrap();
        assert!(store.migrate_with(&old, None, dpapi::protect).is_err());
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
    }

    #[test]
    fn a_current_key_is_left_alone_on_unlock() {
        let dir = TempDir::new("current-untouched");
        let store = KeyStore::in_dir(dir.path());
        store.create("Current", None).unwrap();
        let before = std::fs::read(store.path()).unwrap();
        let stored = store.load().unwrap();
        assert!(!stored.is_legacy());
        store.unlock_stored(&stored, &mut never).unwrap();
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        assert!(!store.backup_path().exists());
    }

    #[test]
    fn importing_an_old_plain_file_stores_the_new_format() {
        let (text, _) = legacy_text("plain");
        let dir = TempDir::new("import-legacy");
        let store = KeyStore::in_dir(dir.path());
        let legacy = read_key_file(&text).unwrap();
        let id = legacy.id();
        let key = store.import(legacy, &mut never).unwrap();
        assert_eq!(key.id(), id);
        assert!(matches!(store.load().unwrap(), StoredKeyFile::Current(_)));

        let exported = store.export(&mut never_id).unwrap();
        let value: Value = serde_json::from_str(&exported).unwrap();
        assert_eq!(value["format"], KeyFile::FORMAT);
        assert_eq!(value["id"], id.to_string());
    }

    #[test]
    fn importing_an_old_protected_file_stores_the_new_format_under_its_passphrase() {
        let (text, vector) = legacy_text("passphrase");
        let passphrase = vector["passphrase"].as_str().unwrap();
        let dir = TempDir::new("import-legacy-protected");
        let store = KeyStore::in_dir(dir.path());
        assert!(store
            .import(read_key_file(&text).unwrap(), &mut fixed("wrong horse"))
            .is_err());
        assert!(!store.exists());

        store
            .import(read_key_file(&text).unwrap(), &mut fixed(passphrase))
            .unwrap();
        let stored = store.load().unwrap();
        assert!(!stored.is_legacy() && stored.is_protected());
        unlock(&stored, &mut fixed(passphrase)).unwrap();
    }

    #[test]
    fn export_of_an_old_plain_key_converts_in_memory_and_leaves_the_file() {
        let (text, _) = legacy_text("plain");
        let dir = TempDir::new("export-legacy");
        let store = KeyStore::in_dir(dir.path());
        store_legacy(&store, &text);
        let before = std::fs::read(store.path()).unwrap();

        let exported = store.export(&mut never_id).unwrap();
        let value: Value = serde_json::from_str(&exported).unwrap();
        assert_eq!(value["format"], KeyFile::FORMAT);
        assert!(!exported.contains("bubbleplay-device-key"));
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
    }

    #[test]
    fn export_of_an_old_protected_key_asks_for_its_passphrase() {
        let (text, vector) = legacy_text("passphrase");
        let passphrase = vector["passphrase"].as_str().unwrap();
        let expected = read_key_file(&text).unwrap().id();
        let dir = TempDir::new("export-legacy-protected");
        let store = KeyStore::in_dir(dir.path());
        store_legacy(&store, &text);

        let exported = store
            .export(&mut |id| {
                assert_eq!(id, expected);
                Ok(Zeroizing::new(passphrase.to_owned()))
            })
            .unwrap();
        let file = KeyFile::parse(&exported).unwrap();
        assert!(file.is_protected());
        file.unlock(Some(passphrase)).unwrap();

        assert!(store
            .export(&mut |_| Ok(Zeroizing::new("wrong horse".to_owned())))
            .is_err());
    }

    #[test]
    fn export_of_a_current_protected_key_asks_for_nothing() {
        let dir = TempDir::new("export-current-protected");
        let store = KeyStore::in_dir(dir.path());
        store.create("Locked", Some("right horse")).unwrap();
        let exported = store.export(&mut never_id).unwrap();
        assert!(KeyFile::parse(&exported).unwrap().is_protected());
    }

    #[test]
    fn utc_dates() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(11_016 * 86_400 + 3_661), "2000-02-29T01:01:01Z");
        assert_eq!(format_utc(20_739 * 86_400 + 34_200), "2026-10-13T09:30:00Z");
    }
}
