use super::protocol::{self, Message, HEADER_LEN};
use std::{
    fs::File,
    io,
    os::windows::io::AsRawHandle,
    time::{Duration, Instant},
};
use windows::Win32::{Foundation::HANDLE, System::Pipes::PeekNamedPipe};
pub fn handle(f: &File) -> HANDLE {
    HANDLE(f.as_raw_handle().cast())
}
/// Peek an entire bounded frame before a synchronous read. Never wait for a
/// malicious/truncated payload after consuming the header.
pub fn poll(f: &mut File) -> io::Result<Option<Message>> {
    let mut h = [0u8; HEADER_LEN];
    let mut n = 0;
    let mut got = 0;
    unsafe {
        PeekNamedPipe(
            handle(f),
            Some(h.as_mut_ptr().cast()),
            HEADER_LEN as u32,
            Some(&mut got),
            Some(&mut n),
            None,
        )
    }
    .map_err(|e| io::Error::from_raw_os_error(e.code().0 & 65535))?;
    if got < HEADER_LEN as u32 {
        return Ok(None);
    }
    let len = protocol::payload_len(&h)?;
    if (n as usize) < HEADER_LEN + len {
        return Ok(None);
    }
    protocol::read(f).map(Some)
}
pub fn read_deadline(f: &mut File, deadline: Instant) -> io::Result<Message> {
    loop {
        if let Some(m) = poll(f)? {
            return Ok(m);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "helper timeout"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
