//! The relay's state files in `state_dir`: `pcs.json` (registered host PCs) and `gate.json`
//! (an opaque value the site gate defines later).
//!
//! Writes are atomic: a temp file in the same directory is fsynced, renamed over the target, and
//! the directory is fsynced on unix. Files are mode 0600 on unix. A corrupt or inconsistent file
//! is an error, never silently replaced by an empty one.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bp_device_key::DeviceId;
use bp_relay_core::limits::ADMIT_VIEWERS_MAX;
use serde::{Deserialize, Serialize};

pub const PCS_FILE: &str = "pcs.json";
pub const GATE_FILE: &str = "gate.json";
pub const PCS_VERSION: u32 = 1;

/// One registered host PC. `id` is the canonical device ID and `public` the unpadded base64url
/// Ed25519 public key whose hash it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PcRecord {
    pub id: String,
    pub public: String,
    pub registered_ms: u64,
    /// The viewer IDs the host admits: canonical, unique, at most ADMIT_VIEWERS_MAX. Absent in
    /// files written before M2, which load as an empty list.
    #[serde(default)]
    pub admitted: Vec<String>,
    /// The host's remote-pairing flag, from its last `admit`.
    #[serde(default, rename = "remotePairing")]
    pub remote_pairing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PcsFile {
    pub version: u32,
    pub pcs: Vec<PcRecord>,
}

impl Default for PcsFile {
    fn default() -> Self {
        Self {
            version: PCS_VERSION,
            pcs: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub enum StoreError {
    Io { path: PathBuf, source: io::Error },
    Corrupt { path: PathBuf, reason: String },
    Invalid(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Corrupt { path, reason } => write!(f, "{} is corrupt: {reason}", path.display()),
            Self::Invalid(reason) => write!(f, "refusing to write invalid state: {reason}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// Decodes an unpadded base64url Ed25519 public key. Non-canonical encodings are rejected.
pub fn decode_public_key(text: &str) -> Option<[u8; 32]> {
    let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
    let public: [u8; 32] = bytes.try_into().ok()?;
    (URL_SAFE_NO_PAD.encode(public) == text).then_some(public)
}

/// Checks the invariants of a `pcs.json` value: known version, canonical IDs, each ID equal to
/// the hash of its own key, and no duplicate IDs.
pub fn validate_pcs(file: &PcsFile) -> Result<(), String> {
    if file.version != PCS_VERSION {
        return Err(format!("unsupported pcs.json version {}", file.version));
    }
    let mut seen = HashSet::new();
    for record in &file.pcs {
        let public = decode_public_key(&record.public).ok_or_else(|| {
            format!(
                "pc {}: public key is not an unpadded base64url 32-byte value",
                record.id
            )
        })?;
        let id = DeviceId::parse(&record.id)
            .map_err(|_| format!("pc {}: id is not a device ID", record.id))?;
        if id.to_string() != record.id {
            return Err(format!("pc {}: id is not in canonical form", record.id));
        }
        if DeviceId::from_public_key(&public) != id {
            return Err(format!(
                "pc {}: id does not match its public key",
                record.id
            ));
        }
        if !seen.insert(record.id.clone()) {
            return Err(format!("pc {}: listed twice", record.id));
        }
        if record.admitted.len() > ADMIT_VIEWERS_MAX {
            return Err(format!(
                "pc {}: more than {ADMIT_VIEWERS_MAX} admitted viewers",
                record.id
            ));
        }
        let mut admitted = HashSet::new();
        for viewer in &record.admitted {
            let canonical = DeviceId::parse(viewer).is_ok_and(|v| v.to_string() == *viewer);
            if !canonical || !admitted.insert(viewer) {
                return Err(format!(
                    "pc {}: admitted viewer {viewer} is not a unique canonical ID",
                    record.id
                ));
            }
        }
    }
    Ok(())
}

/// Loads `pcs.json`. A missing file is an empty table; anything unreadable or invalid is an error.
pub fn load_pcs(state_dir: &Path) -> Result<PcsFile, StoreError> {
    let path = state_dir.join(PCS_FILE);
    let Some(bytes) = read_optional(&path)? else {
        return Ok(PcsFile::default());
    };
    let file: PcsFile = serde_json::from_slice(&bytes).map_err(|e| StoreError::Corrupt {
        path: path.clone(),
        reason: e.to_string(),
    })?;
    validate_pcs(&file).map_err(|reason| StoreError::Corrupt { path, reason })?;
    Ok(file)
}

pub fn save_pcs(state_dir: &Path, file: &PcsFile) -> Result<(), StoreError> {
    validate_pcs(file).map_err(StoreError::Invalid)?;
    write_atomic(state_dir, PCS_FILE, &to_json_bytes(file)?)
}

/// Loads `gate.json`. `None` when the file does not exist yet.
pub fn load_gate(state_dir: &Path) -> Result<Option<serde_json::Value>, StoreError> {
    let path = state_dir.join(GATE_FILE);
    let Some(bytes) = read_optional(&path)? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| StoreError::Corrupt {
            path,
            reason: e.to_string(),
        })
}

pub fn save_gate(state_dir: &Path, value: &serde_json::Value) -> Result<(), StoreError> {
    write_atomic(state_dir, GATE_FILE, &to_json_bytes(value)?)
}

fn to_json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, StoreError> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| StoreError::Invalid(format!("cannot encode state: {e}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(StoreError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), StoreError> {
    let target = dir.join(name);
    let io_error = |source: io::Error, path: &Path| StoreError::Io {
        path: path.to_path_buf(),
        source,
    };
    fs::create_dir_all(dir).map_err(|source| io_error(source, dir))?;
    if let Ok(meta) = fs::symlink_metadata(&target)
        && !meta.is_file()
    {
        return Err(io_error(
            io::Error::other("state file must be a regular file"),
            &target,
        ));
    }
    let suffix = random_suffix().map_err(|source| io_error(source, dir))?;
    let temp = dir.join(format!(".{name}.tmp-{suffix}"));
    let result = write_temp_then_rename(&temp, &target, dir, bytes);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(|source| io_error(source, &target))
}

fn write_temp_then_rename(temp: &Path, target: &Path, dir: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = create_private(temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temp, target)?;
    sync_dir(dir)
}

fn create_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

fn random_suffix() -> io::Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record(seed: u8) -> PcRecord {
        let public = [seed; 32];
        PcRecord {
            id: DeviceId::from_public_key(&public).to_string(),
            public: URL_SAFE_NO_PAD.encode(public),
            registered_ms: 1_700_000_000_000 + u64::from(seed),
            admitted: vec![DeviceId::from_public_key(&[seed.wrapping_add(1); 32]).to_string()],
            remote_pairing: seed.is_multiple_of(2),
        }
    }

    fn sample_file(records: &[PcRecord]) -> PcsFile {
        PcsFile {
            version: PCS_VERSION,
            pcs: records.to_vec(),
        }
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn save_then_reload_round_trips_pcs() {
        let dir = tempfile::tempdir().unwrap();
        let file = sample_file(&[sample_record(1), sample_record(2)]);
        save_pcs(dir.path(), &file).unwrap();
        assert_eq!(load_pcs(dir.path()).unwrap(), file);
    }

    #[test]
    fn second_save_replaces_the_first() {
        let dir = tempfile::tempdir().unwrap();
        save_pcs(dir.path(), &sample_file(&[sample_record(1)])).unwrap();
        let newer = sample_file(&[sample_record(3)]);
        save_pcs(dir.path(), &newer).unwrap();
        assert_eq!(load_pcs(dir.path()).unwrap(), newer);
    }

    #[test]
    fn save_leaves_only_the_state_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        save_pcs(dir.path(), &sample_file(&[sample_record(4)])).unwrap();
        save_gate(dir.path(), &serde_json::json!({"a": 1})).unwrap();
        assert_eq!(
            entries(dir.path()),
            vec![GATE_FILE.to_string(), PCS_FILE.to_string()]
        );
    }

    #[test]
    fn missing_files_load_as_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_pcs(dir.path()).unwrap(), PcsFile::default());
        assert_eq!(load_gate(dir.path()).unwrap(), None);
    }

    #[test]
    fn corrupt_pcs_is_refused_and_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PCS_FILE);
        fs::write(&path, b"{ this is not json").unwrap();
        assert!(matches!(
            load_pcs(dir.path()),
            Err(StoreError::Corrupt { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), b"{ this is not json");
    }

    #[test]
    fn pcs_with_mismatched_id_is_refused_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = sample_record(5);
        record.id = sample_record(6).id;
        let text = serde_json::to_string(&sample_file(&[record])).unwrap();
        fs::write(dir.path().join(PCS_FILE), text).unwrap();
        assert!(matches!(
            load_pcs(dir.path()),
            Err(StoreError::Corrupt { .. })
        ));
    }

    #[test]
    fn unknown_pcs_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let text = r#"{"version":2,"pcs":[]}"#;
        fs::write(dir.path().join(PCS_FILE), text).unwrap();
        assert!(matches!(
            load_pcs(dir.path()),
            Err(StoreError::Corrupt { .. })
        ));
    }

    #[test]
    fn duplicate_pcs_are_refused() {
        let file = sample_file(&[sample_record(7), sample_record(7)]);
        assert!(validate_pcs(&file).unwrap_err().contains("listed twice"));
    }

    #[test]
    fn save_refuses_an_invalid_record_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = sample_record(8);
        record.public = "not-a-key".to_string();
        assert!(matches!(
            save_pcs(dir.path(), &sample_file(&[record])),
            Err(StoreError::Invalid(_))
        ));
        assert!(entries(dir.path()).is_empty());
    }

    #[test]
    fn gate_value_round_trips_and_corrupt_gate_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let value = serde_json::json!({"hashes": ["abc"], "n": 2});
        save_gate(dir.path(), &value).unwrap();
        assert_eq!(load_gate(dir.path()).unwrap(), Some(value));
        fs::write(dir.path().join(GATE_FILE), b"[").unwrap();
        assert!(matches!(
            load_gate(dir.path()),
            Err(StoreError::Corrupt { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn state_files_are_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        save_pcs(dir.path(), &sample_file(&[sample_record(9)])).unwrap();
        save_gate(dir.path(), &serde_json::json!({})).unwrap();
        for name in [PCS_FILE, GATE_FILE] {
            let mode = fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
    }

    #[test]
    fn decode_public_key_rejects_padding_and_wrong_length() {
        let good = URL_SAFE_NO_PAD.encode([3u8; 32]);
        assert!(decode_public_key(&good).is_some());
        assert!(decode_public_key(&format!("{good}=")).is_none());
        assert!(decode_public_key(&URL_SAFE_NO_PAD.encode([3u8; 31])).is_none());
    }
}
