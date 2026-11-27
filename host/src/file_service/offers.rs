//! Building the offer a clipboard copy or a Send To request becomes: which files, hashed.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use tracing::{debug, info, warn};

use super::{mime_for, OfferedFile};
use crate::file_hash::Sha256;
use crate::file_transfer as ft;

/// SHA-256 and byte count of a file, read in full. Offers are hashed before they are announced
/// because the viewer verifies a download against the hash in the sealed offer.
fn hash_file(path: &Path) -> Result<([u8; ft::SHA256_BYTES], u64)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new()?;
    let mut buffer = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok((hasher.finish(), total));
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
}

/// Build offer items from paths: regular files only (directories are skipped
/// in v1), at most 64, each at most 2 GiB. Every file is hashed first; one that
/// cannot be read is not offered.
pub(super) fn offer_items(paths: &[PathBuf]) -> Vec<OfferedFile> {
    let mut items = Vec::new();
    for path in paths {
        if items.len() == ft::MAX_BATCH_FILES as usize {
            info!("offer: more than 64 files; the rest are skipped");
            break;
        }
        if let Some(item) = offer_item(path, items.len() as u32 + 1) {
            items.push(item);
        }
    }
    items
}

fn offer_item(path: &Path, id: u32) -> Option<OfferedFile> {
    let metadata = fs::metadata(path)
        .inspect_err(
            |error| debug!(path = %path.display(), ?error, "offer: unreadable path skipped"),
        )
        .ok()?;
    if !metadata.is_file() {
        info!(path = %path.display(), "offer: directory skipped (files only in v1)");
        return None;
    }
    if metadata.len() > ft::MAX_FILE_BYTES {
        info!(path = %path.display(), size = metadata.len(), "offer: file above 2 GiB skipped");
        return None;
    }
    let name = path.file_name()?.to_string_lossy().into_owned();
    if name.is_empty() || name.len() > ft::MAX_NAME_BYTES {
        return None;
    }
    let (sha256, size) = hash_file(path)
        .inspect_err(|error| warn!(path = %path.display(), ?error, "offer: file cannot be hashed, not offered"))
        .ok()?;
    if size > ft::MAX_FILE_BYTES {
        return None;
    }
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_millis() as u64);
    Some(OfferedFile {
        id,
        path: path.to_path_buf(),
        mime: mime_for(&name).to_string(),
        name,
        size,
        mtime_ms,
        sha256,
    })
}
