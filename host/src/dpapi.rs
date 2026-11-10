//! CurrentUser DPAPI for the host library and the tray. Moved here from the tray's platform
//! helpers. New blobs use fixed BubblePlay application entropy; readers also accept older
//! blobs without entropy so their owning stores can migrate them atomically. No UI is shown.

use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

// Keep these bytes identical to host/scripts/setup-tray.ps1. This is application separation,
// not a secret or protection against another process running as the same Windows user.
const APP_ENTROPY: &[u8] = b"BubblePlay.Host.DPAPI.v1";

/// Decrypt raw DPAPI bytes (CurrentUser scope). UI is forbidden, so this never prompts; a blob
/// protected for another user fails.
pub fn dpapi_unprotect_bytes(encrypted: &[u8]) -> Result<Vec<u8>> {
    dpapi_unprotect_bytes_with_migration(encrypted).map(|(plain, _)| plain)
}

/// Decrypt with app entropy first, then the legacy no-entropy format. The returned flag is
/// true only for legacy data: its owning store must validate and atomically rewrite it with
/// [`dpapi_protect_bytes`] before treating the load as successful.
pub fn dpapi_unprotect_bytes_with_migration(encrypted: &[u8]) -> Result<(Vec<u8>, bool)> {
    match unprotect_with_entropy(encrypted, Some(APP_ENTROPY)) {
        Ok(plain) => Ok((plain, false)),
        Err(_) => unprotect_with_entropy(encrypted, None).map(|plain| (plain, true)),
    }
}

fn unprotect_with_entropy(encrypted: &[u8], entropy: Option<&[u8]>) -> Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: encrypted.len().try_into().context("DPAPI blob too large")?,
        // DPAPI reads its input and does not write to it.
        pbData: encrypted.as_ptr() as *mut u8,
    };
    let entropy = entropy.map(entropy_blob);
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input points at live memory; output is freed with LocalFree below.
    let result = unsafe {
        CryptUnprotectData(
            &input,
            None,
            entropy.as_ref().map(|blob| blob as *const _),
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

/// Encrypt raw bytes with CurrentUser DPAPI and fixed BubblePlay application entropy.
/// No UI is shown. Only this user's logon can decrypt the result.
pub fn dpapi_protect_bytes(plain: &[u8]) -> Result<Vec<u8>> {
    protect_with_entropy(plain, Some(APP_ENTROPY))
}

fn protect_with_entropy(plain: &[u8], entropy: Option<&[u8]>) -> Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len().try_into().context("DPAPI input too large")?,
        // DPAPI reads its input and does not write to it.
        pbData: plain.as_ptr() as *mut u8,
    };
    let entropy = entropy.map(entropy_blob);
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input points at live memory; output is freed with LocalFree below.
    let result = unsafe {
        CryptProtectData(
            &input,
            PCWSTR::null(),
            entropy.as_ref().map(|blob| blob as *const _),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    result.context("DPAPI could not protect the data for this user")?;
    Ok(take_dpapi_output(&output))
}

fn entropy_blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        // Only the fixed application constant above reaches this helper.
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    }
}

#[cfg(test)]
pub(crate) fn protect_legacy_for_test(plain: &[u8]) -> Result<Vec<u8>> {
    protect_with_entropy(plain, None)
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
