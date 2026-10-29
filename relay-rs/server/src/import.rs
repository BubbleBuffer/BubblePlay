//! `import-go-auth`: copies the registered PCs from a Go relay `auth.json` into `pcs.json`.
//!
//! Only the `pcs` table is read. Passwords, TOTP, sessions, rates, gates and approvals are
//! ignored on purpose, and the file's contents are never echoed. Each entry is kept only when
//! its key is the canonical device ID of its own public key, as the Go relay requires. Entries
//! that fail the check are skipped and reported. PCs registered on the Cloudflare Worker relay
//! are not in this file and must re-register with TOKEN.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use bp_device_key::DeviceId;
use serde::Deserialize;

use crate::store::{self, PcRecord, StoreError};

#[derive(Deserialize)]
struct GoAuth {
    version: u32,
    #[serde(default)]
    pcs: BTreeMap<String, GoPc>,
}

#[derive(Deserialize)]
struct GoPc {
    public: String,
    #[serde(rename = "firstSeen", default)]
    first_seen: i64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub imported: usize,
    pub already_present: usize,
    pub skipped: Vec<String>,
}

#[derive(Debug)]
pub enum ImportError {
    Read(std::io::Error),
    Parse(String),
    Version(u32),
    Store(StoreError),
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(source) => write!(f, "cannot read the Go auth file: {source}"),
            Self::Parse(reason) => write!(f, "the Go auth file is not valid JSON: {reason}"),
            Self::Version(version) => {
                write!(f, "unsupported Go auth file version {version}, expected 1")
            }
            Self::Store(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ImportError {}

impl From<StoreError> for ImportError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// Reads a Go `auth.json` from disk and merges its PCs into `state_dir/pcs.json`.
pub fn import_go_auth_file(
    state_dir: &Path,
    auth_path: &Path,
) -> Result<ImportReport, ImportError> {
    let text = std::fs::read_to_string(auth_path).map_err(ImportError::Read)?;
    import_go_auth(state_dir, &text)
}

/// Merges the PCs of Go `auth.json` text into `state_dir/pcs.json` and writes the file only when
/// something new was added. An existing PC with the same ID is kept as it is.
pub fn import_go_auth(state_dir: &Path, auth_json: &str) -> Result<ImportReport, ImportError> {
    let auth: GoAuth =
        serde_json::from_str(auth_json).map_err(|e| ImportError::Parse(e.to_string()))?;
    if auth.version != 1 {
        return Err(ImportError::Version(auth.version));
    }
    let mut file = store::load_pcs(state_dir)?;
    let mut report = ImportReport::default();
    for (key, pc) in &auth.pcs {
        match convert(key, pc) {
            Err(reason) => report.skipped.push(format!("{key}: {reason}")),
            Ok(record) => match file.pcs.iter().find(|existing| existing.id == record.id) {
                Some(existing) if existing.public == record.public => report.already_present += 1,
                Some(_) => report.skipped.push(format!(
                    "{key}: conflicts with a record already in pcs.json"
                )),
                None => {
                    file.pcs.push(record);
                    report.imported += 1;
                }
            },
        }
    }
    if report.imported > 0 {
        store::save_pcs(state_dir, &file)?;
    }
    Ok(report)
}

fn convert(key: &str, pc: &GoPc) -> Result<PcRecord, String> {
    let id = DeviceId::parse(key).map_err(|_| "key is not a device ID".to_string())?;
    if id.to_string() != key {
        return Err("key is not in canonical device ID form".to_string());
    }
    let public = store::decode_public_key(&pc.public)
        .ok_or_else(|| "public key is not an unpadded base64url 32-byte value".to_string())?;
    if DeviceId::from_public_key(&public) != id {
        return Err("device ID does not match its public key".to_string());
    }
    let registered_ms = u64::try_from(pc.first_seen)
        .ok()
        .filter(|ms| *ms > 0)
        .ok_or_else(|| "firstSeen is not a positive time".to_string())?;
    Ok(PcRecord {
        id: key.to_string(),
        public: pc.public.clone(),
        registered_ms,
        admitted: Vec::new(),
        remote_pairing: false,
    })
}
