//! Minimal STUN Binding client (~80 lines, no crate).
//!
//! str0m has no STUN client, and the generic STUN builders add USERNAME /
//! MESSAGE-INTEGRITY which Google's public STUN server rejects. We hand-roll
//! a plain Binding Request (RFC 5389 §6), send it *from the UDP socket str0m
//! will use for media* (so the NAT pinhole matches the srflx candidate), and
//! parse XOR-MAPPED-ADDRESS (RFC 5389 §15.2).

use anyhow::{bail, Context, Result};
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

const STUN_MAGIC_COOKIE: u32 = 0x2112_A442;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// Discover the public (mapped) socket address for `socket` via the given STUN
/// server. Returns the XOR-mapped address.
pub fn discover_mapped_address(socket: &UdpSocket, server: &str) -> Result<SocketAddr> {
    let server_addr: SocketAddr = if let Ok(a) = server.parse() {
        a
    } else {
        // Resolve the first A record.
        let mut iter = std::net::ToSocketAddrs::to_socket_addrs(server)
            .context("resolve STUN server")?;
        iter.next().context("no STUN server address")?
    };

    // 20-byte header: type=Binding Request (0x0001), len=0, magic cookie, 96-bit tid.
    let mut tid = [0u8; 12];
    for b in tid.iter_mut() {
        *b = rand_byte();
    }
    let mut req = vec![0u8; 20];
    req[0..2].copy_from_slice(&0x0001u16.to_be_bytes()); // type
    req[2..4].copy_from_slice(&0u16.to_be_bytes()); // length
    req[4..8].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
    req[8..20].copy_from_slice(&tid);

    // Two attempts, short timeouts.
    let mut buf = [0u8; 576];
    for attempt in 1..=3 {
        socket
            .send_to(&req, server_addr)
            .context("stun send")?;
        socket
            .set_read_timeout(Some(Duration::from_millis(900)))
            .ok();
        match socket.recv_from(&mut buf) {
            Ok((n, from)) if from.ip() == server_addr.ip() => {
                if let Some(addr) = parse_binding_response(&buf[..n], &tid) {
                    return Ok(addr);
                }
                bail!("STUN response from {from} did not contain XOR-MAPPED-ADDRESS");
            }
            Ok((_, _)) => continue, // unexpected source, retry
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                if attempt == 3 {
                    bail!("STUN binding request timed out against {server}");
                }
            }
            Err(e) => return Err(e).context("stun recv"),
        }
    }
    bail!("STUN discovery failed against {server}");
}

/// Parse a Binding Success Response; extract XOR-MAPPED-ADDRESS.
fn parse_binding_response(buf: &[u8], tid: &[u8; 12]) -> Option<SocketAddr> {
    if buf.len() < 20 {
        return None;
    }
    let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
    let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let cookie = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if msg_type != 0x0101 || cookie != STUN_MAGIC_COOKIE || &buf[8..20] != tid {
        return None; // not a matching Binding Success Response
    }
    let total = (20 + len).min(buf.len());
    let mut off = 20;
    while off + 4 <= total {
        let attr_type = u16::from_be_bytes([buf[off], buf[off + 1]]);
        let attr_len = u16::from_be_bytes([buf[off + 2], buf[off + 3]]) as usize;
        let value_start = off + 4;
        if value_start + attr_len > total {
            break;
        }
        if attr_type == ATTR_XOR_MAPPED_ADDRESS && attr_len >= 8 {
            let family = buf[value_start + 1];
            let port_xor = u16::from_be_bytes([buf[value_start + 2], buf[value_start + 3]]);
            let port = port_xor ^ (STUN_MAGIC_COOKIE >> 16) as u16;
            let ip = match family {
                0x01 => {
                    // IPv4: xor with magic cookie
                    let mut ip_bytes = [0u8; 4];
                    ip_bytes.copy_from_slice(&buf[value_start + 4..value_start + 8]);
                    let cookie_bytes = STUN_MAGIC_COOKIE.to_be_bytes();
                    for i in 0..4 {
                        ip_bytes[i] ^= cookie_bytes[i];
                    }
                    std::net::IpAddr::V4(std::net::Ipv4Addr::from(ip_bytes))
                }
                0x02 if attr_len >= 20 => {
                    // IPv6: xor with magic cookie + transaction id
                    let mut ip_bytes = [0u8; 16];
                    ip_bytes.copy_from_slice(&buf[value_start + 4..value_start + 20]);
                    let mut key = [0u8; 16];
                    key[0..4].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
                    key[4..16].copy_from_slice(tid);
                    for i in 0..16 {
                        ip_bytes[i] ^= key[i];
                    }
                    std::net::IpAddr::V6(std::net::Ipv6Addr::from(ip_bytes))
                }
                _ => return None,
            };
            return Some(SocketAddr::new(ip, port));
        }
        off = value_start + attr_len;
        // attributes are padded to 4-byte boundary
        off += (4 - (off & 3)) & 3;
    }
    None
}

/// Tiny RNG for the 96-bit transaction id (avoids the rand crate for 12 bytes).
fn rand_byte() -> u8 {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs().wrapping_mul(0x100_0000))
        .unwrap_or(0)
        | 1;
    let mut x = COUNTER.fetch_add(seed, Ordering::Relaxed).wrapping_add(seed);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    (x & 0xFF) as u8
}
