//! `healthcheck`: a plain HTTP/1.1 GET of `/healthz` on the local listen address, for the Docker
//! HEALTHCHECK. It needs no curl in the image.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

use crate::config::load_config;

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_RESPONSE_BYTES: u64 = 4096;

/// The loopback address to dial for a listen address. An unspecified bind address is reached
/// through loopback of the same family.
pub fn healthz_address(listen: SocketAddr) -> SocketAddr {
    let ip = if listen.ip().is_unspecified() {
        match listen {
            SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        }
    } else {
        listen.ip()
    };
    SocketAddr::new(ip, listen.port())
}

/// Requests `/healthz` and accepts only `200` with the body `ok`.
pub fn probe(address: SocketAddr) -> Result<(), String> {
    probe_with_token(address, None)
}

fn probe_with_token(address: SocketAddr, token: Option<&str>) -> Result<(), String> {
    let mut stream = TcpStream::connect_timeout(&address, PROBE_TIMEOUT)
        .map_err(|_| format!("cannot connect to {address}"))?;
    stream
        .set_read_timeout(Some(PROBE_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(PROBE_TIMEOUT)))
        .map_err(|_| "cannot set probe timeouts".to_string())?;
    let authorization = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "GET /healthz HTTP/1.1\r\nHost: {address}\r\n{authorization}Connection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|_| "cannot send the probe request".to_string())?;
    let mut response = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES)
        .read_to_end(&mut response)
        .map_err(|_| "cannot read the probe response".to_string())?;
    let text = String::from_utf8_lossy(&response);
    let status = text.lines().next().unwrap_or("").split_whitespace().nth(1);
    let body = text.split("\r\n\r\n").nth(1);
    if status == Some("200") && body == Some("ok") {
        Ok(())
    } else {
        Err(format!(
            "healthz answered {}",
            status.unwrap_or("nothing usable")
        ))
    }
}

/// Loads the config and probes its listen address.
pub fn healthcheck(config_path: &Path) -> Result<(), String> {
    let cfg = load_config(config_path).map_err(|e| e.to_string())?;
    probe_with_token(healthz_address(cfg.listen), Some(cfg.admin_token.expose()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// Serves one request with the given status line and body, then returns.
    fn one_shot_server(
        status_line: &'static str,
        body: &'static str,
    ) -> (SocketAddr, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            let mut chunk = [0u8; 512];
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                seen.extend_from_slice(&chunk[..read]);
            }
            let reply = format!(
                "{status_line}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).unwrap();
        });
        (address, handle)
    }

    #[test]
    fn healthz_address_maps_unspecified_hosts_to_loopback() {
        let v4 = healthz_address("0.0.0.0:8080".parse().unwrap());
        assert_eq!(v4, "127.0.0.1:8080".parse::<SocketAddr>().unwrap());
        let v6 = healthz_address("[::]:8080".parse().unwrap());
        assert_eq!(v6, "[::1]:8080".parse::<SocketAddr>().unwrap());
        let kept = healthz_address("10.0.0.5:9000".parse().unwrap());
        assert_eq!(kept, "10.0.0.5:9000".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn healthz_probe_accepts_200_ok() {
        let (address, handle) = one_shot_server("HTTP/1.1 200 OK", "ok");
        assert!(probe(address).is_ok());
        handle.join().unwrap();
    }

    #[test]
    fn healthz_probe_rejects_404() {
        let (address, handle) = one_shot_server("HTTP/1.1 404 Not Found", "");
        assert!(probe(address).is_err());
        handle.join().unwrap();
    }

    #[test]
    fn healthz_probe_rejects_a_200_with_the_wrong_body() {
        let (address, handle) = one_shot_server("HTTP/1.1 200 OK", "nope");
        assert!(probe(address).is_err());
        handle.join().unwrap();
    }
}
