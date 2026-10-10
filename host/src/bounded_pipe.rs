//! Deadline-bound local named-pipe I/O. Never use `File`'s synchronous I/O
//! or `FlushFileBuffers` on these handles.
//!
//! Cancellation is a request, not a completion. In-flight buffers, OVERLAPPED,
//! events and pipe handles stay owned until GetOverlappedResult confirms completion.
//! A fixed, lock-free quarantine bounds even a driver that never completes a
//! cancellation: after sixteen outstanding operations, new I/O fails closed.

use std::cell::UnsafeCell;
use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Instant;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_OVERLAPPED,
    FILE_SHARE_NONE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

const MAX_BUFFER: usize = 128 * 1024;
const RESERVED: *mut Pending = std::ptr::dangling_mut();
static OPERATIONS: [AtomicPtr<Pending>; 16] = [const { AtomicPtr::new(std::ptr::null_mut()) }; 16];

fn os_error(error: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error(error.code().0 & 0xffff)
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "named pipe operation timed out")
}

struct Pending {
    file: Option<File>,
    event: OwnedHandle,
    // Windows mutates completion fields while the operation is outstanding.
    overlapped: UnsafeCell<OVERLAPPED>,
    bytes: Vec<u8>,
}

impl Pending {
    fn handle(&self) -> HANDLE {
        HANDLE(self.file.as_ref().unwrap().as_raw_handle() as _)
    }

    fn result(&self) -> io::Result<u32> {
        let mut transferred = 0;
        // SAFETY: this operation exclusively owns the handle and stable boxed
        // OVERLAPPED. FALSE never waits for an unfinished operation.
        unsafe {
            GetOverlappedResult(
                self.handle(),
                self.overlapped.get(),
                &mut transferred,
                false,
            )
        }
        .map_err(os_error)?;
        Ok(transferred)
    }

    fn incomplete(&self) -> bool {
        self.result()
            .is_err_and(|error| error.raw_os_error() == Some(ERROR_IO_INCOMPLETE.0 as i32))
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        // Only completed operations reach Drop. Remove private input bytes
        // before releasing their allocation (volatile stores prevent elision).
        for byte in &mut self.bytes {
            // SAFETY: exclusive, valid byte pointer.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}

/// Reserves a quarantine slot before issuing I/O, so cancellation never has
/// to allocate storage or acquire a lock. Reaps completed cancelled operations.
fn reserve() -> io::Result<usize> {
    for (index, slot) in OPERATIONS.iter().enumerate() {
        let old = slot.load(Ordering::Acquire);
        if old == RESERVED {
            continue;
        }
        if slot
            .compare_exchange(old, RESERVED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        if old.is_null() {
            return Ok(index);
        }
        // SAFETY: successful CAS gives exclusive ownership of this boxed
        // operation. Its allocations remain stable while Windows uses them.
        let pending = unsafe { Box::from_raw(old) };
        if pending.incomplete() {
            slot.store(Box::into_raw(pending), Ordering::Release);
        } else {
            drop(pending);
            return Ok(index);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "named pipe cancellation capacity exhausted",
    ))
}

struct Operation {
    pending: Option<Box<Pending>>,
    slot: usize,
    in_flight: bool,
}

impl Drop for Operation {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            if self.in_flight {
                // SAFETY: the buffer, event, file and OVERLAPPED remain alive.
                let _ = unsafe { CancelIoEx(pending.handle(), Some(pending.overlapped.get())) };
                if pending.incomplete() {
                    OPERATIONS[self.slot].store(Box::into_raw(pending), Ordering::Release);
                    return;
                }
            }
            drop(pending);
        }
        OPERATIONS[self.slot].store(std::ptr::null_mut(), Ordering::Release);
    }
}

pub(crate) struct BoundedPipe {
    file: Option<File>,
}

impl BoundedPipe {
    pub(crate) fn open(name: &str, access: u32, security_flags: u32) -> io::Result<Self> {
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: terminated name, no borrowed security attributes. Every
        // operation on this exclusively-owned handle is OVERLAPPED.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                access,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(security_flags | FILE_FLAG_OVERLAPPED.0),
                None,
            )
        }
        .map_err(os_error)?;
        // SAFETY: CreateFile returned a new owned handle.
        Ok(Self {
            file: Some(unsafe { File::from_raw_handle(handle.0 as _) }),
        })
    }

    pub(crate) fn file(&self) -> io::Result<&File> {
        self.file.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "named pipe connection closed after incomplete I/O",
            )
        })
    }

    fn transfer(&mut self, bytes: Vec<u8>, write: bool, deadline: Instant) -> io::Result<Vec<u8>> {
        if bytes.len() > MAX_BUFFER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "named pipe operation exceeds buffer limit",
            ));
        }
        if Instant::now() >= deadline {
            return Err(timed_out());
        }
        let slot = reserve()?;
        // SAFETY: no borrowed attributes; private manual-reset event.
        let event = match unsafe { CreateEventW(None, true, false, None) } {
            Ok(event) => event,
            Err(error) => {
                OPERATIONS[slot].store(std::ptr::null_mut(), Ordering::Release);
                return Err(os_error(error));
            }
        };
        // SAFETY: fresh owned event handle.
        let event = unsafe { OwnedHandle::from_raw_handle(event.0 as _) };
        let Some(file) = self.file.take() else {
            OPERATIONS[slot].store(std::ptr::null_mut(), Ordering::Release);
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "named pipe connection closed",
            ));
        };
        let mut operation = Operation {
            pending: Some(Box::new(Pending {
                file: Some(file),
                event,
                overlapped: UnsafeCell::new(OVERLAPPED::default()),
                bytes,
            })),
            slot,
            in_flight: false,
        };
        let pending = operation.pending.as_mut().unwrap();
        let event_handle = HANDLE(pending.event.as_raw_handle() as _);
        // SAFETY: operation has not been submitted yet.
        unsafe { (*pending.overlapped.get()).hEvent = event_handle };
        // SAFETY: boxed storage and buffer are stable, and exclusively held
        // until completed or retained in quarantine. Never pass stack buffers.
        let issued = unsafe {
            if write {
                WriteFile(
                    pending.handle(),
                    Some(&pending.bytes),
                    None,
                    Some(pending.overlapped.get()),
                )
            } else {
                ReadFile(
                    pending.handle(),
                    Some(&mut pending.bytes),
                    None,
                    Some(pending.overlapped.get()),
                )
            }
        };
        match issued {
            Ok(()) => {}
            Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {
                operation.in_flight = true;
                let remaining = deadline.saturating_duration_since(Instant::now());
                let millis = remaining
                    .as_nanos()
                    .div_ceil(1_000_000)
                    .min(u128::from(u32::MAX - 1)) as u32;
                // SAFETY: exclusively-owned valid event; bounded wait.
                let waited = unsafe { WaitForSingleObject(event_handle, millis) };
                if waited == WAIT_TIMEOUT {
                    return Err(timed_out());
                }
                if waited != WAIT_OBJECT_0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Err(error) => {
                let mut pending = operation.pending.take().unwrap();
                self.file = pending.file.take();
                pending.bytes.fill(0);
                return Err(os_error(error));
            }
        }
        let transferred = operation.pending.as_ref().unwrap().result()? as usize;
        operation.in_flight = false;
        let pending = operation.pending.as_mut().unwrap();
        if transferred > pending.bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "named pipe transferred invalid byte count",
            ));
        }
        if write {
            for byte in &mut pending.bytes {
                // SAFETY: I/O has completed and the allocation is exclusive.
                unsafe { std::ptr::write_volatile(byte, 0) };
            }
        }
        pending.bytes.truncate(transferred);
        // Move the owned handle back without cloning or synchronous cleanup.
        let mut pending = operation.pending.take().unwrap();
        self.file = pending.file.take();
        Ok(std::mem::take(&mut pending.bytes))
    }

    pub(crate) fn write_all_until(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<()> {
        if bytes.len() > MAX_BUFFER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "named pipe operation exceeds buffer limit",
            ));
        }
        let mut at = 0;
        while at < bytes.len() {
            let mut completed = self.transfer(bytes[at..].to_vec(), true, deadline)?;
            let transferred = completed.len();
            for byte in &mut completed {
                // SAFETY: exclusive valid byte pointer.
                unsafe { std::ptr::write_volatile(byte, 0) };
            }
            if transferred == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "named pipe wrote zero bytes",
                ));
            }
            at += transferred;
        }
        Ok(())
    }

    pub(crate) fn read_exact_until(&mut self, out: &mut [u8], deadline: Instant) -> io::Result<()> {
        if out.len() > MAX_BUFFER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "named pipe operation exceeds buffer limit",
            ));
        }
        let mut at = 0;
        while at < out.len() {
            let bytes = self.transfer(vec![0; out.len() - at], false, deadline)?;
            if bytes.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "named pipe closed during frame",
                ));
            }
            out[at..at + bytes.len()].copy_from_slice(&bytes);
            at += bytes.len();
        }
        Ok(())
    }
}
