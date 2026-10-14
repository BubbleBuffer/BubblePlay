//! `--input-record PATH`: one JSON line per binary message the fake host
//! receives on `/ws`, flushed as it is written so a test can read the file
//! while the run is still going.
//!
//! The classification mirrors the decoder in `host/src/input.rs` (`decode`,
//! plus the held-input rules of `apply` and `release_all_report`) as a
//! portable copy, not shared code. A packet is `valid` when the host would
//! decode it. Only valid packets change the held state, and a malformed packet
//! is refused, as the host refuses it.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use serde_json::{json, Value};

const SCAN_FLAG_UP: u8 = 1;
const SCAN_FLAG_EXTENDED: u8 = 2;
const CLIPBOARD_FLAG_PASTE: u8 = 1;
const CLIPBOARD_CHUNK_BYTES: usize = 8 * 1024;
const CLIPBOARD_MAX_CHUNKS: u16 = 128;
const MAX_TEXT_BYTES: usize = 512;

/// A key the host believes is down, as `HeldKey` in `host/src/input.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HeldKey {
    Vk(u16),
    Scan { code: u8, extended: bool },
}

impl HeldKey {
    fn to_json(self) -> Value {
        match self {
            HeldKey::Vk(vk) => json!({ "vk": vk }),
            HeldKey::Scan { code, extended } => json!({ "scan": code, "extended": extended }),
        }
    }
}

/// What a valid packet does to the input the host is holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    None,
    Key {
        key: HeldKey,
        up: bool,
    },
    /// Buttons-at-position (9) carries the mask too; it replaces the held mask.
    Buttons(u8),
    ReleaseAll,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    /// `None` only for an empty message.
    pub packet_type: Option<u8>,
    pub kind: &'static str,
    pub valid: bool,
    /// Present only for types that carry a sequence and have its 2 bytes.
    pub seq: Option<u16>,
    pub effect: Effect,
}

/// Keys and buttons the host holds after the packets so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Held {
    pub keys: BTreeSet<HeldKey>,
    pub buttons: u8,
}

impl Held {
    pub fn apply(&mut self, effect: Effect) {
        match effect {
            Effect::None => {}
            Effect::Key { key, up: false } => {
                self.keys.insert(key);
            }
            Effect::Key { key, up: true } => {
                self.keys.remove(&key);
            }
            Effect::Buttons(mask) => self.buttons = mask,
            Effect::ReleaseAll => {
                self.keys.clear();
                self.buttons = 0;
            }
        }
    }

    pub fn to_json(&self) -> Value {
        let keys: Vec<Value> = self.keys.iter().map(|key| key.to_json()).collect();
        json!({ "keys": keys, "buttons": self.buttons })
    }
}

/// One binary message as the hub received it.
pub struct Received<'a> {
    pub epoch: Option<&'a str>,
    pub received_ms: f64,
    pub unix_ms: u128,
    pub bytes: &'a [u8],
}

pub struct Recorder {
    out: BufWriter<File>,
    held: Held,
    written: u64,
}

impl Recorder {
    /// Creates (truncates) the file. Failure here stops the run at startup.
    pub fn create(path: &Path) -> io::Result<Self> {
        Ok(Self {
            out: BufWriter::new(File::create(path)?),
            held: Held::default(),
            written: 0,
        })
    }

    /// Classifies the message, updates the held state if it is valid, and
    /// writes one flushed JSON line.
    pub fn record(&mut self, received: Received<'_>) -> io::Result<()> {
        let classified = classify(received.bytes);
        if classified.valid {
            self.held.apply(classified.effect);
        }
        let refused = if classified.valid {
            None
        } else {
            Some("malformed")
        };
        let mut line = json!({
            "index": self.written,
            "epoch": received.epoch,
            "received_ms": received.received_ms,
            "received_unix_ms": received.unix_ms,
            "len": received.bytes.len(),
            "bytes": received.bytes,
            "type": classified.packet_type,
            "kind": classified.kind,
            "valid": classified.valid,
            "refused": refused,
            "held": self.held.to_json(),
        });
        if let Some(seq) = classified.seq {
            line["seq"] = json!(seq);
        }
        if classified.valid {
            line["decoded"] = decoded_fields(received.bytes);
        }
        writeln!(self.out, "{line}")?;
        self.out.flush()?;
        self.written += 1;
        Ok(())
    }

    pub fn held(&self) -> &Held {
        &self.held
    }

    pub fn written(&self) -> u64 {
        self.written
    }
    /// Lifecycle evidence, never represented as an inbound type-6 packet.
    pub fn disconnect(
        &mut self,
        epoch: Option<&str>,
        reason: &str,
        received_ms: f64,
    ) -> io::Result<()> {
        let before = self.held.to_json();
        self.held = Held::default();
        let line = json!({"event":"disconnect","epoch":epoch,"reason":reason,"received_ms":received_ms,"held_before":before,"held_after":self.held.to_json()});
        writeln!(self.out, "{line}")?;
        self.out.flush()
    }
}

fn decoded_fields(data: &[u8]) -> Value {
    let signed = |at| i16::from_le_bytes([data[at], data[at + 1]]);
    match data[0] {
        0 | 1 => json!({"vk":u16_at(data,1),"up":data[0]==1}),
        2 => json!({"dx":signed(1),"dy":signed(3)}),
        3 => json!({"x":u16_at(data,1),"y":u16_at(data,3)}),
        4 => json!({"mask":data[1]}),
        5 => json!({"delta":signed(1)}),
        6 => json!({}),
        7 => {
            json!({"seq":u16_at(data,1),"dx":signed(3),"dy":signed(5),"wheel_x":signed(7),"wheel_y":signed(9)})
        }
        8 => json!({"seq":u16_at(data,1),"x":u16_at(data,3),"y":u16_at(data,5)}),
        9 => json!({"seq":u16_at(data,1),"x":u16_at(data,3),"y":u16_at(data,5),"mask":data[7]}),
        10 => json!({"seq":u16_at(data,1)}),
        11 => json!({"text":std::str::from_utf8(&data[1..]).ok()}),
        12 => {
            json!({"paste":data[1]&1!=0,"id":u16_at(data,2),"index":u16_at(data,4),"count":u16_at(data,6),"chunk":&data[8..]})
        }
        13 => {
            json!({"scan":u16_at(data,2),"vk":u16_at(data,4),"extended":data[1]&2!=0,"up":data[1]&1!=0})
        }
        _ => Value::Null,
    }
}

pub fn classify(data: &[u8]) -> Classified {
    let Some(&packet_type) = data.first() else {
        return Classified {
            packet_type: None,
            kind: "empty",
            valid: false,
            seq: None,
            effect: Effect::None,
        };
    };
    let seq = match packet_type {
        7..=10 if data.len() >= 3 => Some(u16_at(data, 1)),
        _ => None,
    };
    let (valid, effect) = decode(data);
    Classified {
        packet_type: Some(packet_type),
        kind: kind_name(packet_type),
        valid,
        seq,
        effect,
    }
}

fn kind_name(packet_type: u8) -> &'static str {
    match packet_type {
        0 => "key-down",
        1 => "key-up",
        2 => "mouse-rel",
        3 => "mouse-abs",
        4 => "buttons",
        5 => "wheel",
        6 => "release-all",
        7 => "motion-v2",
        8 => "absolute-v2",
        9 => "buttons-at",
        10 => "cursor-sync",
        11 => "text",
        12 => "clipboard",
        13 => "scan-key",
        128 => "cursor-state",
        129 => "cursor-kind",
        _ => "unknown",
    }
}

fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

/// Validity and effect, following the host's `decode` rules. Lengths are
/// checked before any byte is read, so short messages cannot panic here.
fn decode(data: &[u8]) -> (bool, Effect) {
    let len = data.len();
    match data[0] {
        0 | 1 if len == 3 => (
            true,
            Effect::Key {
                key: HeldKey::Vk(u16_at(data, 1)),
                up: data[0] == 1,
            },
        ),
        2 | 3 if len == 5 => (true, Effect::None),
        4 if len == 2 => (true, Effect::Buttons(data[1])),
        5 if len == 3 => (true, Effect::None),
        6 if len == 1 => (true, Effect::ReleaseAll),
        7 if len == 11 => (true, Effect::None),
        8 if len == 7 => (true, Effect::None),
        9 if len == 8 => (true, Effect::Buttons(data[7])),
        10 if len == 3 => (true, Effect::None),
        11 => {
            let text =
                (2..=MAX_TEXT_BYTES + 1).contains(&len) && std::str::from_utf8(&data[1..]).is_ok();
            (text, Effect::None)
        }
        12 => (clipboard_valid(data), Effect::None),
        13 if len == 6 => scan_key(data),
        _ => (false, Effect::None),
    }
}

fn clipboard_valid(data: &[u8]) -> bool {
    if !(8..=8 + CLIPBOARD_CHUNK_BYTES).contains(&data.len()) {
        return false;
    }
    let flags = data[1];
    let index = u16_at(data, 4);
    let count = u16_at(data, 6);
    flags & !CLIPBOARD_FLAG_PASTE == 0
        && count != 0
        && count <= CLIPBOARD_MAX_CHUNKS
        && index < count
}

fn scan_key(data: &[u8]) -> (bool, Effect) {
    let flags = data[1];
    let scan = u16_at(data, 2);
    let vk = u16_at(data, 4);
    let key = if flags & !(SCAN_FLAG_UP | SCAN_FLAG_EXTENDED) != 0 || scan > 0xFF {
        None
    } else {
        match (scan, vk) {
            (0, 0) => None,
            (0, vk) if vk <= 0xFF => Some(HeldKey::Vk(vk)),
            (0, _) => None,
            (code, _) => Some(HeldKey::Scan {
                code: code as u8,
                extended: flags & SCAN_FLAG_EXTENDED != 0,
            }),
        }
    };
    match key {
        Some(key) => (
            true,
            Effect::Key {
                key,
                up: flags & SCAN_FLAG_UP != 0,
            },
        ),
        None => (false, Effect::None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(data: &[u8]) -> Classified {
        let c = classify(data);
        assert!(c.valid, "expected valid: {data:?} -> {c:?}");
        c
    }

    fn invalid(data: &[u8]) {
        let c = classify(data);
        assert!(!c.valid, "expected refused: {data:?} -> {c:?}");
        assert_eq!(c.effect, Effect::None, "refused packets change nothing");
    }

    #[test]
    fn sequence_is_present_only_for_sequenced_types_with_bytes() {
        assert_eq!(
            valid(&[7, 0xE8, 0x03, 0, 0, 0, 0, 0, 0, 0, 0]).seq,
            Some(1000)
        );
        assert_eq!(valid(&[8, 1, 0, 0, 0, 0, 0]).seq, Some(1));
        assert_eq!(valid(&[9, 2, 0, 0, 0, 0, 0, 0]).seq, Some(2));
        assert_eq!(valid(&[10, 3, 0]).seq, Some(3));
        assert_eq!(valid(&[6]).seq, None);
        assert_eq!(valid(&[13, 0, 30, 0, 0, 0]).seq, None);
        assert_eq!(valid(&[4, 1]).seq, None);
        // A sequenced type too short to hold its sequence has none.
        assert_eq!(classify(&[7, 1]).seq, None);
        // Present but wrong length: refused, sequence still shown.
        let short = classify(&[7, 5, 0]);
        assert!(!short.valid);
        assert_eq!(short.seq, Some(5));
    }

    #[test]
    fn scan_keys_follow_the_host_rules() {
        let down = valid(&[13, 0, 29, 0, 0, 0]);
        assert_eq!(
            down.effect,
            Effect::Key {
                key: HeldKey::Scan {
                    code: 29,
                    extended: false
                },
                up: false
            }
        );
        let up_ext = valid(&[13, 3, 56, 0, 165, 0]);
        assert_eq!(
            up_ext.effect,
            Effect::Key {
                key: HeldKey::Scan {
                    code: 56,
                    extended: true
                },
                up: true
            }
        );
        assert_eq!(
            valid(&[13, 0, 0, 0, 19, 0]).effect,
            Effect::Key {
                key: HeldKey::Vk(19),
                up: false
            }
        );
        invalid(&[13, 4, 30, 0, 0, 0]);
        invalid(&[13, 0, 0, 1, 0, 0]);
        invalid(&[13, 0, 0, 0, 0, 0]);
        invalid(&[13, 0, 0, 0, 0x00, 0x01]);
        invalid(&[13, 0, 30, 0]);
    }

    #[test]
    fn clipboard_and_text_bounds() {
        valid(&[12, 1, 7, 0, 0, 0, 1, 0, 104, 195, 169]);
        invalid(&[12, 1, 0, 0, 1, 0, 1, 0]);
        invalid(&[12, 0, 0, 0, 0, 0, 0, 0]);
        invalid(&[12, 2, 0, 0, 0, 0, 1, 0, 65]);
        valid(&[11, b'h', b'i']);
        invalid(&[11]);
        invalid(&[11, 0xFF]);
    }

    #[test]
    fn buttons_and_release_all_have_held_effects() {
        assert_eq!(valid(&[4, 5]).effect, Effect::Buttons(5));
        assert_eq!(valid(&[9, 0, 0, 0, 0, 0, 0, 2]).effect, Effect::Buttons(2));
        assert_eq!(valid(&[6]).effect, Effect::ReleaseAll);
        invalid(&[6, 0]);
        invalid(&[4]);
    }

    #[test]
    fn host_to_browser_types_are_refused_inbound() {
        invalid(&[128, 0, 0, 0, 0, 0, 0, 0]);
        invalid(&[129, 1]);
        invalid(&[]);
    }

    #[test]
    fn held_state_follows_valid_packets_only() {
        let mut held = Held::default();
        held.apply(classify(&[0, 0x13, 0]).effect);
        held.apply(classify(&[0, 0x13, 0]).effect);
        held.apply(classify(&[4, 5]).effect);
        assert_eq!(held.keys.len(), 1);
        assert_eq!(held.buttons, 5);
        // Refused packets leave the held state alone.
        held.apply(classify(&[0, 0x14]).effect);
        assert_eq!(held.keys.len(), 1);
        held.apply(classify(&[1, 0x13, 0]).effect);
        assert!(held.keys.is_empty());
        held.apply(classify(&[13, 0, 29, 0, 0, 0]).effect);
        held.apply(classify(&[6]).effect);
        assert_eq!(held, Held::default());
    }
}
