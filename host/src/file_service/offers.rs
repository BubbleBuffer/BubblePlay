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
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                debug!(path = %path.display(), ?error, "offer: unreadable path skipped");
                continue;
            }
        };
        if !metadata.is_file() {
            info!(path = %path.display(), "offer: directory skipped (files only in v1)");
            continue;
        }
        if metadata.len() > ft::MAX_FILE_BYTES {
            info!(path = %path.display(), size = metadata.len(), "offer: file above 2 GiB skipped");
            continue;
        }
        let Some(name) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        if name.is_empty() || name.len() > ft::MAX_NAME_BYTES {
            continue;
        }
        if items.len() == ft::MAX_BATCH_FILES as usize {
            info!("offer: more than 64 files; the rest are skipped");
            break;
        }
        let (sha256, size) = match hash_file(path) {
            Ok(hashed) => hashed,
            Err(error) => {
                warn!(path = %path.display(), ?error, "offer: file cannot be hashed, not offered");
                continue;
            }
        };
        if size > ft::MAX_FILE_BYTES {
            continue;
        }
        let mtime_ms = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_millis() as u64);
        items.push(OfferedFile {
            id: items.len() as u32 + 1,
            path: path.clone(),
            mime: mime_for(&name).to_string(),
            name,
            size,
            mtime_ms,
            sha256,
        });
    }
    items
}
