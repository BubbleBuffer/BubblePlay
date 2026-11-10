//! Non-UI PC key storage for the tray. The file is a raw CurrentUser DPAPI blob of a plain
//! `.bpkey` file. Failed loads preserve the file; callers can launch the host without a key. A
//! stored file in another format counts as no key and is replaced by a new one.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use bp_device_key::{DeviceKeyError, KeyFile};
use bubbleplay_host::dpapi::dpapi_unprotect_bytes_with_migration;
use rand_core::{OsRng, RngCore};
use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
use windows::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};
use zeroize::Zeroizing;

use super::platform::{dpapi_protect_bytes, wide};

pub const HANDOFF_ENV: &str = bubbleplay_host::device_key::HANDOFF_ENV;
const FILE_NAME: &str = "device-key.bpkey";

pub fn path_beside(host_config: &Path) -> PathBuf {
    host_config.with_file_name(FILE_NAME)
}

/// Load or create the PC key and return validated plain JSON for the child command only.
/// Existing files are never replaced by this operation, including a file created concurrently,
/// except a validated legacy DPAPI blob is migrated with application entropy, or a file that
/// unprotects to a key file in another format: that counts as no key.
pub fn handoff(path: &Path, label: &str, created: &str) -> Result<Zeroizing<String>> {
    handoff_with_migrating_codec(
        path,
        label,
        created,
        dpapi_protect_bytes,
        dpapi_unprotect_bytes_with_migration,
    )
}

#[cfg(test)]
fn handoff_with_codec(
    path: &Path,
    label: &str,
    created: &str,
    protect: impl Fn(&[u8]) -> Result<Vec<u8>>,
    unprotect: impl Fn(&[u8]) -> Result<Vec<u8>>,
) -> Result<Zeroizing<String>> {
    handoff_with_migrating_codec(path, label, created, protect, |bytes| {
        unprotect(bytes).map(|plain| (plain, false))
    })
}

fn handoff_with_migrating_codec(
    path: &Path,
    label: &str,
    created: &str,
    protect: impl Fn(&[u8]) -> Result<Vec<u8>>,
    unprotect: impl Fn(&[u8]) -> Result<(Vec<u8>, bool)>,
) -> Result<Zeroizing<String>> {
    let mut replace = false;
    let stored = match fs::read(path) {
        Ok(bytes) => {
            if is_other_format(&bytes, &|bytes| unprotect(bytes).map(|(plain, _)| plain)) {
                tracing::warn!("the stored PC key is in an unsupported format; creating a new one");
                replace = true;
                None
            } else {
                Some(bytes)
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("reading PC key; existing file preserved"),
    };
    let protected = match stored {
        Some(bytes) => bytes,
        None => {
            let key = KeyFile::create(label, created, None).context("creating PC key")?;
            let plain = Zeroizing::new(key.to_json().context("serializing PC key")?);
            let protected = protect(plain.as_bytes()).context("protecting PC key")?;
            write_atomic(path, &protected, replace)?;
            // The common path below verifies the actual stored DPAPI blob, even on first use.
            fs::read(path).context("reading newly created PC key")?
        }
    };
    let (plain, legacy) = unprotect(&protected).context("unprotecting PC key")?;
    let bytes = Zeroizing::new(plain);
    let text = std::str::from_utf8(&bytes).context("PC key is not UTF-8")?;
    validate_plain(text)?;
    if legacy {
        let migrated =
            protect(&bytes).context("protecting legacy PC key with application entropy")?;
        write_atomic(path, &migrated, true).context("migrating legacy PC key")?;
    }
    Ok(Zeroizing::new(text.to_owned()))
}

/// Accepts a plain key in the current format. Errors can contain unknown JSON field names, so the
/// message is fixed and keeps those untrusted values out of logs.
fn validate_plain(text: &str) -> Result<()> {
    let invalid = || anyhow!("PC key is not a valid plain device key file");
    let file = KeyFile::parse(text).map_err(|_| invalid())?;
    if file.is_protected() {
        return Err(invalid());
    }
    file.unlock(None).map(|_| ()).map_err(|_| invalid())
}

/// Replaces the PC key file with the plain key `text` after validating it, as one protected file.
/// Invalid input or a failed protect/write leaves the existing file intact.
pub fn replace_pc_key(path: &Path, text: &str) -> Result<()> {
    replace_with_codec(path, text, dpapi_protect_bytes)
}

fn replace_with_codec(
    path: &Path,
    text: &str,
    protect: impl Fn(&[u8]) -> Result<Vec<u8>>,
) -> Result<()> {
    validate_plain(text)?;
    let protected = protect(text.as_bytes()).context("protecting replacement PC key")?;
    write_atomic(path, &protected, true)?;
    Ok(())
}

/// True when the blob unprotects to a key file whose format is not the current one.
fn is_other_format(protected: &[u8], unprotect: &impl Fn(&[u8]) -> Result<Vec<u8>>) -> bool {
    unprotect_text(protected, unprotect).is_some_and(|text| {
        matches!(
            KeyFile::parse(&text),
            Err(DeviceKeyError::UnsupportedFormat)
        )
    })
}

/// The plain text inside a stored blob, or `None` when it does not unprotect or is not UTF-8.
fn unprotect_text(
    protected: &[u8],
    unprotect: &impl Fn(&[u8]) -> Result<Vec<u8>>,
) -> Option<Zeroizing<String>> {
    let bytes = Zeroizing::new(unprotect(protected).ok()?);
    std::str::from_utf8(&bytes)
        .ok()
        .map(|text| Zeroizing::new(text.to_owned()))
}

/// Renames a key file that cannot be read to `device-key.bpkey.unreadable-<UTC time>` in the same
/// folder. The bytes are kept, never deleted; a copy already under that name is not overwritten.
/// Logs the new path only, never the contents.
pub fn quarantine_unreadable(path: &Path) -> Result<PathBuf> {
    let stamp = now_rfc3339().replace(['-', ':'], "");
    let base = format!("{FILE_NAME}.unreadable-{stamp}");
    let mut target = path.with_file_name(&base);
    let mut copy = 2;
    while target.exists() {
        target = path.with_file_name(format!("{base}-{copy}"));
        copy += 1;
    }
    fs::rename(path, &target).context("renaming unreadable PC key")?;
    tracing::warn!(path = %target.display(), "unreadable PC key renamed before import");
    Ok(target)
}

/// Moves a file made by `quarantine_unreadable` back to `path`. Used only when the replacement
/// failed, so that no PC key is silently created in its place.
pub fn restore_quarantined(quarantined: &Path, path: &Path) -> Result<()> {
    fs::rename(quarantined, path).context("restoring unreadable PC key")
}

struct OwnedTemp(PathBuf);

impl Drop for OwnedTemp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Sync a unique temporary file, close it, then rename within the same directory. First creation
/// refuses replacement; explicit import uses replacement. Cleanup only touches our own temp file.
fn write_atomic(path: &Path, bytes: &[u8], replace: bool) -> Result<bool> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).context("creating PC key directory")?;
    }
    let mut random = [0; 8];
    OsRng
        .try_fill_bytes(&mut random)
        .map_err(|_| anyhow::anyhow!("creating PC key temporary name failed"))?;
    let temp = path.with_file_name(format!(
        "{FILE_NAME}.{}.{:016x}.tmp",
        std::process::id(),
        u64::from_le_bytes(random)
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .context("creating PC key temporary file")?;
    let owned = OwnedTemp(temp);
    let written = (|| -> Result<()> {
        file.write_all(bytes)
            .context("writing PC key temporary file")?;
        file.sync_all().context("syncing PC key temporary file")
    })();
    drop(file);
    written?;
    let source = wide(owned.0.as_os_str());
    let target = wide(path.as_os_str());
    let flags = if replace {
        MOVEFILE_WRITE_THROUGH | MOVEFILE_REPLACE_EXISTING
    } else {
        MOVEFILE_WRITE_THROUGH
    };
    // SAFETY: both null-terminated path buffers remain live through this synchronous call.
    match unsafe { MoveFileExW(PCWSTR(source.as_ptr()), PCWSTR(target.as_ptr()), flags) } {
        Ok(()) => Ok(true),
        Err(error)
            if !replace
                && (error.code() == HRESULT::from_win32(ERROR_ALREADY_EXISTS.0)
                    || error.code() == HRESULT::from_win32(ERROR_FILE_EXISTS.0)) =>
        {
            Ok(false)
        }
        Err(error) => Err(error).context("renaming PC key temporary file"),
    }
}
/// The current time as RFC 3339 UTC, the `created` field of a new key.
pub fn now_rfc3339() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    format_utc(i64::try_from(seconds).unwrap_or(i64::MAX))
}

/// The computer name used as the label of a new key.
pub fn default_label() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "BubblePlay PC".to_owned())
}

/// Formats Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (proleptic Gregorian, UTC).
fn format_utc(unix_seconds: i64) -> String {
    let days = unix_seconds.div_euclid(86_400);
    let second_of_day = unix_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3_600,
        second_of_day % 3_600 / 60,
        second_of_day % 60
    )
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's civil_from_days algorithm.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bubbleplay_host::dpapi::dpapi_unprotect_bytes;
    use std::cell::Cell;

    const CREATED: &str = "2026-10-13T09:30:00Z";
    const PREFIX: &[u8] = b"test-codec:";

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let mut random = [0; 8];
            OsRng.try_fill_bytes(&mut random).unwrap();
            let path = std::env::temp_dir().join(format!(
                "bubbleplay-pc-key-{}-{:016x}",
                std::process::id(),
                u64::from_le_bytes(random)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn key(&self) -> PathBuf {
            self.0.join(FILE_NAME)
        }
        fn no_temps(&self) {
            assert!(fs::read_dir(&self.0).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")));
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn seal(bytes: &[u8]) -> Result<Vec<u8>> {
        Ok([PREFIX, bytes].concat())
    }
    fn open(bytes: &[u8]) -> Result<Vec<u8>> {
        bytes
            .strip_prefix(PREFIX)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| anyhow::anyhow!("injected unprotect failure"))
    }
    fn fail(_: &[u8]) -> Result<Vec<u8>> {
        Err(anyhow::anyhow!("injected protection failure"))
    }
    fn plain(label: &str) -> String {
        KeyFile::create(label, CREATED, None)
            .unwrap()
            .to_json()
            .unwrap()
    }

    #[test]
    fn formats_known_unix_times_as_utc() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(format_utc(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn dpapi_round_trips_raw_bytes_for_this_user() {
        let protected = dpapi_protect_bytes(b"device key round trip").unwrap();
        assert!(!protected.starts_with(b"device key"));
        assert_eq!(
            dpapi_unprotect_bytes(&protected).unwrap(),
            b"device key round trip"
        );
    }

    #[test]
    fn legacy_pc_key_is_migrated_once_after_validation() {
        let dir = Scratch::new();
        let path = dir.key();
        let original = plain("legacy PC");
        let legacy = [b"legacy:".as_slice(), original.as_bytes()].concat();
        fs::write(&path, &legacy).unwrap();
        let unprotect = |bytes: &[u8]| {
            if let Some(plain) = bytes.strip_prefix(b"legacy:") {
                Ok((plain.to_vec(), true))
            } else {
                open(bytes).map(|plain| (plain, false))
            }
        };
        assert!(handoff_with_migrating_codec(&path, "PC", CREATED, fail, unprotect).is_err());
        assert_eq!(fs::read(&path).unwrap(), legacy);
        let first =
            handoff_with_migrating_codec(&path, "unused", CREATED, seal, unprotect).unwrap();
        assert_eq!(first.as_str(), original);
        let migrated = fs::read(&path).unwrap();
        assert_eq!(migrated, seal(original.as_bytes()).unwrap());
        let second =
            handoff_with_migrating_codec(&path, "unused", CREATED, fail, unprotect).unwrap();
        assert_eq!(first.as_str(), second.as_str());
        assert_eq!(fs::read(&path).unwrap(), migrated);
        dir.no_temps();

        let invalid = b"legacy:{not JSON";
        fs::write(&path, invalid).unwrap();
        assert!(handoff_with_migrating_codec(&path, "PC", CREATED, seal, unprotect).is_err());
        assert_eq!(fs::read(&path).unwrap(), invalid);
    }

    #[test]
    fn create_unprotects_the_stored_file_and_load_reuses_it() {
        let dir = Scratch::new();
        let path = dir.0.join("new-directory").join(FILE_NAME);
        let calls = Cell::new(0);
        let first = handoff_with_codec(&path, "PC ü 東京", CREATED, seal, |bytes| {
            calls.set(calls.get() + 1);
            open(bytes)
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(KeyFile::parse(&first).unwrap().label(), "PC ü 東京");
        let stored = fs::read(&path).unwrap();
        assert_eq!(stored, seal(first.as_bytes()).unwrap());
        let second = handoff_with_codec(&path, "changed name", CREATED, fail, open).unwrap();
        assert_eq!(first.as_str(), second.as_str());
        assert_eq!(fs::read(path).unwrap(), stored);
    }

    #[test]
    fn corrupt_and_invalid_files_are_preserved() {
        let protected_fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/shared/device-keys-contract.json"
        ))
        .unwrap();
        let protected_json = protected_fixture["keyFiles"]["passphrase"]["file"].to_string();
        for bytes in [
            b"corrupt blob".to_vec(),
            seal(b"{not JSON").unwrap(),
            seal(&[0xff]).unwrap(),
            seal(protected_json.as_bytes()).unwrap(),
        ] {
            let dir = Scratch::new();
            let path = dir.key();
            fs::write(&path, &bytes).unwrap();
            assert!(handoff_with_codec(&path, "PC", CREATED, fail, open).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
            dir.no_temps();
        }
    }

    #[test]
    fn an_unprotect_failure_preserves_the_file() {
        let dir = Scratch::new();
        let path = dir.key();
        let bytes = seal(plain("PC").as_bytes()).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert!(handoff_with_codec(&path, "PC", CREATED, seal, fail).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn a_protect_failure_does_not_create_a_file() {
        let dir = Scratch::new();
        assert!(handoff_with_codec(&dir.key(), "PC", CREATED, fail, open).is_err());
        assert!(!dir.key().exists());
        dir.no_temps();
    }

    #[test]
    fn a_file_appearing_during_creation_is_never_overwritten() {
        let dir = Scratch::new();
        let path = dir.key();
        assert!(handoff_with_codec(
            &path,
            "PC",
            CREATED,
            |bytes| {
                fs::write(&path, b"concurrent corrupt file")?;
                seal(bytes)
            },
            open
        )
        .is_err());
        assert_eq!(fs::read(&path).unwrap(), b"concurrent corrupt file");
        dir.no_temps();
    }

    #[test]
    fn replacement_validates_protects_and_loads_the_new_key() {
        let dir = Scratch::new();
        let path = dir.key();
        handoff_with_codec(&path, "old", CREATED, seal, open).unwrap();
        let new = plain("new");
        replace_with_codec(&path, &new, seal).unwrap();
        let loaded = handoff_with_codec(&path, "unused", CREATED, fail, open).unwrap();
        assert_eq!(loaded.as_str(), new);
        dir.no_temps();
    }

    #[test]
    fn invalid_or_unprotectable_replacements_preserve_the_old_key() {
        let dir = Scratch::new();
        let path = dir.key();
        let old = b"old blob";
        fs::write(&path, old).unwrap();
        assert!(replace_with_codec(&path, "{invalid", seal).is_err());
        assert_eq!(fs::read(&path).unwrap(), old);
        assert!(replace_with_codec(&path, &plain("new"), fail).is_err());
        assert_eq!(fs::read(&path).unwrap(), old);
        dir.no_temps();
    }

    #[test]
    fn a_directory_at_the_key_path_and_a_failed_rename_are_preserved() {
        let dir = Scratch::new();
        let path = dir.key();
        fs::create_dir(&path).unwrap();
        assert!(handoff_with_codec(&path, "PC", CREATED, seal, open).is_err());
        assert!(replace_with_codec(&path, &plain("PC"), seal).is_err());
        assert!(path.is_dir());
        dir.no_temps();
    }

    fn other_format() -> String {
        let mut file: serde_json::Value = serde_json::from_str(&plain("old")).unwrap();
        file["format"] = serde_json::Value::from("other-format");
        file.to_string()
    }

    #[test]
    fn a_key_in_another_format_is_replaced_by_a_new_one() {
        let dir = Scratch::new();
        let path = dir.key();
        let old = other_format();
        fs::write(&path, seal(old.as_bytes()).unwrap()).unwrap();
        let handed = handoff_with_codec(&path, "PC", CREATED, seal, open).unwrap();
        let new = KeyFile::parse(&handed).unwrap();
        assert_eq!(new.label(), "PC");
        let stored = String::from_utf8(open(&fs::read(&path).unwrap()).unwrap()).unwrap();
        assert_eq!(stored, handed.as_str());
        assert_ne!(stored, old);
        assert_eq!(
            fs::read_dir(&dir.0).unwrap().count(),
            1,
            "no backup is kept"
        );
        dir.no_temps();
    }

    #[test]
    fn a_failed_protection_leaves_a_key_in_another_format_untouched() {
        let dir = Scratch::new();
        let path = dir.key();
        let before = seal(other_format().as_bytes()).unwrap();
        fs::write(&path, &before).unwrap();
        assert!(handoff_with_codec(&path, "PC", CREATED, fail, open).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        dir.no_temps();
    }
}
