//! Fixed bounds come from the protocol, never from writable shared memory.
//! A notified slot is held until its exact sequence is acknowledged.
use super::{
    protocol::*,
    trust::{wide, Descriptor, Handle},
};
use anyhow::{ensure, Result};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{GetLastError, ERROR_ALREADY_EXISTS, INVALID_HANDLE_VALUE},
        Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG},
        System::Memory::{
            CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ,
            FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
        },
    },
};
pub struct Mapping {
    _handle: Handle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}
impl Drop for Mapping {
    fn drop(&mut self) {
        let _ = unsafe { UnmapViewOfFile(self.view) };
    }
}
impl Mapping {
    pub fn create(sid: &str, session: u32) -> Result<(Self, String)> {
        let mut random = [0u8; 16];
        unsafe { BCryptGenRandom(None, &mut random, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }.ok()?;
        let nonce: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let name = format!(r"Local\BubblePlay.SecureFrames.s{session}.{nonce}");
        let w = wide(&name);
        let d = Descriptor::new(&super::trust::mapping_sddl(sid))?;
        let a = d.attributes();
        let h = Handle(unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(&a),
                PAGE_READWRITE,
                0,
                MAPPING_BYTES as u32,
                PCWSTR(w.as_ptr()),
            )
        }?);
        ensure!(
            unsafe { GetLastError() } != ERROR_ALREADY_EXISTS,
            "mapping already exists"
        );
        let view = unsafe { MapViewOfFile(h.0, FILE_MAP_WRITE, 0, 0, MAPPING_BYTES) };
        ensure!(!view.Value.is_null(), "mapping failed");
        Ok((Self { _handle: h, view }, name))
    }
    pub fn open(name: &str, session: u32) -> Result<Self> {
        let prefix = format!(r"Local\BubblePlay.SecureFrames.s{session}.");
        let suffix = name
            .strip_prefix(&prefix)
            .ok_or_else(|| anyhow::anyhow!("mapping name"))?;
        ensure!(
            suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit()),
            "mapping nonce"
        );
        let w = wide(name);
        let h = Handle(unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(w.as_ptr())) }?);
        let view = unsafe { MapViewOfFile(h.0, FILE_MAP_READ, 0, 0, MAPPING_BYTES) };
        ensure!(!view.Value.is_null(), "mapping failed");
        Ok(Self { _handle: h, view })
    }
    pub fn publish(&self, f: FrameInfo, p: &[u8]) -> Result<()> {
        let len = f.validate(0)?;
        ensure!(p.len() == len, "pixel length");
        let start = unsafe {
            self.view
                .Value
                .cast::<u8>()
                .add(f.slot as usize * SLOT_SPAN)
        };
        unsafe {
            std::ptr::copy_nonoverlapping(p.as_ptr(), start.add(FRAME_HEADER), len);
            std::ptr::copy_nonoverlapping(f.header().as_ptr(), start, FRAME_HEADER)
        };
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        Ok(())
    }
    pub fn copy(&self, f: FrameInfo, last: u64) -> Result<Vec<u8>> {
        let len = f.validate(last)?;
        let start = unsafe {
            self.view
                .Value
                .cast::<u8>()
                .add(f.slot as usize * SLOT_SPAN)
        };
        let mut header = [0u8; FRAME_HEADER];
        let mut after = [0u8; FRAME_HEADER];
        let mut p = vec![0; len];
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        // No mapped slice/reference: copy only the independently checked fixed
        // range. Corrupt header dimensions can never affect pointer arithmetic.
        unsafe {
            std::ptr::copy_nonoverlapping(start, header.as_mut_ptr(), FRAME_HEADER);
            std::ptr::copy_nonoverlapping(start.add(FRAME_HEADER), p.as_mut_ptr(), len);
            std::ptr::copy_nonoverlapping(start, after.as_mut_ptr(), FRAME_HEADER)
        };
        ensure!(header == after, "torn frame");
        f.validate_copy(&header, &p, last)?;
        Ok(p)
    }
}
