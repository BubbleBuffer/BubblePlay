//! CurrentUser DPAPI wrapping for the stored key file (the same layer the PC host uses).

use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

/// Encrypts `plain` for the current Windows user. No entropy and no UI.
pub(super) fn protect(plain: &[u8]) -> Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len().try_into().context("DPAPI input too large")?,
        // DPAPI reads its input and does not write to it.
        pbData: plain.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input points at live memory; output is freed in take_output.
    unsafe {
        CryptProtectData(
            &input,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .context("DPAPI could not protect the key for this user")?;
    Ok(take_output(&output))
}

/// Decrypts a blob made by [`protect`]. Fails for another user's blob or a damaged file.
pub(super) fn unprotect(wrapped: &[u8]) -> Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: wrapped.len().try_into().context("DPAPI blob too large")?,
        // DPAPI reads its input and does not write to it.
        pbData: wrapped.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input points at live memory; output is freed in take_output.
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .context("DPAPI could not open the stored key (was it saved by another Windows user?)")?;
    Ok(take_output(&output))
}

/// Copies a DPAPI output buffer, wipes it and frees it.
fn take_output(output: &CRYPT_INTEGER_BLOB) -> Vec<u8> {
    if output.pbData.is_null() {
        return Vec::new();
    }
    // SAFETY: DPAPI returned cbData bytes at pbData, which we own until LocalFree below.
    let bytes = unsafe { std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize) };
    let copied = bytes.to_vec();
    zeroize::Zeroize::zeroize(bytes);
    // SAFETY: allocated by the DPAPI call that filled `output`.
    let _ = unsafe { LocalFree(Some(HLOCAL(output.pbData.cast()))) };
    copied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protect_round_trips_and_hides_the_plaintext() {
        let plain = b"{\"format\":\"bubbleplay-device-key\"}";
        let wrapped = protect(plain).unwrap();
        assert!(!wrapped.windows(plain.len()).any(|w| w == plain));
        assert_eq!(unprotect(&wrapped).unwrap(), plain);
    }

    #[test]
    fn damaged_blob_is_refused() {
        let mut wrapped = protect(b"secret").unwrap();
        let last = wrapped.len() - 1;
        wrapped[last] ^= 0xff;
        assert!(unprotect(&wrapped).is_err());
    }
}
