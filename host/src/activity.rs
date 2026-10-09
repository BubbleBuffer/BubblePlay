//! Coalesced local notifications and socket readiness share one blocking wait.
//! Each Activity has one waiter; clones publish notifications from any thread.

use std::net::UdpSocket;
use std::os::windows::io::{AsRawSocket, RawSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

#[derive(Clone)]
pub struct Activity(Arc<Inner>);

struct Inner {
    receiver: UdpSocket,
    sender: UdpSocket,
    pending: AtomicBool,
}

impl Activity {
    pub fn new() -> Result<Self> {
        let receiver = UdpSocket::bind("127.0.0.1:0").context("bind activity socket")?;
        receiver.set_nonblocking(true)?;
        let sender = UdpSocket::bind("127.0.0.1:0")?;
        sender.connect(receiver.local_addr()?)?;
        sender.set_nonblocking(true)?;
        Ok(Self(Arc::new(Inner {
            receiver,
            sender,
            pending: AtomicBool::new(false),
        })))
    }

    /// Publish work before notifying. Repeated notifications need one datagram.
    pub fn notify(&self) {
        if !self.0.pending.swap(true, Ordering::AcqRel) && self.0.sender.send(&[1]).is_err() {
            self.0.pending.store(false, Ordering::Release);
        }
    }

    /// Wait until notified, any socket is ready, or the deadline expires.
    /// Socket entries pair a live handle with whether write readiness matters.
    /// Callers must service their channels/state after every return, before
    /// waiting again. Socket handles must remain open for the whole call.
    pub fn wait(&self, timeout: Duration, sockets: &[(RawSocket, bool)]) -> Result<()> {
        #[repr(C)]
        struct PollFd {
            fd: usize,
            events: i16,
            revents: i16,
        }
        #[link(name = "ws2_32")]
        extern "system" {
            fn WSAPoll(fds: *mut PollFd, count: u32, timeout: i32) -> i32;
            fn WSAGetLastError() -> i32;
        }
        const READ: i16 = 0x0100; // POLLRDNORM
        const WRITE: i16 = 0x0010; // POLLWRNORM
        let mut fds = Vec::with_capacity(sockets.len() + 1);
        fds.push(PollFd {
            fd: self.0.receiver.as_raw_socket() as usize,
            events: READ,
            revents: 0,
        });
        fds.extend(sockets.iter().map(|&(socket, writable)| PollFd {
            fd: socket as usize,
            events: READ | if writable { WRITE } else { 0 },
            revents: 0,
        }));
        // Round up so an unexpired sub-ms deadline does not spin.
        let timeout_ms = timeout.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32;
        // The C layout matches WSAPOLLFD and all handles outlive this call.
        if unsafe { WSAPoll(fds.as_mut_ptr(), fds.len() as u32, timeout_ms) } < 0 {
            return Err(std::io::Error::from_raw_os_error(unsafe {
                WSAGetLastError()
            }))
            .context("wait for socket activity");
        }
        let mut buffer = [0u8; 256];
        while self.0.receiver.recv(&mut buffer).is_ok() {}
        // Notifications racing with the drain are handled by the next state
        // read. Notifications after this store enqueue the next datagram.
        self.0.pending.store(false, Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn notification_interrupts_a_long_wait_and_can_be_reused() {
        let activity = Activity::new().unwrap();
        for _ in 0..2 {
            let notifier = activity.clone();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let thread = thread::spawn(move || {
                ready_tx.send(()).unwrap();
                thread::sleep(Duration::from_millis(20));
                for _ in 0..1_000 {
                    notifier.notify();
                }
            });
            ready_rx.recv().unwrap();
            let started = Instant::now();
            activity.wait(Duration::from_secs(5), &[]).unwrap();
            assert!(started.elapsed() < Duration::from_secs(4));
            thread.join().unwrap();
            activity.wait(Duration::ZERO, &[]).unwrap();
        }
    }

    #[test]
    fn socket_input_interrupts_a_long_wait() {
        let activity = Activity::new().unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let destination = socket.local_addr().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            ready_tx.send(()).unwrap();
            thread::sleep(Duration::from_millis(20));
            UdpSocket::bind("127.0.0.1:0")
                .unwrap()
                .send_to(&[1], destination)
                .unwrap();
        });
        ready_rx.recv().unwrap();
        let started = Instant::now();
        activity
            .wait(Duration::from_secs(5), &[(socket.as_raw_socket(), false)])
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(4));
        thread.join().unwrap();
    }

    #[test]
    fn idle_wait_reaches_its_deadline() {
        let activity = Activity::new().unwrap();
        let started = Instant::now();
        activity.wait(Duration::from_millis(20), &[]).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(15));
    }
}
