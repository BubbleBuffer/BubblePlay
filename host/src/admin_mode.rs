//! Protected, console-user-bound Admin mode. Missing or malformed policy is OFF.
use crate::secure_helper::trust::{self, Handle};
use anyhow::{ensure, Result};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::{
    Foundation::{LocalFree, HLOCAL},
    Security::{
        Authorization::ConvertSidToStringSidW, GetTokenInformation, LookupAccountNameW,
        SidTypeUser, TokenElevation, PSID, SID_NAME_USE, TOKEN_ELEVATION, TOKEN_QUERY,
    },
    System::{
        Registry::{
            RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE,
            KEY_QUERY_VALUE, KEY_SET_VALUE, KEY_WOW64_64KEY, REG_DWORD, REG_SAM_FLAGS, REG_SZ,
            REG_VALUE_TYPE,
        },
        RemoteDesktop::{
            ProcessIdToSessionId, WTSDomainName, WTSFreeMemory, WTSQuerySessionInformationW,
            WTSUserName, WTS_INFO_CLASS,
        },
        Threading::{GetCurrentProcess, GetCurrentProcessId, OpenProcessToken},
    },
};

pub const REGISTRY_KEY: &str = r"SOFTWARE\BubblePlay";

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        let _ = unsafe { RegCloseKey(self.0) };
    }
}
fn open(access: REG_SAM_FLAGS) -> Result<Key> {
    let path = trust::wide(REGISTRY_KEY);
    let mut key = HKEY::default();
    unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(path.as_ptr()),
            None,
            access | KEY_WOW64_64KEY,
            &mut key,
        )
    }
    .ok()?;
    Ok(Key(key))
}
fn read(key: &Key, name: &str) -> Result<(REG_VALUE_TYPE, Vec<u8>)> {
    let name = trust::wide(name);
    // Both values are small. A fixed bounded buffer also fails closed if a
    // concurrent writer enlarges the value, without allocation from HKLM data.
    let mut data = vec![0u8; 1024];
    let mut bytes = data.len() as u32;
    let mut kind = REG_VALUE_TYPE::default();
    unsafe {
        RegQueryValueExW(
            key.0,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            Some(data.as_mut_ptr()),
            Some(&mut bytes),
        )
    }
    .ok()?;
    ensure!(bytes as usize <= data.len(), "oversized Admin mode policy");
    data.truncate(bytes as usize);
    Ok((kind, data))
}
fn effective_values(
    mode: (REG_VALUE_TYPE, &[u8]),
    bound: (REG_VALUE_TYPE, &[u8]),
    sid: &str,
) -> bool {
    if mode.0 != REG_DWORD || mode.1 != 1u32.to_le_bytes() || bound.0 != REG_SZ || sid.is_empty() {
        return false;
    }
    if bound.1.len() < 2 || !bound.1.len().is_multiple_of(2) {
        return false;
    }
    let mut text: Vec<u16> = bound
        .1
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    if text.pop() != Some(0) || text.contains(&0) {
        return false;
    }
    String::from_utf16(&text).is_ok_and(|value| value == sid)
}
/// Reads the native registry view anew. The caller supplies a trusted SID,
/// resolved independently of a remote request.
pub fn effective_for_sid(sid: &str) -> bool {
    let Ok(key) = open(KEY_QUERY_VALUE) else {
        return false;
    };
    let (Ok((mode_type, mode)), Ok((sid_type, bound))) =
        (read(&key, "AdminMode"), read(&key, "AdminModeSid"))
    else {
        return false;
    };
    effective_values((mode_type, &mode), (sid_type, &bound), sid)
}
fn session_text(session: u32, kind: WTS_INFO_CLASS) -> Result<String> {
    let mut buffer = PWSTR::null();
    let mut bytes = 0;
    unsafe { WTSQuerySessionInformationW(None, session, kind, &mut buffer, &mut bytes) }?;
    let result = (|| {
        ensure!(
            !buffer.is_null() && (2..=65536).contains(&bytes) && bytes % 2 == 0,
            "invalid console identity text"
        );
        let text = unsafe { std::slice::from_raw_parts(buffer.0, bytes as usize / 2) };
        let end = text
            .iter()
            .position(|c| *c == 0)
            .ok_or_else(|| anyhow::anyhow!("unterminated console identity"))?;
        Ok(String::from_utf16(&text[..end])?)
    })();
    unsafe { WTSFreeMemory(buffer.0.cast()) };
    result
}
/// Resolves WTS-provided console account information, without opening the
/// user's token (WTSQueryUserToken requires SYSTEM).
pub fn console_sid() -> Result<String> {
    let session = trust::console();
    ensure!(session != u32::MAX, "no console");
    let user = session_text(session, WTSUserName)?;
    let domain = session_text(session, WTSDomainName)?;
    ensure!(!user.is_empty() && !domain.is_empty(), "no console user");
    let account = trust::wide(format!("{domain}\\{user}"));
    let mut sid_bytes = 0;
    let mut domain_chars = 0;
    let mut use_kind = SID_NAME_USE::default();
    let _ = unsafe {
        LookupAccountNameW(
            PCWSTR::null(),
            PCWSTR(account.as_ptr()),
            None,
            &mut sid_bytes,
            None,
            &mut domain_chars,
            &mut use_kind,
        )
    };
    ensure!(
        (1..=65536).contains(&sid_bytes) && domain_chars <= 65536,
        "invalid console SID length"
    );
    let mut sid = vec![0usize; (sid_bytes as usize).div_ceil(std::mem::size_of::<usize>())];
    let mut resolved_domain = vec![0u16; domain_chars as usize];
    unsafe {
        LookupAccountNameW(
            PCWSTR::null(),
            PCWSTR(account.as_ptr()),
            Some(PSID(sid.as_mut_ptr().cast())),
            &mut sid_bytes,
            Some(PWSTR(resolved_domain.as_mut_ptr())),
            &mut domain_chars,
            &mut use_kind,
        )
    }?;
    ensure!(use_kind == SidTypeUser, "console account is not a user");
    let mut text = PWSTR::null();
    unsafe { ConvertSidToStringSidW(PSID(sid.as_mut_ptr().cast()), &mut text) }?;
    let result = unsafe { text.to_string() };
    let _ = unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
    ensure!(
        trust::console() == session,
        "console changed while resolving identity"
    );
    Ok(result?)
}
pub fn effective_for_console() -> bool {
    console_sid().is_ok_and(|sid| effective_for_sid(&sid))
}
fn enabling_identity() -> Result<String> {
    let mut token = windows::Win32::Foundation::HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }?;
    let token = Handle(token);
    let mut elevation = TOKEN_ELEVATION::default();
    let mut bytes = 0;
    unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut bytes,
        )
    }?;
    ensure!(
        bytes == std::mem::size_of::<TOKEN_ELEVATION>() as u32 && elevation.TokenIsElevated != 0,
        "Admin mode ON requires elevation"
    );
    let own_sid = trust::token_sid(token.0)?;
    let mut own_session = u32::MAX;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut own_session) }?;
    ensure!(
        own_session == trust::console(),
        "process is not in the console session"
    );
    let console_sid = console_sid()?;
    ensure!(
        own_session == trust::console() && console_sid == own_sid,
        "elevated identity differs from the console user"
    );
    Ok(own_sid)
}
fn write(key: &Key, name: &str, kind: REG_VALUE_TYPE, data: &[u8]) -> Result<()> {
    let name = trust::wide(name);
    unsafe { RegSetValueExW(key.0, PCWSTR(name.as_ptr()), None, kind, Some(data)) }.ok()?;
    Ok(())
}
/// ON is allowed only for the elevated console user. OFF never changes the
/// SID and relies on the protected key's administrator/SYSTEM write ACL.
pub fn set_admin_mode(on: bool) -> Result<()> {
    let sid = if on { Some(enabling_identity()?) } else { None };
    let key = open(KEY_SET_VALUE)?;
    if let Some(sid) = sid {
        // Clear before updating the binding: a partial write stays OFF.
        write(&key, "AdminMode", REG_DWORD, &0u32.to_le_bytes())?;
        let data: Vec<u8> = sid
            .encode_utf16()
            .chain(Some(0))
            .flat_map(u16::to_le_bytes)
            .collect();
        write(&key, "AdminModeSid", REG_SZ, &data)?;
        write(&key, "AdminMode", REG_DWORD, &1u32.to_le_bytes())
    } else {
        write(&key, "AdminMode", REG_DWORD, &0u32.to_le_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Registry::{REG_BINARY, REG_EXPAND_SZ};
    const SID: &str = "S-1-5-21-1-2-3-1001";
    fn sz(value: &str) -> Vec<u8> {
        value
            .encode_utf16()
            .chain(Some(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }
    #[test]
    fn only_exact_dword_one_and_matching_string_sid_enable() {
        let sid = sz(SID);
        assert!(effective_values(
            (REG_DWORD, &1u32.to_le_bytes()),
            (REG_SZ, &sid),
            SID
        ));
        for value in [0u32, 2, u32::MAX] {
            assert!(!effective_values(
                (REG_DWORD, &value.to_le_bytes()),
                (REG_SZ, &sid),
                SID
            ));
        }
        assert!(!effective_values(
            (REG_BINARY, &1u32.to_le_bytes()),
            (REG_SZ, &sid),
            SID
        ));
        assert!(!effective_values(
            (REG_DWORD, &1u32.to_le_bytes()),
            (REG_EXPAND_SZ, &sid),
            SID
        ));
        assert!(!effective_values(
            (REG_DWORD, &1u32.to_le_bytes()),
            (REG_SZ, &sid),
            "other"
        ));
        assert!(!effective_values(
            (REG_DWORD, &1u32.to_le_bytes()),
            (REG_SZ, &sz("")),
            ""
        ));
    }
    #[test]
    fn missing_and_malformed_values_fail_closed() {
        let one = 1u32.to_le_bytes();
        let sid = sz(SID);
        for value in [&b""[..], &b"\x01"[..], &b"\x01\0\0\0\0"[..]] {
            assert!(!effective_values((REG_DWORD, value), (REG_SZ, &sid), SID));
        }
        for value in [
            Vec::new(),
            vec![0],
            vec![0, 0],
            sz("other"),
            sz("S\0extra"),
            sid[..sid.len() - 2].to_vec(),
            vec![0, 0xd8, 0, 0],
        ] {
            assert!(!effective_values((REG_DWORD, &one), (REG_SZ, &value), SID));
        }
    }
}
