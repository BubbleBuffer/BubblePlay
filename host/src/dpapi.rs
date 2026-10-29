//! CurrentUser DPAPI for the host library and the tray. Moved here from the tray's platform
//! helpers, which re-export these two names. No entropy and no UI: only this user's logon can
//! decrypt the result.

use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

/// Decrypt raw DPAPI bytes (CurrentUser scope). UI is forbidden, so this never prompts; a blob
/// protected for another user fails.
pub fn dpapi_unprotect_bytes(encrypted: &[u8]) -> Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: encrypted.len().try_into().context("DPAPI blob too large")?,
        // DPAPI reads its input and does not write to it.
        pbData: encrypted.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input points at live memory; output is freed with LocalFree below.
    let result = unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    result.context(
        "DPAPI could not decrypt the stored secret (was it protected for another user?)",
    )?;
    Ok(take_dpapi_output(&output))
}

/// Encrypt raw bytes with CurrentUser DPAPI. No entropy and no UI: only this user's logon can
/// decrypt the result.
pub fn dpapi_protect_bytes(plain: &[u8]) -> Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len().try_into().context("DPAPI input too large")?,
        // DPAPI reads its input and does not write to it.
        pbData: plain.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input points at live memory; output is freed with LocalFree below.
    let result = unsafe {
        CryptProtectData(
            &input,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    result.context("DPAPI could not protect the data for this user")?;
    Ok(take_dpapi_output(&output))
}

/// Copies a DPAPI output buffer into a Vec, zeroes the DPAPI copy and frees it.
fn take_dpapi_output(output: &CRYPT_INTEGER_BLOB) -> Vec<u8> {
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
