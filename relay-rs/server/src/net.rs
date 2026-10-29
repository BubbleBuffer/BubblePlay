//! Client IP for rate limits and logs. Follows the Go relay's `ClientIP` (G8): the forwarded
//! header is read only when the direct peer is inside a trusted proxy range.

use std::net::{IpAddr, SocketAddr};

use axum::http::HeaderMap;
use ipnet::IpNet;

/// Most forwarded entries we walk. Longer chains are treated as hostile and ignored.
const MAX_FORWARDED_HOPS: usize = 16;

/// Returns the client address. With no trusted proxy in front of the peer, the peer address is
/// returned and `X-Forwarded-For` is ignored. Behind a trusted proxy, the chain is walked from
/// the right and the first address outside the trusted ranges wins, so a client cannot choose
/// its own address by prefixing the header.
pub fn client_ip(peer: SocketAddr, headers: &HeaderMap, trusted_proxy_cidrs: &[IpNet]) -> IpAddr {
    let peer_ip = peer.ip();
    if !trusted_proxy_cidrs.iter().any(|net| net.contains(&peer_ip)) {
        return peer_ip;
    }
    let raw = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let hops: Vec<&str> = raw.split(',').collect();
    if hops.len() > MAX_FORWARDED_HOPS {
        return peer_ip;
    }
    for hop in hops.iter().rev() {
        let Ok(candidate) = hop.trim().parse::<IpAddr>() else {
            return peer_ip;
        };
        if !trusted_proxy_cidrs
            .iter()
            .any(|net| net.contains(&candidate))
        {
            return candidate;
        }
    }
    peer_ip
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn peer(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    fn nets(list: &[&str]) -> Vec<IpNet> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    fn xff(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn without_trusted_proxies_the_peer_wins_and_xff_is_ignored() {
        let ip = client_ip(peer("203.0.113.9:4000"), &xff("198.51.100.7"), &[]);
        assert_eq!(ip, "203.0.113.9".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn untrusted_peer_ignores_xff_even_when_proxies_are_configured() {
        let trusted = nets(&["10.0.0.0/8"]);
        let ip = client_ip(peer("203.0.113.9:4000"), &xff("198.51.100.7"), &trusted);
        assert_eq!(ip, "203.0.113.9".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn trusted_v4_proxy_yields_the_forwarded_client() {
        let trusted = nets(&["10.0.0.0/8"]);
        let ip = client_ip(peer("10.1.2.3:4000"), &xff("198.51.100.7"), &trusted);
        assert_eq!(ip, "198.51.100.7".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn rightmost_untrusted_hop_beats_a_spoofed_prefix() {
        let trusted = nets(&["10.0.0.0/8"]);
        let ip = client_ip(
            peer("10.1.2.3:4000"),
            &xff("1.2.3.4, 198.51.100.7, 10.9.9.9"),
            &trusted,
        );
        assert_eq!(ip, "198.51.100.7".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn all_trusted_chain_falls_back_to_the_peer() {
        let trusted = nets(&["10.0.0.0/8"]);
        let ip = client_ip(peer("10.1.2.3:4000"), &xff("10.9.9.9, 10.8.8.8"), &trusted);
        assert_eq!(ip, "10.1.2.3".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn garbage_hop_falls_back_to_the_peer() {
        let trusted = nets(&["10.0.0.0/8"]);
        let ip = client_ip(peer("10.1.2.3:4000"), &xff("not-an-ip"), &trusted);
        assert_eq!(ip, "10.1.2.3".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn missing_header_behind_a_trusted_proxy_falls_back_to_the_peer() {
        let trusted = nets(&["10.0.0.0/8"]);
        let ip = client_ip(peer("10.1.2.3:4000"), &HeaderMap::new(), &trusted);
        assert_eq!(ip, "10.1.2.3".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn overlong_chain_is_ignored() {
        let trusted = nets(&["10.0.0.0/8"]);
        let long = vec!["198.51.100.7"; 17].join(", ");
        let ip = client_ip(peer("10.1.2.3:4000"), &xff(&long), &trusted);
        assert_eq!(ip, "10.1.2.3".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn trusted_v6_proxy_yields_the_forwarded_v6_client() {
        let trusted = nets(&["fd00::/8"]);
        let ip = client_ip(peer("[fd00::5]:4000"), &xff("2001:db8::7"), &trusted);
        assert_eq!(ip, "2001:db8::7".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn v4_peer_outside_a_v6_range_is_not_trusted() {
        let trusted = nets(&["fd00::/8"]);
        let ip = client_ip(peer("10.1.2.3:4000"), &xff("198.51.100.7"), &trusted);
        assert_eq!(ip, "10.1.2.3".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn v6_client_behind_a_v4_proxy_is_read_from_the_header() {
        let trusted = nets(&["10.0.0.0/8"]);
        let ip = client_ip(
            peer("10.1.2.3:4000"),
            &xff("2001:db8::7, 10.0.0.2"),
            &trusted,
        );
        assert_eq!(ip, "2001:db8::7".parse::<IpAddr>().unwrap());
    }
}
