//! Fail-closed identity and protected-image checks for both ends of the pipe.
use anyhow::{ensure, Result};
use std::{
    ffi::OsStr,
    os::windows::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::{
    Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL},
    Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetNamedSecurityInfoW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT, SE_KERNEL_OBJECT,
    },
    Security::{
        GetAce, GetTokenInformation, TokenUser, ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
    },
    System::{
        RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken},
        Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
};
pub const HELPER_EXE: &str = "bubbleplay_secure_helper.exe";
pub struct Handle(pub HANDLE);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}
pub fn wide(t: impl AsRef<OsStr>) -> Vec<u16> {
    t.as_ref().encode_wide().chain(Some(0)).collect()
}
pub fn installed_image() -> Option<PathBuf> {
    crate::admin_agent::trust::program_files()
        .map(|p| p.join("BubblePlay").join("agent").join(HELPER_EXE))
}
pub fn console() -> u32 {
    unsafe { WTSGetActiveConsoleSessionId() }
}
pub fn token_sid(token: HANDLE) -> Result<String> {
    let mut n = 0;
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut n) };
    ensure!(n > 0 && n < 65536, "token length");
    // TOKEN_USER contains pointers: give the API an explicitly aligned buffer.
    let mut b = vec![0usize; (n as usize).div_ceil(std::mem::size_of::<usize>())];
    unsafe { GetTokenInformation(token, TokenUser, Some(b.as_mut_ptr().cast()), n, &mut n) }?;
    let u = unsafe { &*b.as_ptr().cast::<TOKEN_USER>() };
    sid_string(u.User.Sid)
}
fn sid_string(sid: windows::Win32::Security::PSID) -> Result<String> {
    let mut t = PWSTR::null();
    unsafe { ConvertSidToStringSidW(sid, &mut t) }?;
    let s = unsafe { t.to_string() };
    let _ = unsafe { LocalFree(Some(HLOCAL(t.0.cast()))) };
    Ok(s?)
}
pub fn process_sid(pid: Option<u32>) -> Result<String> {
    let p = pid
        .map(|p| unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, p) })
        .transpose()?
        .map(Handle);
    let process = p.as_ref().map_or(unsafe { GetCurrentProcess() }, |p| p.0);
    let mut t = HANDLE::default();
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut t) }?;
    token_sid(Handle(t).0)
}
/// Owner of the pipe the client is connected to. The helper creates its pipe
/// with owner SYSTEM; a process without SYSTEM (or restore privilege) cannot
/// set that owner, so a squatting user-level server is told apart without
/// opening the server's token, which a medium-integrity client may not do.
pub fn pipe_owner(pipe: HANDLE) -> Result<String> {
    let mut sd = PSECURITY_DESCRIPTOR::default();
    let mut owner = windows::Win32::Security::PSID::default();
    let e = unsafe {
        GetSecurityInfo(
            pipe,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut sd),
        )
    };
    ensure!(e.0 == 0, "cannot inspect pipe owner");
    let result = sid_string(owner);
    let _ = unsafe { LocalFree(Some(HLOCAL(sd.0))) };
    result
}
pub fn console_user() -> Result<(Handle, String)> {
    let session = console();
    ensure!(session != u32::MAX, "no console");
    let mut t = HANDLE::default();
    unsafe { WTSQueryUserToken(session, &mut t) }?;
    let h = Handle(t);
    let s = token_sid(h.0)?;
    ensure!(
        s.starts_with("S-1-5-21-") || s.starts_with("S-1-12-1-"),
        "unexpected console identity"
    );
    Ok((h, s))
}
fn privileged(sid: &str) -> bool {
    matches!(
        sid,
        "S-1-5-18"
            | "S-1-5-32-544"
            | "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
    )
}
fn protected_acl(path: &Path) -> Result<()> {
    let p = wide(path);
    let mut sd = PSECURITY_DESCRIPTOR::default();
    let mut owner = windows::Win32::Security::PSID::default();
    let mut acl = std::ptr::null_mut::<ACL>();
    let e = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(p.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut acl),
            None,
            &mut sd,
        )
    };
    ensure!(e.0 == 0, "cannot inspect image ACL");
    let result = (|| {
        ensure!(
            !acl.is_null() && privileged(&sid_string(owner)?),
            "unprotected image owner"
        );
        for i in 0..unsafe { (*acl).AceCount } {
            let mut ace = std::ptr::null_mut();
            unsafe { GetAce(acl, i as u32, &mut ace) }?;
            let a = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            // Access allowed ACEs only; deny ACEs cannot grant write access.
            if a.Header.AceFlags & 0x08 != 0 {
                continue;
            } // INHERIT_ONLY does not apply here.
            ensure!(
                !matches!(a.Header.AceType, 5 | 9 | 11),
                "unsupported allow ACE in image ACL"
            );
            if a.Header.AceType == 0 && a.Mask & 0x500D_0156 != 0 {
                let sid =
                    windows::Win32::Security::PSID((&a.SidStart as *const u32).cast_mut().cast());
                ensure!(
                    privileged(&sid_string(sid)?),
                    "unprivileged image write permission"
                );
            }
        }
        Ok(())
    })();
    let _ = unsafe { LocalFree(Some(HLOCAL(sd.0))) };
    result
}
pub fn protected_image(path: &Path) -> Result<()> {
    let expected = installed_image().ok_or_else(|| anyhow::anyhow!("Program Files missing"))?;
    ensure!(
        path.to_string_lossy()
            .eq_ignore_ascii_case(&expected.to_string_lossy()),
        "unexpected helper image"
    );
    protected_install_file(path)
}
/// Verify a shipped file and each directory up to Program Files. Neither
/// reparse points nor user-writable ownership/ACLs can authorize elevation or
/// a privileged pipe client.
pub fn protected_install_file(path: &Path) -> Result<()> {
    let root = crate::admin_agent::trust::program_files()
        .ok_or_else(|| anyhow::anyhow!("Program Files missing"))?;
    let install = root.join("BubblePlay");
    let prefix = format!("{}\\", install.display());
    ensure!(
        path.is_absolute()
            && path
                .to_string_lossy()
                .to_ascii_lowercase()
                .starts_with(&prefix.to_ascii_lowercase())
            && !path.components().any(|c| matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )),
        "image is outside Program Files\\BubblePlay"
    );
    ensure!(
        std::fs::symlink_metadata(path)?.is_file(),
        "installed file missing"
    );
    let mut p = Some(path);
    let mut reached_root = false;
    while let Some(part) = p {
        ensure!(
            std::fs::symlink_metadata(part)?.file_attributes() & 0x400 == 0,
            "reparse point in protected install path"
        );
        protected_acl(part)?;
        if part
            .to_string_lossy()
            .eq_ignore_ascii_case(&root.to_string_lossy())
        {
            reached_root = true;
            break;
        }
        p = part.parent();
    }
    ensure!(reached_root, "protected path does not reach Program Files");
    Ok(())
}
/// Shared admission rule for both privileged servers. Session and SID checks
/// are performed independently by their pipe servers.
pub fn protected_client_image(path: &Path) -> Result<()> {
    protected_install_file(path)
}
pub fn guard() -> Result<()> {
    ensure!(
        process_sid(None)? == "S-1-5-18",
        "helper requires LocalSystem"
    );
    protected_image(&std::env::current_exe()?)
}
/// Logging is a fixed protected install path, never an environment override.
pub fn log_path() -> Result<PathBuf> {
    guard()?;
    let image = installed_image().ok_or_else(|| anyhow::anyhow!("Program Files missing"))?;
    let directory = image.parent().unwrap().join("logs");
    ensure!(
        std::fs::symlink_metadata(&directory)?.file_attributes() & 0x400 == 0,
        "reparse log directory"
    );
    ensure!(directory.is_dir(), "log directory missing");
    protected_acl(&directory)?;
    for index in 0..5 {
        let name = if index == 0 {
            "secure-helper.log".into()
        } else {
            format!("secure-helper.{index}.log")
        };
        let path = directory.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(m) => {
                ensure!(
                    m.is_file() && m.file_attributes() & 0x400 == 0,
                    "unsafe helper log file"
                );
                protected_acl(&path)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(directory.join("secure-helper.log"))
}
pub struct Descriptor(pub PSECURITY_DESCRIPTOR);
unsafe impl Send for Descriptor {}
unsafe impl Sync for Descriptor {}
impl Drop for Descriptor {
    fn drop(&mut self) {
        let _ = unsafe { LocalFree(Some(HLOCAL(self.0 .0))) };
    }
}
impl Descriptor {
    pub fn new(sddl: &str) -> Result<Self> {
        let w = wide(sddl);
        let mut d = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(w.as_ptr()),
                SDDL_REVISION_1,
                &mut d,
                None,
            )
        }?;
        Ok(Self(d))
    }
    pub fn attributes(&self) -> windows::Win32::Security::SECURITY_ATTRIBUTES {
        windows::Win32::Security::SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<windows::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0 .0,
            bInheritHandle: false.into(),
        }
    }
}
pub fn pipe_sddl(sid: &str) -> String {
    format!("O:SYD:P(A;;GA;;;SY)(A;;0x0012018b;;;{sid})S:(ML;;NW;;;ME)")
}
pub fn mapping_sddl(sid: &str) -> String {
    format!("O:SYD:P(A;;GA;;;SY)(A;;GR;;;{sid})S:(ML;;NW;;;ME)")
}
pub fn query_sddl(sid: &str, access: u32) -> String {
    format!("O:SYD:P(A;;GA;;;SY)(A;;{access:#010x};;;{sid})")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn client_image_outside_protected_install_is_refused() {
        // Exercise the same public validator used by both production servers,
        // with a real image under target/ rather than a synthetic ACL predicate.
        let image = std::env::current_exe().unwrap();
        assert!(
            protected_client_image(&image).is_err(),
            "{}",
            image.display()
        );
    }
    #[test]
    fn object_acls_grant_only_required_console_rights() {
        let sid = "S-1-5-21-1-2-3-1001";
        let pipe = pipe_sddl(sid);
        assert!(pipe.starts_with("O:SYD:P"));
        assert!(!pipe.contains(";;;BA"));
        assert!(!pipe.contains(";;;WD"));
        assert_eq!(crate::admin_agent::protocol::USER_PIPE_RIGHTS & 4, 0);
        let mapping = mapping_sddl(sid);
        assert!(mapping.contains(&format!("(A;;GR;;;{sid})")));
        assert!(!mapping.contains("GW"));
        assert!(query_sddl(sid, 8).contains(&format!("(A;;0x00000008;;;{sid})")));
        assert!(query_sddl(sid, 0x1000).contains(&format!("(A;;0x00001000;;;{sid})")));
    }
}
