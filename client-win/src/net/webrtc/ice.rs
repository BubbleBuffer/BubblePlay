//! Host and server-reflexive gathering on the same socket used by str0m.
//! Browser ICE-server limits come from turn.js; TURN is opt-in in the browser
//! and is outside this native step. Loopback qualification never resolves or
//! sends to a public STUN server, even when hello-ok omits ICE configuration.
use serde_json::Value;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const COOKIE: u32 = 0x2112a442;
pub fn stun_urls(value: &Value) -> Vec<String> {
    validated_stun_urls(value).unwrap_or_else(|| vec!["stun:stun.l.google.com:19302".into()])
}
fn validated_stun_urls(value: &Value) -> Option<Vec<String>> {
    let servers = value.as_array().filter(|a| !a.is_empty() && a.len() <= 4)?;
    let mut result = Vec::new();
    for server in servers {
        let object = server.as_object()?;
        let urls: Vec<&str> = match object.get("urls")? {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) if !a.is_empty() && a.len() <= 4 => {
                a.iter().map(Value::as_str).collect::<Option<_>>()?
            }
            _ => return None,
        };
        let has_turn = urls.iter().any(|uri| {
            uri.get(..5)
                .is_some_and(|p| p.eq_ignore_ascii_case("turn:"))
                || uri
                    .get(..6)
                    .is_some_and(|p| p.eq_ignore_ascii_case("turns:"))
        });
        let username = object.get("username");
        let credential = object.get("credential");
        let bounded = |v: Option<&Value>| {
            v.and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty() && s.encode_utf16().count() <= 512)
        };
        if has_turn && (!bounded(username) || !bounded(credential)) {
            return None;
        }
        if !has_turn && (username.is_some() || credential.is_some()) {
            return None;
        }
        for uri in urls {
            if uri.is_empty()
                || uri.encode_utf16().count() > 256
                || uri.chars().any(|c| c.is_whitespace() || c.is_control())
                || uri.contains('#')
            {
                return None;
            }
            let (scheme, rest) = uri.split_once(':')?;
            let scheme = scheme.to_ascii_lowercase();
            if !matches!(scheme.as_str(), "stun" | "turn" | "turns") {
                return None;
            }
            let (authority, query) = rest
                .split_once('?')
                .map_or((rest, None), |(a, q)| (a, Some(q)));
            if !valid_authority(authority) {
                return None;
            }
            if scheme == "stun" {
                if query.is_some() {
                    return None;
                }
                result.push(authority.to_string());
            } else if query.is_some_and(|q| !matches!(q, "transport=udp" | "transport=tcp")) {
                return None;
            }
        }
    }
    Some(result)
}
fn valid_authority(authority: &str) -> bool {
    if authority.contains('@') || authority.contains('/') {
        return false;
    }
    let port_ok = |s: &str| !s.is_empty() && s.len() <= 5 && s.bytes().all(|c| c.is_ascii_digit());
    if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, tail)) = rest.split_once(']') else {
            return false;
        };
        return !host.is_empty()
            && host
                .bytes()
                .all(|c| c.is_ascii_hexdigit() || matches!(c, b':' | b'.' | b'%'))
            && (tail.is_empty() || tail.strip_prefix(':').is_some_and(port_ok));
    }
    let (host, port) = authority
        .split_once(':')
        .map_or((authority, None), |(h, p)| (h, Some(p)));
    !host.is_empty()
        && host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'))
        && port.is_none_or(port_ok)
}
pub async fn resolve(server: &str, loopback_only: bool) -> Vec<SocketAddr> {
    // Refuse before DNS: even a hostname resolving to loopback is insufficient
    // for the no-real-network development fence, except literal localhost.
    let authority = server.trim_start_matches("stun:");
    let authority = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:3478")
    };
    if loopback_only
        && !authority
            .parse::<SocketAddr>()
            .is_ok_and(|a| a.ip().is_loopback())
        && !authority.starts_with("localhost:")
    {
        return Vec::new();
    }
    tokio::net::lookup_host(authority)
        .await
        .map(|iter| {
            iter.filter(|addr| addr.is_ipv4() && (!loopback_only || addr.ip().is_loopback()))
                .collect()
        })
        .unwrap_or_default()
}
pub fn binding_request(tid: [u8; 12]) -> [u8; 20] {
    let mut packet = [0; 20];
    packet[0..2].copy_from_slice(&1u16.to_be_bytes());
    packet[4..8].copy_from_slice(&COOKIE.to_be_bytes());
    packet[8..20].copy_from_slice(&tid);
    packet
}
pub fn mapped_response(bytes: &[u8], tid: &[u8; 12]) -> Option<SocketAddr> {
    if bytes.len() < 20
        || bytes[0..2] != [1, 1]
        || bytes[4..8] != COOKIE.to_be_bytes()
        || bytes[8..20] != *tid
    {
        return None;
    }
    let length = usize::from(u16::from_be_bytes(bytes[2..4].try_into().ok()?));
    if length % 4 != 0 || bytes.len() != 20 + length {
        return None;
    }
    let mut offset = 20;
    while offset + 4 <= bytes.len() {
        let kind = u16::from_be_bytes(bytes[offset..offset + 2].try_into().ok()?);
        let size = usize::from(u16::from_be_bytes(
            bytes[offset + 2..offset + 4].try_into().ok()?,
        ));
        offset += 4;
        let end = offset.checked_add(size)?;
        let value = bytes.get(offset..end)?;
        if kind == 0x0020 && value.len() >= 8 {
            let port = u16::from_be_bytes(value[2..4].try_into().ok()?) ^ 0x2112;
            let ip = match value[1] {
                1 if size == 8 => {
                    let mut octets: [u8; 4] = value[4..8].try_into().ok()?;
                    for (o, k) in octets.iter_mut().zip(COOKIE.to_be_bytes()) {
                        *o ^= k;
                    }
                    IpAddr::V4(Ipv4Addr::from(octets))
                }
                2 if size == 20 => {
                    let mut octets: [u8; 16] = value[4..20].try_into().ok()?;
                    let mut key = [0; 16];
                    key[..4].copy_from_slice(&COOKIE.to_be_bytes());
                    key[4..].copy_from_slice(tid);
                    for (o, k) in octets.iter_mut().zip(key) {
                        *o ^= k;
                    }
                    IpAddr::V6(Ipv6Addr::from(octets))
                }
                _ => return None,
            };
            return (port != 0).then_some(SocketAddr::new(ip, port));
        }
        offset = end + (4 - size % 4) % 4;
    }
    None
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_server_limits_and_default() {
        assert_eq!(
            stun_urls(&Value::Null),
            vec!["stun:stun.l.google.com:19302"]
        );
        assert_eq!(
            stun_urls(&serde_json::json!([{"urls":"stun:127.0.0.1:3478"}])),
            vec!["127.0.0.1:3478"]
        );
        assert_eq!(
            stun_urls(&serde_json::json!([{"urls":"stun:bad\nserver"}])),
            stun_urls(&Value::Null)
        );
    }
    #[test]
    fn browser_validation_rejects_whole_configuration_with_invalid_credentials_or_uri() {
        let fallback = stun_urls(&Value::Null);
        for server in [
            serde_json::json!({"urls":"stun:127.0.0.1:3478","username":"unexpected"}),
            serde_json::json!({"urls":"turn:127.0.0.1:3478"}),
            serde_json::json!({"urls":"turn:127.0.0.1:3478?transport=bad","username":"user","credential":"synthetic"}),
            serde_json::json!({"urls":"stun:127.0.0.1:3478?transport=udp"}),
            serde_json::json!({"urls":"stun:user@127.0.0.1:3478"}),
        ] {
            assert_eq!(
                stun_urls(&serde_json::json!([{"urls":"stun:127.0.0.1:1111"},server])),
                fallback
            );
        }
        assert_eq!(
            stun_urls(
                &serde_json::json!([{"urls":["TURN:127.0.0.1:3478?transport=udp","STUN:127.0.0.1:19302"],"username":"user","credential":"synthetic"}])
            ),
            vec!["127.0.0.1:19302"]
        );
    }
    #[test]
    fn mapping_requires_complete_response_and_matching_transaction() {
        let tid = [7; 12];
        let mut packet = binding_request(tid).to_vec();
        packet[0..2].copy_from_slice(&[1, 1]);
        packet[2..4].copy_from_slice(&12u16.to_be_bytes());
        packet.extend_from_slice(&[0, 32, 0, 8, 0, 1]);
        packet.extend_from_slice(&(1234u16 ^ 0x2112).to_be_bytes());
        for (o, k) in [127, 0, 0, 1].into_iter().zip(COOKIE.to_be_bytes()) {
            packet.push(o ^ k);
        }
        assert_eq!(
            mapped_response(&packet, &tid),
            Some("127.0.0.1:1234".parse().unwrap())
        );
        assert_eq!(mapped_response(&packet, &[8; 12]), None);
        packet.pop();
        assert_eq!(mapped_response(&packet, &tid), None);
    }
    #[tokio::test]
    async fn loopback_run_refuses_public_stun_before_dns() {
        assert!(resolve("stun.l.google.com:19302", true).await.is_empty());
        assert!(resolve("8.8.8.8:3478", true).await.is_empty());
        assert_eq!(resolve("127.0.0.1:3478", true).await.len(), 1);
    }
}
