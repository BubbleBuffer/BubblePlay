//! `lab` feature only (off by default): loopback guard for `serve`. `serve` refuses to start
//! unless every listen address is loopback (127.0.0.0/8 or ::1).

use std::net::SocketAddr;

/// `Err` names the first address that is not loopback. An empty list is refused too.
pub fn require_loopback(addresses: &[SocketAddr]) -> Result<(), String> {
    if addresses.is_empty() {
        return Err("lab: no listen address given".to_string());
    }
    for address in addresses {
        if !address.ip().is_loopback() {
            return Err(format!(
                "lab feature refuses to listen on {address}; bind 127.0.0.1 or ::1 only"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn lab_guard_accepts_ipv4_loopback_range() {
        assert!(require_loopback(&[addr("127.0.0.1:8080")]).is_ok());
        assert!(require_loopback(&[addr("127.8.9.10:1")]).is_ok());
    }

    #[test]
    fn lab_guard_accepts_ipv6_loopback() {
        assert!(require_loopback(&[addr("[::1]:8080")]).is_ok());
    }

    #[test]
    fn lab_guard_refuses_unspecified_and_lan_addresses() {
        let err = require_loopback(&[addr("0.0.0.0:8080")]).unwrap_err();
        assert!(err.contains("0.0.0.0:8080"), "{err}");
        assert!(require_loopback(&[addr("192.168.1.5:8080")]).is_err());
        assert!(require_loopback(&[addr("[::]:8080")]).is_err());
    }

    #[test]
    fn lab_guard_refuses_when_any_address_is_not_loopback() {
        let mixed = [addr("127.0.0.1:8080"), addr("10.0.0.1:8080")];
        let err = require_loopback(&mixed).unwrap_err();
        assert!(err.contains("10.0.0.1"), "{err}");
    }

    #[test]
    fn lab_guard_refuses_an_empty_list() {
        assert!(require_loopback(&[]).is_err());
    }
}
