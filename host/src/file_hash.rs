//! Streaming SHA-256 through Windows CNG (no extra crate).

use anyhow::{bail, Result};
use windows::Win32::Security::Cryptography::{
    BCryptCreateHash, BCryptDestroyHash, BCryptFinishHash, BCryptHashData, BCRYPT_HASH_HANDLE,
    BCRYPT_SHA256_ALG_HANDLE,
};

pub struct Sha256 {
    handle: BCRYPT_HASH_HANDLE,
}

// The CNG hash object is only used from one thread at a time.
unsafe impl Send for Sha256 {}

impl Sha256 {
    pub fn new() -> Result<Self> {
        let mut handle = BCRYPT_HASH_HANDLE::default();
        let status =
            unsafe { BCryptCreateHash(BCRYPT_SHA256_ALG_HANDLE, &mut handle, None, None, 0) };
        if status.is_err() {
            bail!("BCryptCreateHash failed: {:#x}", status.0);
        }
        Ok(Self { handle })
    }

    pub fn update(&mut self, data: &[u8]) {
        // ULONG-sized calls; chunks are at most 64 KiB in practice.
        for part in data.chunks(1 << 30) {
            let status = unsafe { BCryptHashData(self.handle, part, 0) };
            assert!(status.is_ok(), "BCryptHashData failed: {:#x}", status.0);
        }
    }

    pub fn finish(self) -> [u8; 32] {
        let mut out = [0u8; 32];
        let status = unsafe { BCryptFinishHash(self.handle, &mut out, 0) };
        assert!(status.is_ok(), "BCryptFinishHash failed: {:#x}", status.0);
        out
    }
}

impl Drop for Sha256 {
    fn drop(&mut self) {
        unsafe {
            let _ = BCryptDestroyHash(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        let hex = |bytes: [u8; 32]| -> String {
            bytes.iter().map(|byte| format!("{byte:02x}")).collect()
        };
        assert_eq!(
            hex(Sha256::new().unwrap().finish()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let mut hasher = Sha256::new().unwrap();
        hasher.update(b"hel");
        hasher.update(b"lo");
        assert_eq!(
            hex(hasher.finish()),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
