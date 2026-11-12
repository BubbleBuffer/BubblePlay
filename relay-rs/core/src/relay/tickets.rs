//! Tickets and the `iceServers` list: the credentials a signalling socket hands out in hello-ok.

use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use super::Relay;
use super::state::{Conn, Role, Ticket, key_for};
use super::types::ConnId;
use super::util;
use crate::RelayRng;

impl Relay {
    /// Checks a ticket against the signalling peer of `issuer_role` for `pc`.
    /// Returns that peer's epoch when the ticket is good. A ticket is good while
    /// its issuer is open and before it expires (12 h).
    pub(super) fn valid_ticket(&self, issuer_role: Role, pc: &str, text: &str, now_ms: u64) -> Option<String> {
        util::decode_b64url(text, 32)?;
        let issuer = self.peers.get(&key_for(issuer_role, "", pc)).copied()?;
        let conn: Conn = self.open_conn(issuer)?;
        let Ticket { hash, expires_ms } = conn.ticket.clone()?;
        if now_ms >= expires_ms || !util::ct_eq(&sha256(text.as_bytes()), &hash) {
            return None;
        }
        Some(conn.epoch)
    }

    /// Mints a ticket for a signalling socket: 32 random bytes, base64url. Only
    /// the SHA-256 of the text is kept (Go hashes the text, server.go:572).
    pub(super) fn issue_ticket(&mut self, id: ConnId, now_ms: u64, rng: &mut dyn RelayRng) -> String {
        let raw: [u8; 32] = util::random(rng);
        let text = util::b64url(&raw);
        let hash = sha256(text.as_bytes());
        let ttl = self.config.timings.ticket_ttl_ms;
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.ticket = Some(Ticket {
                hash,
                expires_ms: now_ms + ttl,
            });
        }
        text
    }

    /// The `iceServers` list: the STUN server always, and TURN credentials when
    /// the viewer asked for them and TURN is configured (Go server.go:766-794).
    pub(super) fn ice_servers(&self, turn: bool, now_ms: u64, rng: &mut dyn RelayRng) -> Value {
        let mut servers = vec![json!({"urls": "stun:stun.l.google.com:19302"})];
        let Some(cfg) = self.config.turn.as_ref() else {
            return Value::Array(servers);
        };
        if !turn || cfg.secret.is_empty() {
            return Value::Array(servers);
        }
        let Some(authority) = util::turn_authority(&cfg.host) else {
            return Value::Array(servers);
        };
        // Go's randomCredential(12): 12 random bytes, unpadded base64url.
        let suffix: [u8; 12] = util::random(rng);
        let expiry = now_ms / 1000 + cfg.ttl_s;
        let username = format!("{expiry}:{}", util::b64url(&suffix));
        let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(cfg.secret.as_bytes()) else {
            return Value::Array(servers);
        };
        mac.update(username.as_bytes());
        let credential = util::b64_std(&mac.finalize().into_bytes());
        servers.push(json!({
            "urls": [
                format!("turn:{authority}?transport=udp"),
                format!("turn:{authority}?transport=tcp"),
            ],
            "username": username,
            "credential": credential,
        }));
        Value::Array(servers)
    }
}

pub(super) fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(bytes));
    out
}
