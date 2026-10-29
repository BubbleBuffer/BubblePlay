//! The viewer's local state: its device key and the list of PCs it knows.
//!
//! Both live in `%LOCALAPPDATA%\BubblePlay\viewer\`:
//!
//! - `device-key.bpkey`: the portable key file (`bp_device_key::KeyFile`), additionally wrapped
//!   as a CurrentUser DPAPI blob so a copied file is useless to another Windows account.
//! - `pcs.json`: the PCs this viewer can connect to, with each PC's pinned public key.
//!
//! See `docs/architecture/device-keys.md` and `docs/architecture/windows-client.md`.

pub mod cli;
pub mod console;
mod dpapi;
pub mod key;
pub mod pcs;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// `%LOCALAPPDATA%\BubblePlay\viewer`.
pub fn viewer_dir() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    Ok(PathBuf::from(base).join("BubblePlay").join("viewer"))
}

/// Writes `bytes` to `path` through a temporary file in the same directory, so a crash never
/// leaves a half-written key or PC list behind. Creates the directory when needed.
fn write_replacing(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("store path has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    std::fs::write(&temp, bytes).with_context(|| format!("write {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("replace {}", path.display()))
}

#[cfg(test)]
pub(crate) mod test_dir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh directory under the system temp dir, removed on drop.
    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        pub(crate) fn new(tag: &str) -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("bubbleplay-store-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create test dir");
            Self(path)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
