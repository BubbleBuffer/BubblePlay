//! The trusted-device list: the viewers this PC has paired with, admitted by ID and key.
//!
//! The file is `%LOCALAPPDATA%\BubblePlay\trusted-devices.json`: the JSON
//! `{"version":1,"devices":[{"id","public","name","paired","lastSeen"}]}` wrapped as one
//! CurrentUser DPAPI blob. A missing file is an empty list. A file that cannot be decrypted or
//! parsed is an error and is never replaced, and `load` fails, so nothing can be mutated on it.
//!
//! Every mutation builds the new list, writes it atomically (a temporary file renamed over the
//! list, with the previous list kept as `trusted-devices.json.bak`) and only then replaces the
//! in-memory list. A failed write leaves both the file and memory as they were, so a failed save
//! never admits or revokes a device.

use std::fs;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

use crate::device_key::DeviceId;
use crate::dpapi::{dpapi_protect_bytes, dpapi_unprotect_bytes};

pub const VERSION: u32 = 1;
/// Longest stored device name, in characters.
pub const NAME_MAX_CHARS: usize = 64;
/// Stored when a device sends no usable name.
pub const UNNAMED_DEVICE: &str = "Unnamed device";
/// The relay rejects a published admit list longer than this, so the trusted list never grows
/// past it: `load` fails on a longer file and `add` refuses the next device.
pub const MAX_DEVICES: usize = 256;
/// `lastSeen` is written at most once per device in this many seconds, counted from the last
/// stored value, so a clock that moves backwards cannot cause repeated writes.
const TOUCH_INTERVAL_SECS: i64 = 5 * 60;
const FILE_NAME: &str = "trusted-devices.json";

/// One paired viewer. Fields are read-only from outside: changes go through [`TrustedDevices`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrustedDevice {
    /// Canonical device ID, `XXXX-XXXX-XXXX-XXXX`, derived from `public`.
    pub id: String,
    /// The Ed25519 public key: 32 bytes, unpadded base64url.
    pub public: String,
    /// Sanitised label: no control characters, trimmed, at most [`NAME_MAX_CHARS`] characters.
    pub name: String,
    /// RFC 3339 UTC in whole seconds: when the device was paired.
    pub paired: String,
    /// RFC 3339 UTC in whole seconds: the last session that touched the device.
    pub last_seen: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListIn {
    version: u32,
    devices: Vec<TrustedDevice>,
}

#[derive(Serialize)]
struct ListOut<'a> {
    version: u32,
    devices: &'a [TrustedDevice],
}

/// The loaded trusted list and the file it is saved to.
#[derive(Debug)]
pub struct TrustedDevices {
    path: PathBuf,
    devices: Vec<TrustedDevice>,
}

/// `%LOCALAPPDATA%\BubblePlay\trusted-devices.json`. Errors when `LOCALAPPDATA` is absent or
/// empty: there is no other user's folder to fall back to.
pub fn default_path() -> Result<PathBuf> {
    default_path_in(std::env::var_os("LOCALAPPDATA"))
}

fn default_path_in(local_app_data: Option<std::ffi::OsString>) -> Result<PathBuf> {
    let local_app_data = local_app_data
        .filter(|value| !value.is_empty())
        .context("LOCALAPPDATA is not set, so the trusted device list has no location")?;
    Ok(PathBuf::from(local_app_data)
        .join("BubblePlay")
        .join(FILE_NAME))
}

/// Loads the list at [`default_path`].
pub fn open_default() -> Result<TrustedDevices> {
    load(default_path()?)
}

/// Loads the list at `path`. A missing file is an empty list and nothing is written. Any other
/// failure is an error, and the file is left as it is.
pub fn load(path: impl AsRef<Path>) -> Result<TrustedDevices> {
    let path = path.as_ref();
    let protected = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TrustedDevices {
                path: path.to_path_buf(),
                devices: Vec::new(),
            });
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("reading {}; the file was left unchanged", path.display())
            })
        }
    };
    let plain = dpapi_unprotect_bytes(&protected).with_context(|| {
        format!(
            "{} cannot be decrypted by this user's DPAPI key; the file was left unchanged",
            path.display()
        )
    })?;
    let file: ListIn = serde_json::from_slice(&plain).with_context(|| {
        format!(
            "{} is not a valid trusted device list; the file was left unchanged",
            path.display()
        )
    })?;
    if file.version != VERSION {
        bail!(
            "{} has trusted device list version {}, expected {VERSION}; the file was left unchanged",
            path.display(),
            file.version
        );
    }
    if file.devices.len() > MAX_DEVICES {
        bail!(
            "{} holds {} devices, more than the {MAX_DEVICES} allowed; the file was left unchanged",
            path.display(),
            file.devices.len()
        );
    }
    let devices = file
        .devices
        .into_iter()
        .map(checked)
        .collect::<Result<Vec<_>>>()
        .with_context(|| {
            format!(
                "{} holds an invalid device entry; the file was left unchanged",
                path.display()
            )
        })?;
    ensure_unique(&devices).with_context(|| {
        format!(
            "{} repeats a device; the file was left unchanged",
            path.display()
        )
    })?;
    Ok(TrustedDevices {
        path: path.to_path_buf(),
        devices,
    })
}

impl TrustedDevices {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn devices(&self) -> &[TrustedDevice] {
        &self.devices
    }

    /// The device with exactly this canonical ID.
    pub fn get(&self, id: &str) -> Option<&TrustedDevice> {
        self.devices.iter().find(|device| device.id == id)
    }

    /// The device only when both its ID and its public key match. A viewer is admitted only on
    /// this match.
    pub fn get_matching(&self, id: &str, public: &str) -> Option<&TrustedDevice> {
        self.get(id).filter(|device| device.public == public)
    }

    /// Adds a paired device with `paired` and `lastSeen` set to `now`, and saves the list.
    /// `id` must be the canonical ID of `public`, and must not already be trusted. The name is
    /// sanitised with [`sanitize_name`]. Returns the stored entry.
    pub fn add(
        &mut self,
        id: &str,
        public: &str,
        name: &str,
        now: SystemTime,
    ) -> Result<TrustedDevice> {
        let canonical = id_for_public(public)?;
        if id != canonical {
            bail!("device ID is not the canonical ID of its public key");
        }
        if self.get(id).is_some() {
            bail!("device is already in the trusted list");
        }
        if self.devices.len() >= MAX_DEVICES {
            bail!("trusted device list is full ({MAX_DEVICES} devices); revoke one first");
        }
        let stamp = rfc3339(unix_seconds(now));
        let device = TrustedDevice {
            id: canonical,
            public: public.to_owned(),
            name: sanitize_name(name),
            paired: stamp.clone(),
            last_seen: stamp,
        };
        let mut next = self.devices.clone();
        next.push(device.clone());
        self.commit(next)?;
        Ok(device)
    }

    /// Removes the device with this ID and saves the list. `Ok(false)` when it is not trusted;
    /// nothing is written then.
    pub fn revoke(&mut self, id: &str) -> Result<bool> {
        let Some(index) = self.position(id) else {
            return Ok(false);
        };
        let mut next = self.devices.clone();
        next.remove(index);
        self.commit(next)?;
        Ok(true)
    }

    /// Sets `lastSeen` to `now` and saves, but only when `now` is at least
    /// [`TOUCH_INTERVAL_SECS`] after the stored `lastSeen`. Any earlier `now`, including one
    /// from a clock that moved backwards, leaves the stored value alone and writes nothing.
    /// Returns `Ok(false)` when the device is unknown or was throttled.
    pub fn touch(&mut self, id: &str, now: SystemTime) -> Result<bool> {
        let Some(index) = self.position(id) else {
            return Ok(false);
        };
        let now = unix_seconds(now);
        let last = parse_rfc3339(&self.devices[index].last_seen).unwrap_or(i64::MIN);
        if now < last.saturating_add(TOUCH_INTERVAL_SECS) {
            return Ok(false);
        }
        let mut next = self.devices.clone();
        next[index].last_seen = rfc3339(now);
        self.commit(next)?;
        Ok(true)
    }

    /// Replaces the stored name with `name` after sanitising it, and saves. `Ok(false)` when
    /// the device is unknown.
    pub fn update_name(&mut self, id: &str, name: &str) -> Result<bool> {
        let Some(index) = self.position(id) else {
            return Ok(false);
        };
        let name = sanitize_name(name);
        if self.devices[index].name == name {
            return Ok(true);
        }
        let mut next = self.devices.clone();
        next[index].name = name;
        self.commit(next)?;
        Ok(true)
    }

    fn position(&self, id: &str) -> Option<usize> {
        self.devices.iter().position(|device| device.id == id)
    }

    /// Writes `devices` to the file, then makes it the in-memory list. On error neither changes.
    fn commit(&mut self, devices: Vec<TrustedDevice>) -> Result<()> {
        let plain = serde_json::to_vec(&ListOut {
            version: VERSION,
            devices: devices.as_slice(),
        })
        .context("serialising the trusted device list")?;
        let protected =
            dpapi_protect_bytes(&plain).context("protecting the trusted device list")?;
        write_atomic(&self.path, &protected)?;
        self.devices = devices;
        Ok(())
    }
}

/// Makes a device name safe to store and show: control characters (Cc) and format characters
/// (Cf: bidi overrides and isolates, zero-width characters, ...) are removed, the rest is trimmed
/// and cut to [`NAME_MAX_CHARS`] characters. An empty result becomes [`UNNAMED_DEVICE`].
pub fn sanitize_name(raw: &str) -> String {
    let stripped: String = raw
        .chars()
        .filter(|&ch| !ch.is_control() && !is_format_char(ch))
        .collect();
    let truncated: String = stripped.trim().chars().take(NAME_MAX_CHARS).collect();
    if truncated.is_empty() {
        UNNAMED_DEVICE.to_owned()
    } else {
        truncated
    }
}

/// Unicode general category Cf (format characters), Unicode 16.0. The table is small and
/// stable, so it is kept here instead of pulling in a Unicode data crate.
fn is_format_char(ch: char) -> bool {
    const CF: &[(u32, u32)] = &[
        (0x00AD, 0x00AD),
        (0x0600, 0x0605),
        (0x061C, 0x061C),
        (0x06DD, 0x06DD),
        (0x070F, 0x070F),
        (0x0890, 0x0891),
        (0x08E2, 0x08E2),
        (0x180E, 0x180E),
        (0x200B, 0x200F),
        (0x202A, 0x202E),
        (0x2060, 0x2064),
        (0x2066, 0x206F),
        (0xFEFF, 0xFEFF),
        (0xFFF9, 0xFFFB),
        (0x110BD, 0x110BD),
        (0x110CD, 0x110CD),
        (0x13430, 0x1343F),
        (0x1BCA0, 0x1BCA3),
        (0x1D173, 0x1D17A),
        (0xE0001, 0xE0001),
        (0xE0020, 0xE007F),
    ];
    let code = u32::from(ch);
    CF.iter().any(|&(low, high)| (low..=high).contains(&code))
}

/// The canonical ID of a public key given as unpadded base64url. The key must be 32 bytes and
/// must re-encode to the same text.
fn id_for_public(public: &str) -> Result<String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(public)
        .context("public key is not unpadded base64url")?;
    let key: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .context("public key is not 32 bytes")?;
    if URL_SAFE_NO_PAD.encode(key) != public {
        bail!("public key is not canonical unpadded base64url");
    }
    Ok(DeviceId::from_public_key(&key).to_string())
}

/// Checks a device read from the file and returns it with its name sanitised.
fn checked(device: TrustedDevice) -> Result<TrustedDevice> {
    if device.id != id_for_public(&device.public)? {
        bail!("device ID does not match its public key");
    }
    if parse_rfc3339(&device.paired).is_none() || parse_rfc3339(&device.last_seen).is_none() {
        bail!("device has a time that is not RFC 3339 UTC");
    }
    Ok(TrustedDevice {
        name: sanitize_name(&device.name),
        ..device
    })
}

fn ensure_unique(devices: &[TrustedDevice]) -> Result<()> {
    for (index, device) in devices.iter().enumerate() {
        let repeated = devices[..index]
            .iter()
            .any(|earlier| earlier.id == device.id || earlier.public == device.public);
        if repeated {
            bail!("trusted device list has the same device twice");
        }
    }
    Ok(())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A new, unused name beside `path` for a temporary copy.
fn unique_temp_beside(path: &Path) -> Result<PathBuf> {
    let mut random = [0u8; 8];
    OsRng
        .try_fill_bytes(&mut random)
        .map_err(|_| anyhow!("creating a temporary name for the trusted device list failed"))?;
    Ok(path.with_file_name(format!(
        "{}.{}.{:016x}.tmp",
        file_name(path),
        std::process::id(),
        u64::from_le_bytes(random)
    )))
}

/// Removes a temporary file when dropped. After a successful rename there is nothing to remove,
/// and the error is ignored.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Writes `bytes` to `path` through a temporary file, keeping the previous file as `.bak`.
///
/// The backup is copied before the list is replaced, so a failed backup stops the save with the
/// list still in place. Renames use `MoveFileExW` with write-through, as the tray's key file does.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).context("creating the trusted device folder")?;
    }
    let temp = Scratch(unique_temp_beside(path)?);
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp.0)
            .context("creating the temporary trusted device file")?;
        file.write_all(bytes)
            .context("writing the temporary trusted device file")?;
        file.sync_all()
            .context("syncing the temporary trusted device file")?;
    }
    if path.exists() {
        let backup_temp = Scratch(unique_temp_beside(path)?);
        fs::copy(path, &backup_temp.0).context("copying the trusted device list to its backup")?;
        let backup = path.with_file_name(format!("{}.bak", file_name(path)));
        replace_file(&backup_temp.0, &backup)?;
    }
    replace_file(&temp.0, path)
}

/// Renames `source` over `target`, replacing it, and waits until the rename is on disk.
fn replace_file(source: &Path, target: &Path) -> Result<()> {
    let source = wide(source);
    let target = wide(target);
    // SAFETY: both null-terminated buffers outlive this synchronous call.
    let result = unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(target.as_ptr()),
            MOVEFILE_WRITE_THROUGH | MOVEFILE_REPLACE_EXISTING,
        )
    };
    result.context("replacing the trusted device list")
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn unix_seconds(now: SystemTime) -> i64 {
    match now.duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// Formats Unix seconds as `YYYY-MM-DDTHH:MM:SSZ`, the only form this module writes.
fn rfc3339(unix_seconds: i64) -> String {
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

/// Parses the form [`rfc3339`] writes. The text must round-trip exactly, which also rejects
/// dates such as February 30 and any other spelling.
fn parse_rfc3339(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let separators_ok = bytes.len() == 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z';
    if !separators_ok {
        return None;
    }
    let field = |range: std::ops::Range<usize>| -> Option<i64> { text.get(range)?.parse().ok() };
    let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
    let (hour, minute, second) = (field(11..13)?, field(14..16)?, field(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    (rfc3339(seconds) == text).then_some(seconds)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A unique scratch folder under the system temp directory, removed on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let mut random = [0u8; 8];
            OsRng.fill_bytes(&mut random);
            let dir = std::env::temp_dir().join(format!(
                "bp-trusted-{tag}-{}-{:016x}",
                std::process::id(),
                u64::from_le_bytes(random)
            ));
            fs::create_dir_all(&dir).expect("creating test folder");
            Self(dir)
        }

        fn file(&self) -> PathBuf {
            self.0.join(FILE_NAME)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn key(seed: u8) -> String {
        URL_SAFE_NO_PAD.encode([seed; 32])
    }

    fn id_of(public: &str) -> String {
        id_for_public(public).expect("canonical id")
    }

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    /// Writes `json` protected with DPAPI, the way a damaged or foreign writer might.
    fn write_protected(file: &Path, json: &str) {
        let protected = dpapi_protect_bytes(json.as_bytes()).expect("protect");
        fs::write(file, protected).expect("write test file");
    }

    fn temp_files(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .expect("read dir")
            .map(|entry| file_name(&entry.expect("entry").path()))
            .filter(|name| name.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn dpapi_round_trips_and_the_saved_file_is_not_plaintext() {
        let protected = dpapi_protect_bytes(b"trusted round trip").expect("protect");
        assert_ne!(protected.as_slice(), b"trusted round trip");
        assert_eq!(
            dpapi_unprotect_bytes(&protected).expect("unprotect"),
            b"trusted round trip"
        );

        let dir = TestDir::new("roundtrip");
        let mut list = load(dir.file()).expect("missing file is empty");
        assert!(list.devices().is_empty());
        assert!(!dir.file().exists(), "loading must not create the file");

        let public = key(1);
        let id = id_of(&public);
        let added = list
            .add(&id, &public, "Living room laptop", at(1_700_000_000))
            .expect("add");
        assert_eq!(added.paired, "2023-11-14T22:13:20Z");
        assert_eq!(added.last_seen, "2023-11-14T22:13:20Z");

        let on_disk = fs::read(dir.file()).expect("read saved file");
        let text = String::from_utf8_lossy(&on_disk);
        assert!(!text.contains("Living room laptop"));
        assert!(!text.contains(&public));

        let reloaded = load(dir.file()).expect("reload");
        assert_eq!(reloaded.devices(), &[added]);
        assert_eq!(
            reloaded.get_matching(&id, &public).map(|d| d.name.as_str()),
            Some("Living room laptop")
        );
        assert!(reloaded.get_matching(&id, &key(2)).is_none());
    }

    #[test]
    fn saves_are_atomic_and_keep_the_previous_list_as_bak() {
        let dir = TestDir::new("bak");
        let mut list = load(dir.file()).expect("load");
        let first = list
            .add(&id_of(&key(1)), &key(1), "First", at(1_700_000_000))
            .expect("add first");
        assert!(!dir.0.join("trusted-devices.json.bak").exists());

        list.add(&id_of(&key(2)), &key(2), "Second", at(1_700_000_100))
            .expect("add second");

        let backup = load(dir.0.join("trusted-devices.json.bak")).expect("backup loads");
        assert_eq!(backup.devices(), &[first]);
        assert_eq!(load(dir.file()).expect("current").devices().len(), 2);
        assert!(
            temp_files(&dir.0).is_empty(),
            "no temporary files may remain"
        );
    }

    #[test]
    fn corrupt_or_undecryptable_files_are_errors_and_are_never_replaced() {
        let dir = TestDir::new("corrupt");
        let file = dir.file();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("plain json", br#"{"version":1,"devices":[]}"#.to_vec()),
            ("garbage ciphertext", vec![1, 2, 3, 4, 5]),
            (
                "protected non-json",
                dpapi_protect_bytes(b"not json").expect("protect"),
            ),
            (
                "unknown version",
                dpapi_protect_bytes(br#"{"version":2,"devices":[]}"#).expect("protect"),
            ),
            (
                "unknown field",
                dpapi_protect_bytes(br#"{"version":1,"devices":[],"extra":true}"#)
                    .expect("protect"),
            ),
        ];
        for (label, bytes) in cases {
            fs::write(&file, &bytes).expect("write case");
            assert!(load(&file).is_err(), "{label} must be an error");
            assert_eq!(
                fs::read(&file).expect("read"),
                bytes,
                "{label} must be left unchanged"
            );
        }

        let public = key(3);
        let mismatched = format!(
            r#"{{"version":1,"devices":[{{"id":"{}","public":"{public}","name":"x","paired":"2023-11-14T22:13:20Z","lastSeen":"2023-11-14T22:13:20Z"}}]}}"#,
            id_of(&key(4))
        );
        write_protected(&file, &mismatched);
        assert!(
            load(&file).is_err(),
            "an ID that does not match its key is corrupt"
        );

        let bad_time = format!(
            r#"{{"version":1,"devices":[{{"id":"{}","public":"{public}","name":"x","paired":"2026-02-30T00:00:00Z","lastSeen":"2023-11-14T22:13:20Z"}}]}}"#,
            id_of(&public)
        );
        write_protected(&file, &bad_time);
        assert!(load(&file).is_err(), "an impossible date is corrupt");
    }

    #[test]
    fn revoke_removes_only_that_device_and_persists() {
        let dir = TestDir::new("revoke");
        let mut list = load(dir.file()).expect("load");
        let keep = list
            .add(&id_of(&key(1)), &key(1), "Keep", at(1_700_000_000))
            .expect("add");
        let drop_id = id_of(&key(2));
        list.add(&drop_id, &key(2), "Drop", at(1_700_000_000))
            .expect("add");

        assert!(list.revoke(&drop_id).expect("revoke"));
        assert_eq!(list.devices(), std::slice::from_ref(&keep));
        assert_eq!(load(dir.file()).expect("reload").devices(), &[keep]);

        let before = fs::read(dir.file()).expect("read");
        assert!(!list.revoke(&drop_id).expect("second revoke"));
        assert_eq!(
            fs::read(dir.file()).expect("read"),
            before,
            "unknown revoke writes nothing"
        );
    }

    #[test]
    fn touch_is_throttled_to_once_per_five_minutes() {
        let dir = TestDir::new("touch");
        let t0 = 1_700_000_000u64;
        let mut list = load(dir.file()).expect("load");
        let id = id_of(&key(1));
        list.add(&id, &key(1), "Laptop", at(t0)).expect("add");

        assert!(!list.touch(&id, at(t0 + 299)).expect("throttled"));
        assert_eq!(
            list.get(&id).expect("device").last_seen,
            "2023-11-14T22:13:20Z"
        );

        assert!(list.touch(&id, at(t0 + 300)).expect("due"));
        assert_eq!(
            list.get(&id).expect("device").last_seen,
            rfc3339(t0 as i64 + 300)
        );
        assert_eq!(
            load(dir.file())
                .expect("reload")
                .get(&id)
                .expect("device")
                .last_seen,
            rfc3339(t0 as i64 + 300)
        );

        assert!(!list.touch(&id, at(t0 + 400)).expect("throttled again"));
        assert!(!list
            .touch("AAAA-AAAA-AAAA-AAAA", at(t0 + 9_000))
            .expect("unknown"));
    }

    #[test]
    fn a_failed_write_neither_admits_nor_revokes() {
        let dir = TestDir::new("failed");
        let mut list = load(dir.file()).expect("load");
        let kept_id = id_of(&key(1));
        let kept = list
            .add(&kept_id, &key(1), "Kept", at(1_700_000_000))
            .expect("add");

        // A folder where the list file should be makes the backup copy fail, so every save fails.
        fs::remove_file(dir.file()).expect("remove list");
        fs::create_dir(dir.file()).expect("block the list path");

        assert!(list
            .add(&id_of(&key(2)), &key(2), "New", at(1_700_000_100))
            .is_err());
        assert_eq!(
            list.devices(),
            std::slice::from_ref(&kept),
            "a failed add must not admit"
        );
        assert!(list.get(&id_of(&key(2))).is_none());

        assert!(list.revoke(&kept_id).is_err());
        assert_eq!(
            list.devices(),
            std::slice::from_ref(&kept),
            "a failed revoke must not remove"
        );

        assert!(list.touch(&kept_id, at(1_700_000_000 + 900)).is_err());
        assert_eq!(
            list.get(&kept_id),
            Some(&kept),
            "a failed touch must not change lastSeen"
        );

        assert!(list.update_name(&kept_id, "Renamed").is_err());
        assert_eq!(list.get(&kept_id).expect("device").name, "Kept");

        fs::remove_dir(dir.file()).expect("unblock");
        assert!(
            temp_files(&dir.0).is_empty(),
            "failed saves clean up their temporary files"
        );
    }

    #[test]
    fn add_rejects_non_canonical_or_mismatched_ids_and_bad_keys() {
        let dir = TestDir::new("add");
        let mut list = load(dir.file()).expect("load");
        let public = key(1);
        let id = id_of(&public);
        assert!(list.add(&id.to_lowercase(), &public, "x", at(0)).is_err());
        assert!(list.add(&id.replace('-', ""), &public, "x", at(0)).is_err());
        assert!(list.add(&id_of(&key(2)), &public, "x", at(0)).is_err());
        assert!(list.add(&id, &format!("{public}="), "x", at(0)).is_err());
        assert!(list
            .add(&id, &URL_SAFE_NO_PAD.encode([1u8; 31]), "x", at(0))
            .is_err());
        assert!(!dir.file().exists(), "rejected adds write nothing");

        list.add(&id, &public, "x", at(0)).expect("add");
        assert!(
            list.add(&id, &public, "again", at(0)).is_err(),
            "no duplicate devices"
        );
        assert_eq!(list.devices().len(), 1);
    }

    #[test]
    fn names_are_sanitised_and_updated_in_the_file() {
        assert_eq!(sanitize_name("  Lap\u{7}top\n\t  "), "Laptop");
        assert_eq!(sanitize_name("\t\r\n"), UNNAMED_DEVICE);
        assert_eq!(sanitize_name(""), UNNAMED_DEVICE);
        let long = "é".repeat(100);
        assert_eq!(sanitize_name(&long).chars().count(), NAME_MAX_CHARS);
        // Bidi overrides and isolates, zero-width characters and the BOM are format characters.
        assert_eq!(sanitize_name("\u{202E}Phone\u{202C}"), "Phone");
        assert_eq!(
            sanitize_name("Lap\u{200B}top\u{2066}\u{2069}\u{200D}\u{FEFF}"),
            "Laptop"
        );
        assert_eq!(sanitize_name(" \u{202E} \u{200B} "), UNNAMED_DEVICE);
        // Stripping happens before the cut, so format characters do not use up the 64.
        let padded = format!("{}{}", "\u{200B}".repeat(10), "a".repeat(100));
        assert_eq!(sanitize_name(&padded), "a".repeat(NAME_MAX_CHARS));

        let dir = TestDir::new("names");
        let mut list = load(dir.file()).expect("load");
        let id = id_of(&key(1));
        list.add(&id, &key(1), "Old\u{1b}[31m name", at(0))
            .expect("add");
        assert_eq!(list.get(&id).expect("device").name, "Old[31m name");

        assert!(list.update_name(&id, "New\u{0}name").expect("rename"));
        assert_eq!(
            load(dir.file())
                .expect("reload")
                .get(&id)
                .expect("device")
                .name,
            "Newname"
        );
        assert!(!list
            .update_name(&id_of(&key(2)), "nobody")
            .expect("unknown"));
    }

    /// A distinct valid key for each `n`, for lists longer than the 256 seeds `key` can give.
    fn key_n(n: u16) -> String {
        let mut bytes = [0u8; 32];
        bytes[..2].copy_from_slice(&n.to_be_bytes());
        URL_SAFE_NO_PAD.encode(bytes)
    }

    fn devices_n(count: u16) -> Vec<TrustedDevice> {
        (0..count)
            .map(|n| {
                let public = key_n(n);
                TrustedDevice {
                    id: id_of(&public),
                    public,
                    name: "Device".to_owned(),
                    paired: "2023-11-14T22:13:20Z".to_owned(),
                    last_seen: "2023-11-14T22:13:20Z".to_owned(),
                }
            })
            .collect()
    }

    /// Writes `devices` as a protected list file, bypassing `add` so a test can build any size.
    fn write_list(file: &Path, devices: &[TrustedDevice]) {
        let plain = serde_json::to_vec(&ListOut {
            version: VERSION,
            devices,
        })
        .expect("serialise");
        fs::write(file, dpapi_protect_bytes(&plain).expect("protect")).expect("write list");
    }

    #[test]
    fn touch_writes_nothing_when_the_clock_moves_backwards() {
        let dir = TestDir::new("clock");
        let t0 = 1_700_000_000u64;
        let mut list = load(dir.file()).expect("load");
        let id = id_of(&key(1));
        list.add(&id, &key(1), "Laptop", at(t0)).expect("add");
        assert!(list.touch(&id, at(t0 + 300)).expect("due"));
        let stored = fs::read(dir.file()).expect("read");
        let last = rfc3339(t0 as i64 + 300);

        for backwards in [t0 + 299, t0 + 100, t0, t0 - 3_600] {
            assert!(!list.touch(&id, at(backwards)).expect("backwards"));
            assert_eq!(
                list.get(&id).expect("device").last_seen,
                last,
                "lastSeen is preserved"
            );
            assert_eq!(
                fs::read(dir.file()).expect("read"),
                stored,
                "a clock moving backwards must not write"
            );
        }

        assert!(!list
            .touch(&id, at(t0 + 599))
            .expect("just short of the interval"));
        assert!(list.touch(&id, at(t0 + 600)).expect("due again"));
        assert_eq!(
            list.get(&id).expect("device").last_seen,
            rfc3339(t0 as i64 + 600)
        );
    }

    #[test]
    fn the_default_path_errors_without_local_app_data() {
        use std::ffi::OsString;
        assert!(default_path_in(None).is_err());
        assert!(default_path_in(Some(OsString::new())).is_err());
        let path =
            default_path_in(Some(OsString::from(r"C:\Users\Tester\AppData\Local"))).expect("path");
        assert_eq!(
            path,
            Path::new(r"C:\Users\Tester\AppData\Local")
                .join("BubblePlay")
                .join(FILE_NAME)
        );
    }

    #[test]
    fn a_list_over_256_devices_is_refused_on_load_and_add() {
        let limit = u16::try_from(MAX_DEVICES).expect("limit fits in u16");
        let dir = TestDir::new("limit");
        let file = dir.file();

        write_list(&file, &devices_n(limit));
        assert_eq!(
            load(&file).expect("256 devices load").devices().len(),
            MAX_DEVICES
        );

        write_list(&file, &devices_n(limit + 1));
        let oversized = fs::read(&file).expect("read");
        assert!(load(&file).is_err(), "257 devices must be refused on load");
        assert_eq!(
            fs::read(&file).expect("read"),
            oversized,
            "the oversized file is left alone"
        );

        write_list(&file, &devices_n(limit));
        let mut list = load(&file).expect("full list loads");
        let full = fs::read(&file).expect("read");
        let public = key_n(limit);
        assert!(list
            .add(&id_of(&public), &public, "Too many", at(0))
            .is_err());
        assert_eq!(list.devices().len(), MAX_DEVICES);
        assert_eq!(
            fs::read(&file).expect("read"),
            full,
            "a refused add writes nothing"
        );
    }

    #[test]
    fn rfc3339_round_trips_and_rejects_other_spellings() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2023-11-14T22:13:20Z"), Some(1_700_000_000));
        assert_eq!(
            parse_rfc3339("2024-02-29T12:00:00Z")
                .map(rfc3339)
                .as_deref(),
            Some("2024-02-29T12:00:00Z")
        );
        for bad in [
            "2026-02-30T00:00:00Z",
            "2026-10-27 12:00:00Z",
            "2026-10-27T12:00:00+00:00",
            "2026-10-27T12:00:60Z",
            "2026-10-27T24:00:00Z",
            "2026-10-27T12:00:00.5Z",
            "+026-10-27T12:00:00Z",
            "",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad} must be rejected");
        }
    }
}
